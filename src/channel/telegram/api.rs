//! Telegram Bot API wire types and low-level HTTP calls.
//!
//! This module owns Telegram protocol details. Higher-level routing, session
//! state, and transcript rendering should stay in sibling modules.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use reqwest::blocking::Client;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    parameters: Option<TelegramResponseParameters>,
}

#[derive(Debug, Deserialize)]
struct TelegramResponseParameters {
    retry_after: Option<u64>,
}

const TELEGRAM_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(300);
const TELEGRAM_RATE_LIMIT_SLACK: Duration = Duration::from_secs(1);
const TELEGRAM_MAX_RATE_LIMIT_RETRIES: usize = 2;

static TELEGRAM_NEXT_REQUEST_AT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TelegramRequestMode {
    Reliable,
    BestEffort,
}

impl<T> TelegramResponse<T> {
    fn description_or_unknown(&self) -> String {
        self.description
            .clone()
            .unwrap_or_else(|| "unknown error".to_string())
    }

    fn retry_after_seconds(&self) -> Option<u64> {
        self.parameters
            .as_ref()
            .and_then(|parameters| parameters.retry_after)
            .or_else(|| {
                self.description
                    .as_deref()
                    .and_then(telegram_retry_after_seconds_from_description)
            })
    }
}

fn telegram_rate_limit_state() -> &'static Mutex<Option<Instant>> {
    TELEGRAM_NEXT_REQUEST_AT.get_or_init(|| Mutex::new(None))
}

fn telegram_rate_limit_delay() -> Duration {
    let next_request_at = telegram_rate_limit_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    next_request_at
        .map(|next| next.saturating_duration_since(Instant::now()))
        .unwrap_or_default()
}

fn wait_for_telegram_rate_limit(method: &str, mode: TelegramRequestMode) -> Result<()> {
    loop {
        let sleep_for = telegram_rate_limit_delay();
        if sleep_for.is_zero() {
            return Ok(());
        }
        if mode == TelegramRequestMode::BestEffort {
            anyhow::bail!(
                "Telegram {method} skipped: global rate limit active for {}ms",
                sleep_for.as_millis()
            );
        }
        if sleep_for >= Duration::from_secs(1) {
            eprintln!(
                "telegram global rate limit waiting method={method} cooldown_ms={}",
                sleep_for.as_millis()
            );
        }
        thread::sleep(sleep_for);
    }
}

fn record_telegram_request_spacing() {
    let next_allowed = Instant::now() + TELEGRAM_MIN_REQUEST_INTERVAL;
    let mut next_request_at = telegram_rate_limit_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *next_request_at =
        Some(next_request_at.map_or(next_allowed, |existing| existing.max(next_allowed)));
}

fn record_telegram_retry_after(method: &str, seconds: u64) {
    let delay = Duration::from_secs(seconds).saturating_add(TELEGRAM_RATE_LIMIT_SLACK);
    let next_allowed = Instant::now() + delay;
    let mut next_request_at = telegram_rate_limit_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *next_request_at =
        Some(next_request_at.map_or(next_allowed, |existing| existing.max(next_allowed)));
    eprintln!(
        "telegram global rate limit method={method} retry_after_s={seconds} cooldown_ms={}",
        delay.as_millis()
    );
}

fn telegram_retry_after_seconds_from_description(description: &str) -> Option<u64> {
    let marker = "retry after ";
    let start = description.find(marker)? + marker.len();
    description[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .parse::<u64>()
        .ok()
}

fn post_form<T, P>(
    client: &Client,
    token: &str,
    method: &'static str,
    payload: &P,
) -> Result<TelegramResponse<T>>
where
    T: DeserializeOwned,
    P: Serialize + ?Sized,
{
    post_form_with_mode(
        client,
        token,
        method,
        payload,
        TelegramRequestMode::Reliable,
    )
}

fn post_form_best_effort<T, P>(
    client: &Client,
    token: &str,
    method: &'static str,
    payload: &P,
) -> Result<TelegramResponse<T>>
where
    T: DeserializeOwned,
    P: Serialize + ?Sized,
{
    post_form_with_mode(
        client,
        token,
        method,
        payload,
        TelegramRequestMode::BestEffort,
    )
}

fn post_form_with_mode<T, P>(
    client: &Client,
    token: &str,
    method: &'static str,
    payload: &P,
    mode: TelegramRequestMode,
) -> Result<TelegramResponse<T>>
where
    T: DeserializeOwned,
    P: Serialize + ?Sized,
{
    let url = telegram_method_url(token, method);
    for attempt in 0..=TELEGRAM_MAX_RATE_LIMIT_RETRIES {
        wait_for_telegram_rate_limit(method, mode)?;
        let response = client
            .post(url.as_str())
            .form(payload)
            .send()
            .map_err(|err| {
                anyhow::anyhow!("Telegram {method} request failed: {}", err.without_url())
            })?;
        let response = response.json::<TelegramResponse<T>>().map_err(|err| {
            anyhow::anyhow!(
                "decode Telegram {method} response failed: {}",
                err.without_url()
            )
        })?;
        record_telegram_request_spacing();
        if !response.ok {
            if let Some(seconds) = response.retry_after_seconds() {
                record_telegram_retry_after(method, seconds);
                if mode == TelegramRequestMode::Reliable
                    && attempt < TELEGRAM_MAX_RATE_LIMIT_RETRIES
                {
                    continue;
                }
            }
        }
        return Ok(response);
    }
    unreachable!("bounded Telegram retry loop always returns")
}

#[derive(Debug, Serialize)]
pub(super) struct TelegramBotCommand {
    pub(super) command: &'static str,
    description: &'static str,
}

#[derive(Debug, Deserialize)]
pub(super) struct TelegramUpdate {
    pub(super) update_id: i64,
    pub(super) message: Option<TelegramMessage>,
    pub(super) edited_message: Option<TelegramMessage>,
    pub(super) channel_post: Option<TelegramMessage>,
    pub(super) edited_channel_post: Option<TelegramMessage>,
    pub(super) callback_query: Option<TelegramCallbackQuery>,
    pub(super) my_chat_member: Option<TelegramChatMemberUpdated>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct TelegramMessage {
    pub(super) chat: TelegramChat,
    pub(super) message_id: i64,
    pub(super) date: Option<i64>,
    pub(super) message_thread_id: Option<i64>,
    pub(super) text: Option<String>,
    pub(super) forum_topic_edited: Option<TelegramForumTopicEdited>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct TelegramForumTopicEdited {
    #[serde(default)]
    pub(super) name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct TelegramChat {
    pub(super) id: i64,
    #[serde(default)]
    pub(super) is_forum: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct TelegramCallbackQuery {
    pub(super) id: String,
    pub(super) message: Option<TelegramMessage>,
    pub(super) data: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TelegramChatMemberUpdated {
    pub(super) chat: TelegramChat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TelegramUpdateSource {
    Message,
    EditedMessage,
    ChannelPost,
    EditedChannelPost,
    CallbackQuery,
    MyChatMember,
    Unknown,
}

#[derive(Debug)]
pub(super) struct TelegramUpdateView {
    pub(super) source: TelegramUpdateSource,
    pub(super) chat_id: Option<i64>,
    pub(super) message: Option<TelegramMessage>,
    pub(super) callback_query_id: Option<String>,
    pub(super) callback_data: Option<String>,
}

impl TelegramUpdate {
    pub(super) fn view(self) -> TelegramUpdateView {
        if let Some(message) = self.message {
            return TelegramUpdateView::message(TelegramUpdateSource::Message, message);
        }
        if let Some(message) = self.edited_message {
            return TelegramUpdateView::message(TelegramUpdateSource::EditedMessage, message);
        }
        if let Some(message) = self.channel_post {
            return TelegramUpdateView::message(TelegramUpdateSource::ChannelPost, message);
        }
        if let Some(message) = self.edited_channel_post {
            return TelegramUpdateView::message(TelegramUpdateSource::EditedChannelPost, message);
        }
        if let Some(callback_query) = self.callback_query {
            return match callback_query.message {
                Some(message) => TelegramUpdateView::callback_query(
                    message,
                    callback_query.id,
                    callback_query.data,
                ),
                None => TelegramUpdateView {
                    source: TelegramUpdateSource::CallbackQuery,
                    chat_id: None,
                    message: None,
                    callback_query_id: Some(callback_query.id),
                    callback_data: callback_query.data,
                },
            };
        }
        if let Some(my_chat_member) = self.my_chat_member {
            return TelegramUpdateView {
                source: TelegramUpdateSource::MyChatMember,
                chat_id: Some(my_chat_member.chat.id),
                message: None,
                callback_query_id: None,
                callback_data: None,
            };
        }
        TelegramUpdateView {
            source: TelegramUpdateSource::Unknown,
            chat_id: None,
            message: None,
            callback_query_id: None,
            callback_data: None,
        }
    }
}

impl TelegramUpdateView {
    fn message(source: TelegramUpdateSource, message: TelegramMessage) -> Self {
        Self {
            source,
            chat_id: Some(message.chat.id),
            message: Some(message),
            callback_query_id: None,
            callback_data: None,
        }
    }

    fn callback_query(
        message: TelegramMessage,
        callback_query_id: String,
        callback_data: Option<String>,
    ) -> Self {
        Self {
            source: TelegramUpdateSource::CallbackQuery,
            chat_id: Some(message.chat.id),
            message: Some(message),
            callback_query_id: Some(callback_query_id),
            callback_data,
        }
    }
}

impl TelegramUpdateSource {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::EditedMessage => "edited_message",
            Self::ChannelPost => "channel_post",
            Self::EditedChannelPost => "edited_channel_post",
            Self::CallbackQuery => "callback_query",
            Self::MyChatMember => "my_chat_member",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct TelegramInlineKeyboardMarkup {
    pub(super) inline_keyboard: Vec<Vec<TelegramInlineKeyboardButton>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct TelegramInlineKeyboardButton {
    pub(super) text: String,
    pub(super) callback_data: String,
}

pub(super) fn get_updates(
    client: &Client,
    token: &str,
    offset: Option<i64>,
    timeout: u64,
) -> Result<Vec<TelegramUpdate>> {
    let url = telegram_method_url(token, "getUpdates");
    let timeout_string = timeout.to_string();
    let mut request = client
        .get(url)
        .query(&[("timeout", timeout_string.as_str())]);
    let offset_string;
    if let Some(offset) = offset {
        offset_string = offset.to_string();
        request = request.query(&[("offset", offset_string.as_str())]);
    }
    let response = request.send().map_err(|err| {
        anyhow::anyhow!("Telegram getUpdates request failed: {}", err.without_url())
    })?;
    let response = response
        .json::<TelegramResponse<Vec<TelegramUpdate>>>()
        .map_err(|err| {
            anyhow::anyhow!(
                "decode Telegram getUpdates response failed: {}",
                err.without_url()
            )
        })?;
    if !response.ok {
        anyhow::bail!(
            "Telegram getUpdates failed: {}",
            response
                .description
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
    Ok(response.result.unwrap_or_default())
}

#[derive(Debug, Serialize)]
struct TelegramSendMessage<'a> {
    chat_id: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct TelegramSentMessage {
    message_id: i64,
}

pub(super) fn send_message(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_thread_id: Option<i64>,
    text: &str,
    reply_markup: Option<&TelegramInlineKeyboardMarkup>,
) -> Result<i64> {
    let chat_id = chat_id.to_string();
    let message_thread_id = message_thread_id.map(|thread_id| thread_id.to_string());
    let reply_markup = reply_markup
        .map(serde_json::to_string)
        .transpose()
        .context("serialize Telegram reply markup")?;
    let text = telegram_html_text(text);
    let payload = TelegramSendMessage {
        chat_id: chat_id.as_str(),
        text: text.as_str(),
        parse_mode: Some("HTML"),
        message_thread_id,
        reply_markup: reply_markup.as_deref(),
    };
    let response = post_form::<TelegramSentMessage, _>(client, token, "sendMessage", &payload)?;
    if !response.ok {
        anyhow::bail!(
            "Telegram sendMessage failed: {}",
            response.description_or_unknown()
        );
    }
    response
        .result
        .map(|message| message.message_id)
        .ok_or_else(|| anyhow::anyhow!("Telegram sendMessage: no message_id in response"))
}

#[derive(Debug, Serialize)]
struct TelegramEditMessageText<'a> {
    chat_id: &'a str,
    message_id: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<&'a str>,
}

pub(super) fn edit_message_text(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
    reply_markup: Option<&TelegramInlineKeyboardMarkup>,
) -> Result<()> {
    let chat_id = chat_id.to_string();
    let message_id = message_id.to_string();
    let reply_markup = reply_markup
        .map(serde_json::to_string)
        .transpose()
        .context("serialize Telegram reply markup")?;
    let text = telegram_html_text(text);
    let payload = TelegramEditMessageText {
        chat_id: chat_id.as_str(),
        message_id: message_id.as_str(),
        text: text.as_str(),
        parse_mode: Some("HTML"),
        reply_markup: reply_markup.as_deref(),
    };
    let response = post_form::<serde_json::Value, _>(client, token, "editMessageText", &payload)?;
    if !response.ok {
        if response
            .description
            .as_deref()
            .is_some_and(is_telegram_noop_error)
        {
            return Ok(());
        }
        anyhow::bail!(
            "Telegram editMessageText failed: {}",
            response.description_or_unknown()
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct TelegramDeleteMessage<'a> {
    chat_id: &'a str,
    message_id: &'a str,
}

pub(super) fn delete_message(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_id: i64,
) -> Result<()> {
    let chat_id = chat_id.to_string();
    let message_id = message_id.to_string();
    let payload = TelegramDeleteMessage {
        chat_id: chat_id.as_str(),
        message_id: message_id.as_str(),
    };
    let response = post_form::<bool, _>(client, token, "deleteMessage", &payload)?;
    if !response.ok {
        if response
            .description
            .as_deref()
            .is_some_and(is_telegram_delete_noop_error)
        {
            return Ok(());
        }
        anyhow::bail!(
            "Telegram deleteMessage failed: {}",
            response.description_or_unknown()
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct TelegramForumTopicParams<'a> {
    chat_id: &'a str,
    name: &'a str,
}

pub(super) fn create_forum_topic(
    client: &Client,
    token: &str,
    chat_id: i64,
    name: &str,
) -> Result<i64> {
    let chat_id = chat_id.to_string();
    let payload = TelegramForumTopicParams {
        chat_id: chat_id.as_str(),
        name,
    };
    let response = post_form::<serde_json::Value, _>(client, token, "createForumTopic", &payload)?;
    if !response.ok {
        anyhow::bail!(
            "Telegram createForumTopic failed: {}",
            response.description_or_unknown()
        );
    }
    let topic_id = response
        .result
        .and_then(|r| r.get("message_thread_id")?.as_i64())
        .ok_or_else(|| {
            anyhow::anyhow!("Telegram createForumTopic: no message_thread_id in response")
        })?;
    Ok(topic_id)
}

#[derive(Debug, Serialize)]
struct TelegramDeleteForumTopicParams<'a> {
    chat_id: &'a str,
    message_thread_id: &'a str,
}

pub(super) fn delete_forum_topic(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_thread_id: i64,
) -> Result<()> {
    let chat_id = chat_id.to_string();
    let message_thread_id = message_thread_id.to_string();
    let payload = TelegramDeleteForumTopicParams {
        chat_id: chat_id.as_str(),
        message_thread_id: message_thread_id.as_str(),
    };
    let response = post_form::<serde_json::Value, _>(client, token, "deleteForumTopic", &payload)?;
    if !response.ok {
        anyhow::bail!(
            "Telegram deleteForumTopic failed: {}",
            response.description_or_unknown()
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct TelegramEditGeneralForumTopicParams<'a> {
    chat_id: &'a str,
    name: &'a str,
}

pub(super) fn edit_general_forum_topic(
    client: &Client,
    token: &str,
    chat_id: i64,
    name: &str,
) -> Result<()> {
    let chat_id = chat_id.to_string();
    let payload = TelegramEditGeneralForumTopicParams {
        chat_id: chat_id.as_str(),
        name,
    };
    let response =
        post_form::<serde_json::Value, _>(client, token, "editGeneralForumTopic", &payload)?;
    if !response.ok {
        if response
            .description
            .as_deref()
            .is_some_and(is_telegram_noop_error)
        {
            return Ok(());
        }
        anyhow::bail!(
            "Telegram editGeneralForumTopic failed: {}",
            response.description_or_unknown()
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct TelegramGeneralForumTopicParams<'a> {
    chat_id: &'a str,
}

pub(super) fn unhide_general_forum_topic(client: &Client, token: &str, chat_id: i64) -> Result<()> {
    let chat_id = chat_id.to_string();
    let payload = TelegramGeneralForumTopicParams {
        chat_id: chat_id.as_str(),
    };
    let response =
        post_form::<serde_json::Value, _>(client, token, "unhideGeneralForumTopic", &payload)?;
    if !response.ok {
        if response
            .description
            .as_deref()
            .is_some_and(is_telegram_noop_error)
        {
            return Ok(());
        }
        anyhow::bail!(
            "Telegram unhideGeneralForumTopic failed: {}",
            response.description_or_unknown()
        );
    }
    Ok(())
}

pub(super) fn is_telegram_noop_error(description: &str) -> bool {
    description.contains("message is not modified") || description.contains("TOPIC_NOT_MODIFIED")
}

pub(super) fn is_telegram_delete_noop_error(description: &str) -> bool {
    description.contains("message to delete not found")
}

pub(super) fn is_telegram_missing_thread_error(err: &anyhow::Error) -> bool {
    let message = format!("{err:#}");
    message.contains("message thread not found") || message.contains("MESSAGE_THREAD_NOT_FOUND")
}

#[derive(Debug, Serialize)]
struct TelegramSendChatAction<'a> {
    chat_id: &'a str,
    action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<String>,
}

pub(super) fn send_chat_action(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_thread_id: Option<i64>,
) -> Result<()> {
    let chat_id = chat_id.to_string();
    let message_thread_id = message_thread_id.map(|thread_id| thread_id.to_string());
    let payload = TelegramSendChatAction {
        chat_id: chat_id.as_str(),
        action: "typing",
        message_thread_id,
    };
    let response =
        post_form_best_effort::<serde_json::Value, _>(client, token, "sendChatAction", &payload)?;
    if !response.ok {
        anyhow::bail!(
            "Telegram sendChatAction failed: {}",
            response.description_or_unknown()
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct TelegramReaction<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    emoji: &'a str,
}

#[derive(Debug, Serialize)]
struct TelegramSetMessageReaction<'a> {
    chat_id: &'a str,
    message_id: &'a str,
    reaction: &'a str,
}

pub(super) fn set_message_reaction(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_id: i64,
    emoji: &str,
) -> Result<()> {
    let chat_id = chat_id.to_string();
    let message_id = message_id.to_string();
    let reaction = serde_json::to_string(&[TelegramReaction {
        kind: "emoji",
        emoji,
    }])
    .context("serialize Telegram reaction")?;
    let payload = TelegramSetMessageReaction {
        chat_id: chat_id.as_str(),
        message_id: message_id.as_str(),
        reaction: reaction.as_str(),
    };
    let response = post_form_best_effort::<serde_json::Value, _>(
        client,
        token,
        "setMessageReaction",
        &payload,
    )?;
    if !response.ok {
        anyhow::bail!(
            "Telegram setMessageReaction failed: {}",
            response.description_or_unknown()
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct TelegramAnswerCallbackQuery<'a> {
    callback_query_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
}

pub(super) fn answer_callback_query(
    client: &Client,
    token: &str,
    callback_query_id: &str,
    text: Option<&str>,
) -> Result<()> {
    let payload = TelegramAnswerCallbackQuery {
        callback_query_id,
        text,
    };
    let response = post_form_best_effort::<serde_json::Value, _>(
        client,
        token,
        "answerCallbackQuery",
        &payload,
    )?;
    if !response.ok {
        anyhow::bail!(
            "Telegram answerCallbackQuery failed: {}",
            response.description_or_unknown()
        );
    }
    Ok(())
}

pub(super) fn sync_bot_commands(client: &Client, token: &str) -> Result<()> {
    let commands = serde_json::to_string(bot_commands()).context("serialize bot commands")?;
    let payload = [("commands", commands.as_str())];
    let response = post_form::<serde_json::Value, _>(client, token, "setMyCommands", &payload)?;
    if !response.ok {
        anyhow::bail!(
            "Telegram setMyCommands failed: {}",
            response.description_or_unknown()
        );
    }
    Ok(())
}

pub(super) fn bot_commands() -> &'static [TelegramBotCommand] {
    &[
        TelegramBotCommand {
            command: "start",
            description: "Bind this chat and open cx",
        },
        TelegramBotCommand {
            command: "bind",
            description: "Trust this chat with a one-time secret",
        },
        TelegramBotCommand {
            command: "portal",
            description: "Open the Codex handoff portal",
        },
        TelegramBotCommand {
            command: "status",
            description: "Show the current handoff status",
        },
    ]
}

pub(super) fn telegram_html_text(text: &str) -> String {
    let mut output = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        append_inline_markup_html(&rest[..start], &mut output);
        let after_open = &rest[start + 3..];
        let Some(end) = after_open.find("```") else {
            push_html_escaped(&mut output, "```");
            rest = after_open;
            continue;
        };
        let block = markdown_fence_body(&after_open[..end]);
        output.push_str("<pre>");
        push_html_escaped(&mut output, block);
        output.push_str("</pre>");
        rest = &after_open[end + 3..];
    }
    append_inline_markup_html(rest, &mut output);
    output
}

fn append_inline_markup_html(mut text: &str, output: &mut String) {
    while !text.is_empty() {
        let code_at = text.find('`');
        let bold_at = text.find("**");
        let strike_at = text.find("~~");
        let mut candidates = Vec::new();
        if let Some(index) = code_at {
            candidates.push((index, "`"));
        }
        if let Some(index) = bold_at {
            candidates.push((index, "**"));
        }
        if let Some(index) = strike_at {
            candidates.push((index, "~~"));
        }
        let Some((start, token)) = candidates.into_iter().min_by_key(|(index, _)| *index) else {
            push_html_escaped(output, text);
            return;
        };
        if start > text.len() {
            // This branch is unreachable for valid `find` results, but keeps
            // future token additions from indexing stale text.
            push_html_escaped(output, text);
            return;
        }
        if token.is_empty() {
            push_html_escaped(output, text);
            return;
        }
        if start == text.len() {
            return;
        }

        push_html_escaped(output, &text[..start]);
        match token {
            "`" => {
                let after_open = &text[start + 1..];
                let Some(end) = after_open.find('`') else {
                    push_html_escaped(output, &text[start..]);
                    return;
                };
                output.push_str("<code>");
                push_html_escaped(output, &after_open[..end]);
                output.push_str("</code>");
                text = &after_open[end + 1..];
            }
            "**" => {
                let after_open = &text[start + 2..];
                let Some(end) = after_open.find("**") else {
                    push_html_escaped(output, &text[start..]);
                    return;
                };
                output.push_str("<b>");
                push_html_escaped(output, &after_open[..end]);
                output.push_str("</b>");
                text = &after_open[end + 2..];
            }
            "~~" => {
                let after_open = &text[start + 2..];
                let Some(end) = after_open.find("~~") else {
                    push_html_escaped(output, &text[start..]);
                    return;
                };
                output.push_str("<s>");
                push_html_escaped(output, &after_open[..end]);
                output.push_str("</s>");
                text = &after_open[end + 2..];
            }
            _ => {
                push_html_escaped(output, text);
                return;
            }
        }
    }
}

fn markdown_fence_body(block: &str) -> &str {
    if let Some(rest) = block.strip_prefix('\n') {
        return rest;
    }
    let Some(newline) = block.find('\n') else {
        return block;
    };
    let language = block[..newline].trim();
    if !language.is_empty()
        && language.len() <= 32
        && language
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '+' | '#' | '.'))
    {
        &block[newline + 1..]
    } else {
        block
    }
}

fn push_html_escaped(output: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            _ => output.push(ch),
        }
    }
}

pub(super) fn telegram_text_chunks(text: &str) -> Vec<String> {
    const MAX_CHARS: usize = 3900;
    if text.chars().count() <= MAX_CHARS {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.chars().count() >= MAX_CHARS {
            chunks.push(current);
            current = String::new();
        }
        current.push(ch);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn telegram_method_url(token: &str, method: &str) -> String {
    format!("https://api.telegram.org/bot{token}/{method}")
}
