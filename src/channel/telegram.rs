use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::blocking::Client;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use crate::app_server::parse_server_event;
use crate::app_server::AppServerClient;
use crate::app_server::AppStreamEvent;
use crate::app_server::AppThreadSummary;
use crate::app_server::ApprovalRequest;
use crate::app_server::CommandActivity;
use crate::app_server::CommandExecution;
use crate::app_server::CommandExecutionStatus;
use crate::app_server::ParsedServerEvent;
use crate::cli::TelegramBindArgs;
use crate::cli::TelegramMenuArgs;
use crate::cli::TelegramRunArgs;
use crate::cli::TelegramStatusArgs;
use crate::paths::ManagerPaths;
use crate::serve;
use crate::session;
use crate::session::AcquireLeaseRequest;
use crate::session::ChannelId;
use crate::session::CreateSessionRequest;
use crate::session::RecordChannelMessageRequest;
use crate::session::SessionId;

mod transcript;

#[cfg(test)]
use self::transcript::activity_watch_text;
use self::transcript::info_watch_text;
#[cfg(test)]
use self::transcript::thinking_watch_text;
use self::transcript::user_watch_text;
use self::transcript::TelegramActivityPanel;
use self::transcript::TelegramActivityState;
use self::transcript::TelegramStatusPanel;
use self::transcript::TelegramStatusState;
use self::transcript::TelegramThinkingPanel;
use self::transcript::TelegramTranscriptTarget;

const TELEGRAM_STATE_SCHEMA_VERSION: u64 = 1;
const PORTAL_TOPIC_TITLE: &str = "cx portal";
const PORTAL_APP_SERVER_TIMEOUT: Duration = Duration::from_millis(750);
#[cfg(test)]
const ROLLOUT_OWNER_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const WATCH_DRAIN_MAX_LINES: usize = 1000;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct TelegramState {
    schema_version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_update_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    trusted_routes: Vec<TelegramRoute>,
    bindings: Vec<TelegramBinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    active_routes: Vec<TelegramActiveRoute>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct TelegramBinding {
    chat_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    channel_id: ChannelId,
    session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    app_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    app_thread_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    app_thread_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    topic_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    panel_message_id: Option<i64>,
    #[serde(default)]
    telegram_paused: bool,
    #[serde(default)]
    topic_created_by_adapter: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watch_proxy_offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watch_rollout_offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watch_activity: Option<TelegramActivityState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watch_status: Option<TelegramStatusState>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct TelegramActiveRoute {
    chat_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
struct TelegramRoute {
    chat_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Serialize)]
struct TelegramBotCommand {
    command: &'static str,
    description: &'static str,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
    edited_message: Option<TelegramMessage>,
    channel_post: Option<TelegramMessage>,
    edited_channel_post: Option<TelegramMessage>,
    callback_query: Option<TelegramCallbackQuery>,
    my_chat_member: Option<TelegramChatMemberUpdated>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramMessage {
    chat: TelegramChat,
    message_id: i64,
    date: Option<i64>,
    message_thread_id: Option<i64>,
    text: Option<String>,
    forum_topic_edited: Option<TelegramForumTopicEdited>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramForumTopicEdited {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramChat {
    id: i64,
    #[serde(default)]
    is_forum: bool,
}

#[derive(Debug, Deserialize)]
struct TelegramCallbackQuery {
    id: String,
    message: Option<TelegramMessage>,
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramChatMemberUpdated {
    chat: TelegramChat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TelegramUpdateSource {
    Message,
    EditedMessage,
    ChannelPost,
    EditedChannelPost,
    CallbackQuery,
    MyChatMember,
    Unknown,
}

#[derive(Debug)]
struct TelegramUpdateView {
    source: TelegramUpdateSource,
    chat_id: Option<i64>,
    message: Option<TelegramMessage>,
    callback_query_id: Option<String>,
    callback_data: Option<String>,
}

impl TelegramUpdate {
    fn view(self) -> TelegramUpdateView {
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
    fn as_str(self) -> &'static str {
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

impl TelegramState {
    fn empty() -> Self {
        Self {
            schema_version: TELEGRAM_STATE_SCHEMA_VERSION,
            last_update_id: None,
            trusted_routes: Vec::new(),
            bindings: Vec::new(),
            active_routes: Vec::new(),
        }
    }

    fn trust_route(&mut self, route: &TelegramRoute) {
        if !self.trusted_routes.contains(route) {
            self.trusted_routes.push(route.clone());
        }
    }

    fn binding_for_route(
        &self,
        route: &TelegramRoute,
        alias: Option<&str>,
    ) -> Option<&TelegramBinding> {
        self.bindings.iter().find(|binding| {
            binding.chat_id == route.chat_id
                && binding.message_thread_id == route.message_thread_id
                && binding.alias.as_deref() == alias
        })
    }

    fn active_binding_for_route(&self, route: &TelegramRoute) -> Option<&TelegramBinding> {
        let active_alias = self.active_alias_for_route(route);
        self.binding_for_route(route, active_alias)
            .or_else(|| self.binding_for_route(route, None))
    }

    fn active_alias_for_route(&self, route: &TelegramRoute) -> Option<&str> {
        self.active_routes
            .iter()
            .find(|active| {
                active.chat_id == route.chat_id
                    && active.message_thread_id == route.message_thread_id
            })
            .and_then(|active| active.alias.as_deref())
    }

    fn binding_for_route_mut(
        &mut self,
        route: &TelegramRoute,
        alias: Option<&str>,
    ) -> Option<&mut TelegramBinding> {
        self.bindings.iter_mut().find(|binding| {
            binding.chat_id == route.chat_id
                && binding.message_thread_id == route.message_thread_id
                && binding.alias.as_deref() == alias
        })
    }

    fn bind_route(
        &mut self,
        paths: &ManagerPaths,
        route: &TelegramRoute,
        alias: Option<&str>,
    ) -> Result<TelegramBinding> {
        if let Some(binding) = self.binding_for_route(route, alias) {
            return Ok(binding.clone());
        }
        let channel_id = route.channel_id(alias)?;
        let result = session::create_session(
            paths,
            CreateSessionRequest {
                session_id: None,
                channel_id: channel_id.clone(),
            },
        )?;
        let binding = TelegramBinding {
            chat_id: route.chat_id,
            message_thread_id: route.message_thread_id,
            alias: alias.map(str::to_string),
            channel_id,
            session_id: result.session.session_id,
            app_thread_id: None,
            app_thread_title: None,
            app_thread_cwd: None,
            topic_title: None,
            panel_message_id: None,
            telegram_paused: false,
            topic_created_by_adapter: false,
            watch_proxy_offset: None,
            watch_rollout_offset: None,
            watch_activity: None,
            watch_status: None,
        };
        self.bindings.push(binding.clone());
        self.set_active_route(route, alias);
        Ok(binding)
    }

    fn bind_app_thread(
        &mut self,
        paths: &ManagerPaths,
        route: &TelegramRoute,
        app_thread: &AppThreadPortalEntry,
        topic_created_by_adapter: bool,
    ) -> Result<TelegramBinding> {
        let mut binding = self.bind_route(paths, route, None)?;
        let existing_session_id = binding.session_id.clone();
        if let Some(stored) = self.binding_for_route_mut(route, None) {
            stored.app_thread_id = Some(app_thread.thread_id.clone());
            stored.app_thread_title = app_thread.title.clone();
            stored.app_thread_cwd = Some(app_thread.cwd.clone());
            stored.topic_title = Some(work_topic_title(app_thread));
            stored.panel_message_id = None;
            stored.telegram_paused = false;
            stored.topic_created_by_adapter = topic_created_by_adapter;
            stored.watch_proxy_offset = None;
            stored.watch_rollout_offset = None;
            stored.watch_activity = None;
            stored.watch_status = None;
            binding = stored.clone();
        }
        debug_assert_eq!(binding.session_id, existing_session_id);
        Ok(binding)
    }

    fn set_topic_title(&mut self, route: &TelegramRoute, title: &str) {
        for binding in self.bindings.iter_mut().filter(|binding| {
            binding.chat_id == route.chat_id && binding.message_thread_id == route.message_thread_id
        }) {
            binding.topic_title = Some(title.to_string());
        }
    }

    fn panel_message_id_for_route(&self, route: &TelegramRoute) -> Option<i64> {
        self.bindings
            .iter()
            .find(|binding| {
                binding.chat_id == route.chat_id
                    && binding.message_thread_id == route.message_thread_id
            })
            .and_then(|binding| binding.panel_message_id)
    }

    fn remember_panel_message(&mut self, route: &TelegramRoute, message_id: i64) {
        for binding in self.bindings.iter_mut().filter(|binding| {
            binding.chat_id == route.chat_id && binding.message_thread_id == route.message_thread_id
        }) {
            binding.panel_message_id = Some(message_id);
        }
    }

    fn set_active_route(&mut self, route: &TelegramRoute, alias: Option<&str>) {
        if let Some(active) = self.active_routes.iter_mut().find(|active| {
            active.chat_id == route.chat_id && active.message_thread_id == route.message_thread_id
        }) {
            active.alias = alias.map(str::to_string);
            return;
        }
        self.active_routes.push(TelegramActiveRoute {
            chat_id: route.chat_id,
            message_thread_id: route.message_thread_id,
            alias: alias.map(str::to_string),
        });
    }

    fn remove_route_binding(&mut self, route: &TelegramRoute, alias: Option<&str>) -> bool {
        let original_len = self.bindings.len();
        self.bindings.retain(|binding| {
            !(binding.chat_id == route.chat_id
                && binding.message_thread_id == route.message_thread_id
                && binding.alias.as_deref() == alias)
        });
        if original_len != self.bindings.len() {
            self.active_routes.retain(|active| {
                !(active.chat_id == route.chat_id
                    && active.message_thread_id == route.message_thread_id
                    && active.alias.as_deref() == alias)
            });
            return true;
        }
        false
    }

    fn remove_all_route_bindings(&mut self, route: &TelegramRoute) -> bool {
        let original_len = self.bindings.len();
        self.bindings.retain(|binding| {
            !(binding.chat_id == route.chat_id
                && binding.message_thread_id == route.message_thread_id)
        });
        if original_len != self.bindings.len() {
            self.active_routes.retain(|active| {
                !(active.chat_id == route.chat_id
                    && active.message_thread_id == route.message_thread_id)
            });
            return true;
        }
        false
    }
}

impl TelegramRoute {
    fn from_message(message: &TelegramMessage) -> Self {
        Self {
            chat_id: message.chat.id,
            message_thread_id: message.message_thread_id,
        }
    }

    fn channel_id(&self, alias: Option<&str>) -> Result<ChannelId> {
        let mut raw = format!("telegram:{}", self.chat_id);
        if let Some(thread_id) = self.message_thread_id {
            raw.push_str(&format!(":topic:{thread_id}"));
        }
        if let Some(alias) = alias {
            raw.push_str(":session:");
            raw.push_str(alias);
        }
        ChannelId::parse(raw)
    }

    fn display(&self) -> String {
        match self.message_thread_id {
            Some(thread_id) => format!("{} topic {}", self.chat_id, thread_id),
            None => self.chat_id.to_string(),
        }
    }
}

pub fn run(args: TelegramRunArgs) -> Result<()> {
    validate_timeouts(args.poll_timeout, args.request_timeout)?;
    if args.app_server_timeout <= 0.0 {
        anyhow::bail!("--app-server-timeout must be positive");
    }
    let token = read_token_env(&args.bot_token_env)?;
    let paths = ManagerPaths::new(args.manager_dir)?;
    let mut state = read_state(&paths)?;
    let mut trusted = trusted_chats(&state, args.allow_chats);
    let mut bind_secret = if trusted.is_empty() {
        let secret = generate_bind_secret()?;
        println!("cx telegram onboarding");
        println!("send this message to the Telegram bot:");
        println!("/bind {secret}");
        println!("waiting for first matching chat...");
        Some(secret)
    } else {
        None
    };
    let client = Client::builder()
        .timeout(Duration::from_secs_f32(args.request_timeout))
        .build()
        .context("build Telegram HTTP client")?;
    sync_bot_commands(&client, &token)?;
    refresh_startup_portals(&paths, &client, &token, &mut state, args.app_server_timeout)?;

    loop {
        let outcome = poll_once(
            &paths,
            &client,
            &token,
            &mut state,
            &mut trusted,
            PollOptions {
                bind_secret: bind_secret.as_deref(),
                poll_timeout: args.poll_timeout,
                log_updates: args.log_updates,
                acquire_lease: args.acquire_lease,
                steal: args.steal,
                app_server_timeout: args.app_server_timeout,
                trust_existing: true,
            },
        )?;
        if bind_secret.is_some() && outcome.bound_route.is_some() {
            println!("telegram route bound; continuing adapter run");
            bind_secret = None;
        }
    }
}

pub fn bind(args: TelegramBindArgs) -> Result<()> {
    validate_timeouts(args.poll_timeout, args.request_timeout)?;
    let token = read_token_env(&args.bot_token_env)?;
    let paths = ManagerPaths::new(args.manager_dir)?;
    let mut state = read_state(&paths)?;
    let mut trusted = trusted_chats(&state, Vec::new());
    let secret = generate_bind_secret()?;

    println!("cx telegram bind");
    println!("send this message to the Telegram chat you want to trust:");
    println!("/bind {secret}");
    println!("waiting for matching chat...");

    let client = Client::builder()
        .timeout(Duration::from_secs_f32(args.request_timeout))
        .build()
        .context("build Telegram HTTP client")?;
    sync_bot_commands(&client, &token)?;

    loop {
        let outcome = poll_once(
            &paths,
            &client,
            &token,
            &mut state,
            &mut trusted,
            PollOptions {
                bind_secret: Some(&secret),
                poll_timeout: args.poll_timeout,
                log_updates: args.log_updates,
                acquire_lease: false,
                steal: false,
                app_server_timeout: 0.0,
                trust_existing: false,
            },
        )?;
        if let Some(route) = outcome.bound_route {
            println!("trusted Telegram route {}", route.display());
            return Ok(());
        }
    }
}

pub fn menu(args: TelegramMenuArgs) -> Result<()> {
    if args.request_timeout <= 0.0 {
        anyhow::bail!("--request-timeout must be positive");
    }
    let token = read_token_env(&args.bot_token_env)?;
    let client = Client::builder()
        .timeout(Duration::from_secs_f32(args.request_timeout))
        .build()
        .context("build Telegram HTTP client")?;
    sync_bot_commands(&client, &token)?;
    println!("telegram command menu synced");
    Ok(())
}

pub fn status(args: TelegramStatusArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.manager_dir)?;
    let state = read_state(&paths)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&state)?);
        return Ok(());
    }
    println!("cx telegram channel");
    println!("state: {}", paths.telegram_channel_state_file().display());
    match state.last_update_id {
        Some(update_id) => println!("last_update_id: {update_id}"),
        None => println!("last_update_id: <none>"),
    }
    if state.bindings.is_empty() {
        println!("bindings: 0");
    } else {
        for binding in state.bindings {
            let app_thread = binding.app_thread_id.as_deref().unwrap_or("<none>");
            let handoff = if binding.telegram_paused {
                "paused"
            } else {
                "active"
            };
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                binding.chat_id,
                binding.message_thread_id.unwrap_or_default(),
                binding.alias.as_deref().unwrap_or("default"),
                binding.channel_id,
                binding.session_id,
                app_thread,
                handoff
            );
        }
    }
    if state.trusted_routes.is_empty() {
        println!("trusted_routes: 0");
    } else {
        for route in state.trusted_routes {
            println!("trusted\t{}", route.display());
        }
    }
    Ok(())
}

struct PollOutcome {
    bound_route: Option<TelegramRoute>,
}

struct PollOptions<'a> {
    bind_secret: Option<&'a str>,
    poll_timeout: u64,
    log_updates: bool,
    acquire_lease: bool,
    steal: bool,
    app_server_timeout: f32,
    trust_existing: bool,
}

fn poll_once(
    paths: &ManagerPaths,
    client: &Client,
    token: &str,
    state: &mut TelegramState,
    trusted: &mut BTreeSet<TelegramRoute>,
    options: PollOptions<'_>,
) -> Result<PollOutcome> {
    let mut bound_route = None;
    let poll_timeout = if options.app_server_timeout > 0.0 && has_watched_bindings(state) {
        options.poll_timeout.min(2)
    } else {
        options.poll_timeout
    };
    let offset = state.last_update_id.map(|id| id + 1);
    let poll_start = Instant::now();
    let updates = get_updates(client, token, offset, poll_timeout)?;
    if options.log_updates {
        eprintln!(
                "telegram timing phase=poll offset={offset:?} timeout_s={poll_timeout} updates={} elapsed_ms={}",
                updates.len(),
                elapsed_ms(poll_start)
            );
    }
    for update in updates {
        let update_start = Instant::now();
        state.last_update_id = Some(
            state
                .last_update_id
                .map_or(update.update_id, |id| id.max(update.update_id)),
        );
        let update_id = update.update_id;
        let view = update.view();
        if options.log_updates {
            log_update_summary(update_id, &view, trusted);
        }
        let Some(message) = view.message else {
            continue;
        };
        let access = message_access(
            &message,
            state,
            trusted,
            options.bind_secret,
            options.trust_existing,
        );
        if matches!(access, MessageAccess::Ignored) {
            continue;
        }
        if matches!(access, MessageAccess::AuthorizedByBind) {
            let route = trusted_route_for_message(&message);
            state.trust_route(&route);
            trusted.insert(route.clone());
            bound_route = Some(route);
        }
        if options.log_updates {
            let route = TelegramRoute::from_message(&message);
            eprintln!(
                    "telegram timing update_id={update_id} phase=received source={} route={} message_id={} telegram_age_ms={} elapsed_ms={}",
                    view.source.as_str(),
                    route.display(),
                    message.message_id,
                    telegram_age_ms(message.date).map_or_else(|| String::from("<unknown>"), |age| age.to_string()),
                    elapsed_ms(update_start)
                );
        }
        let callback_message_id = view.callback_query_id.as_ref().map(|_| message.message_id);
        let notifier = TelegramNotifier { client, token };
        let ack_start = Instant::now();
        if let Some(callback_query_id) = view.callback_query_id.as_deref() {
            notifier.answer_callback_query(callback_query_id, None);
        } else {
            notifier.ack_seen(&message);
        }
        if options.log_updates {
            eprintln!(
                "telegram timing update_id={update_id} phase=ack_seen elapsed_ms={}",
                elapsed_ms(ack_start)
            );
        }
        let handle_start = Instant::now();
        let reply = handle_message(
            paths,
            state,
            message,
            HandleOptions {
                notifier: Some(&notifier),
                acquire_lease: options.acquire_lease,
                steal: options.steal,
                authorized_by_bind: matches!(access, MessageAccess::AuthorizedByBind),
                app_server_timeout: options.app_server_timeout,
                trace_timings: options.log_updates,
                callback_data: view.callback_data.as_deref(),
                callback_message_id,
            },
        )?;
        if options.log_updates {
            eprintln!(
                "telegram timing update_id={update_id} phase=handle_message reply={} elapsed_ms={}",
                reply.is_some(),
                elapsed_ms(handle_start)
            );
        }
        write_state(paths, state)?;
        if let Some(reply) = reply {
            let route = reply.route();
            let deliver_start = Instant::now();
            match deliver_reply(client, token, &reply) {
                Ok(panel_message_id) => {
                    if options.log_updates {
                        eprintln!(
                                "telegram timing update_id={update_id} phase=deliver_reply route={} message_id={panel_message_id:?} elapsed_ms={}",
                                route.display(),
                                elapsed_ms(deliver_start)
                            );
                    }
                    if reply.remember_panel_message {
                        if let Some(message_id) = panel_message_id {
                            state.remember_panel_message(&route, message_id);
                            write_state(paths, state)?;
                        }
                    }
                }
                Err(err) => {
                    if options.log_updates {
                        eprintln!(
                                "telegram timing update_id={update_id} phase=deliver_reply route={} error=true elapsed_ms={}",
                                route.display(),
                                elapsed_ms(deliver_start)
                            );
                    }
                    eprintln!(
                        "telegram reply delivery failed for {}: {err:#}",
                        route.display()
                    );
                }
            }
        }
        if options.log_updates {
            eprintln!(
                "telegram timing update_id={update_id} phase=update_complete elapsed_ms={}",
                elapsed_ms(update_start)
            );
        }
    }
    if options.app_server_timeout > 0.0 {
        let observe_start = Instant::now();
        observe_watched_bindings(paths, client, token, state, options.app_server_timeout)?;
        if options.log_updates && has_watched_bindings(state) {
            eprintln!(
                "telegram timing phase=observe_watches elapsed_ms={}",
                elapsed_ms(observe_start)
            );
        }
    }
    write_state(paths, state)?;
    Ok(PollOutcome { bound_route })
}

fn has_watched_bindings(state: &TelegramState) -> bool {
    state
        .bindings
        .iter()
        .any(|binding| binding.telegram_paused && binding.app_thread_id.is_some())
}

fn observe_watched_bindings(
    paths: &ManagerPaths,
    client: &Client,
    token: &str,
    state: &mut TelegramState,
    _app_server_timeout: f32,
) -> Result<()> {
    let notifier = TelegramNotifier { client, token };
    for binding in watched_bindings(state) {
        let route = TelegramRoute {
            chat_id: binding.chat_id,
            message_thread_id: binding.message_thread_id,
        };
        match drain_watched_binding(paths, state, &route, &binding, &notifier) {
            Ok(()) => {}
            Err(err) if is_telegram_missing_thread_error(&err) => {
                eprintln!(
                    "telegram watch route no longer exists; removing {}: {err:#}",
                    route.display()
                );
                state.remove_route_binding(&route, binding.alias.as_deref());
            }
            Err(err) => {
                eprintln!(
                    "telegram watch observe failed for {}: {err:#}",
                    route.display()
                );
            }
        }
    }
    Ok(())
}

fn drain_watched_binding(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    binding: &TelegramBinding,
    notifier: &TelegramNotifier<'_>,
) -> Result<()> {
    let Some(thread_id) = binding.app_thread_id.as_deref() else {
        return Ok(());
    };
    let proxy_path = proxy_event_log_path(paths);
    if proxy_path.exists() {
        let proxy_len = fs::metadata(&proxy_path)
            .with_context(|| format!("stat proxy event log {}", proxy_path.display()))?
            .len();
        let proxy_offset = binding
            .watch_proxy_offset
            .filter(|offset| *offset <= proxy_len)
            .unwrap_or(proxy_len);
        let proxy_drain =
            proxy_events_since(&proxy_path, proxy_offset, WATCH_DRAIN_MAX_LINES, thread_id)?;
        let activity_state = state
            .binding_for_route(route, binding.alias.as_deref())
            .and_then(|stored| stored.watch_activity.clone())
            .or_else(|| binding.watch_activity.clone());
        let status_state = state
            .binding_for_route(route, binding.alias.as_deref())
            .and_then(|stored| stored.watch_status.clone())
            .or_else(|| binding.watch_status.clone());
        let proxy_had_events = !proxy_drain.events.is_empty();
        let send_result = send_watch_events(
            route,
            notifier,
            proxy_drain.events,
            activity_state,
            status_state,
        )?;
        if let Some(stored) = state.binding_for_route_mut(route, binding.alias.as_deref()) {
            stored.watch_proxy_offset = Some(proxy_drain.next_offset);
            stored.watch_activity = send_result.activity;
            stored.watch_status = send_result.status;
        }
        if send_result.sent_any && proxy_had_events {
            return Ok(());
        }
    }

    let Some(path) = rollout_path_for_thread(paths, thread_id) else {
        return Ok(());
    };
    if !path.exists() {
        return Ok(());
    }

    let file_len = fs::metadata(&path)
        .with_context(|| format!("stat rollout {}", path.display()))?
        .len();
    let missing_activity_state = binding.watch_activity.is_none();
    let active_turn_offset = latest_active_rollout_turn(&path)
        .ok()
        .flatten()
        .map(|(_, offset)| offset);
    let start_offset = match (binding.watch_rollout_offset, active_turn_offset) {
        (Some(offset), Some(active_offset)) if missing_activity_state && active_offset < offset => {
            active_offset
        }
        (Some(offset), _) if offset <= file_len => offset,
        (_, Some(active_offset)) => active_offset,
        _ => file_len,
    };
    let drain = rollout_events_since(&path, start_offset, WATCH_DRAIN_MAX_LINES)?;

    let activity_state = state
        .binding_for_route(route, binding.alias.as_deref())
        .and_then(|stored| stored.watch_activity.clone())
        .or_else(|| binding.watch_activity.clone());
    let status_state = state
        .binding_for_route(route, binding.alias.as_deref())
        .and_then(|stored| stored.watch_status.clone())
        .or_else(|| binding.watch_status.clone());
    let send_result =
        send_watch_events(route, notifier, drain.events, activity_state, status_state)?;
    if let Some(stored) = state.binding_for_route_mut(route, binding.alias.as_deref()) {
        stored.watch_rollout_offset = Some(drain.next_offset);
        stored.watch_activity = send_result.activity;
        stored.watch_status = send_result.status;
    }
    Ok(())
}

struct WatchSendResult {
    sent_any: bool,
    activity: Option<TelegramActivityState>,
    status: Option<TelegramStatusState>,
}

fn send_watch_events(
    route: &TelegramRoute,
    notifier: &TelegramNotifier<'_>,
    events: Vec<RolloutObserveEvent>,
    activity: Option<TelegramActivityState>,
    status: Option<TelegramStatusState>,
) -> Result<WatchSendResult> {
    let mut sink = TelegramWatchSink::new_best_effort(notifier, route.clone(), activity, status);
    let mut last_sent_agent_message = None::<String>;
    for event in events {
        match event {
            RolloutObserveEvent::Stream(AppStreamEvent::AgentDelta(message)) => {
                send_rollout_agent_message(&message, &mut last_sent_agent_message, &mut |event| {
                    sink.push_event(event)
                })?;
            }
            RolloutObserveEvent::Stream(event) => sink.push_event(event)?,
            RolloutObserveEvent::Terminal {
                last_agent_message, ..
            } => {
                sink.turn_completed()?;
                if let Some(message) = last_agent_message {
                    send_rollout_agent_message(
                        &message,
                        &mut last_sent_agent_message,
                        &mut |event| sink.push_event(event),
                    )?;
                }
            }
        }
    }
    sink.flush_pending()?;
    Ok(WatchSendResult {
        sent_any: sink.sent_any(),
        activity: sink.activity_state(),
        status: sink.status_state(),
    })
}

fn watched_bindings(state: &TelegramState) -> Vec<TelegramBinding> {
    let mut seen = BTreeSet::<(i64, Option<i64>, String)>::new();
    state
        .bindings
        .iter()
        .filter(|binding| binding.telegram_paused)
        .filter_map(|binding| {
            let thread_id = binding.app_thread_id.clone()?;
            let key = (binding.chat_id, binding.message_thread_id, thread_id);
            seen.insert(key).then(|| binding.clone())
        })
        .collect()
}

fn validate_timeouts(poll_timeout: u64, request_timeout: f32) -> Result<()> {
    if poll_timeout == 0 {
        anyhow::bail!("--poll-timeout must be positive");
    }
    if request_timeout <= 0.0 {
        anyhow::bail!("--request-timeout must be positive");
    }
    Ok(())
}

fn refresh_startup_portals(
    paths: &ManagerPaths,
    client: &Client,
    token: &str,
    state: &mut TelegramState,
    app_server_timeout: f32,
) -> Result<()> {
    let mut routes = state
        .bindings
        .iter()
        .filter(|binding| binding.message_thread_id.is_none())
        .map(|binding| TelegramRoute {
            chat_id: binding.chat_id,
            message_thread_id: None,
        })
        .collect::<Vec<_>>();
    routes.sort();
    routes.dedup();

    for route in routes {
        if route.chat_id < 0 {
            if let Err(err) = unhide_general_forum_topic(client, token, route.chat_id) {
                eprintln!(
                    "telegram portal startup unhideGeneralForumTopic failed for {}: {err:#}",
                    route.display()
                );
            }
            if let Err(err) =
                edit_general_forum_topic(client, token, route.chat_id, PORTAL_TOPIC_TITLE)
            {
                eprintln!(
                    "telegram portal startup editGeneralForumTopic failed for {}: {err:#}",
                    route.display()
                );
            }
        }

        let edit_message_id = state.panel_message_id_for_route(&route);
        let reply = portal_reply(paths, &route, app_server_timeout, edit_message_id, false)
            .unwrap_or_else(|err| portal_unavailable_reply(&route, err, edit_message_id));
        match deliver_reply(client, token, &reply) {
            Ok(Some(message_id)) => {
                state.remember_panel_message(&route, message_id);
            }
            Ok(None) => {}
            Err(err) if reply.edit_message_id.is_some() => {
                eprintln!(
                    "telegram portal startup edit panel failed for {}: {err:#}",
                    route.display()
                );
                let fresh_reply = portal_reply(paths, &route, app_server_timeout, None, false)
                    .unwrap_or_else(|err| portal_unavailable_reply(&route, err, None));
                match deliver_reply(client, token, &fresh_reply) {
                    Ok(Some(message_id)) => {
                        state.remember_panel_message(&route, message_id);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        eprintln!(
                            "telegram portal startup fresh panel failed for {}: {err:#}",
                            route.display()
                        );
                    }
                }
            }
            Err(err) => {
                eprintln!(
                    "telegram portal startup send panel failed for {}: {err:#}",
                    route.display()
                );
            }
        }
    }
    write_state(paths, state)?;
    Ok(())
}

fn read_token_env(env_name: &str) -> Result<String> {
    let token = std::env::var(env_name).with_context(|| format!("{env_name} is not set"))?;
    if token.trim().is_empty() {
        anyhow::bail!("{env_name} is empty");
    }
    Ok(token)
}

fn trusted_chats(state: &TelegramState, allow_chats: Vec<i64>) -> BTreeSet<TelegramRoute> {
    state
        .trusted_routes
        .iter()
        .cloned()
        .chain(state.bindings.iter().map(|binding| TelegramRoute {
            chat_id: binding.chat_id,
            message_thread_id: binding.message_thread_id,
        }))
        .chain(allow_chats.into_iter().map(|chat_id| TelegramRoute {
            chat_id,
            message_thread_id: None,
        }))
        .collect()
}

fn trusted_route_for_message(message: &TelegramMessage) -> TelegramRoute {
    let route = TelegramRoute::from_message(message);
    if message.chat.is_forum {
        TelegramRoute {
            chat_id: route.chat_id,
            message_thread_id: None,
        }
    } else {
        route
    }
}

fn generate_bind_secret() -> Result<String> {
    let mut bytes = [0_u8; 16];
    let mut file = fs::File::open("/dev/urandom").context("open /dev/urandom")?;
    file.read_exact(&mut bytes).context("read /dev/urandom")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TelegramReply {
    chat_id: i64,
    message_thread_id: Option<i64>,
    text: String,
    reply_markup: Option<TelegramInlineKeyboardMarkup>,
    edit_message_id: Option<i64>,
    remember_panel_message: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct TelegramInlineKeyboardMarkup {
    inline_keyboard: Vec<Vec<TelegramInlineKeyboardButton>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct TelegramInlineKeyboardButton {
    text: String,
    callback_data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppThreadPortalEntry {
    thread_id: String,
    title: Option<String>,
    preview: String,
    cwd: String,
    active: bool,
    watchable: bool,
    status: String,
}

impl From<AppThreadSummary> for AppThreadPortalEntry {
    fn from(summary: AppThreadSummary) -> Self {
        Self {
            thread_id: summary.upstream_thread_id,
            title: summary.title,
            preview: summary.preview,
            cwd: summary.cwd,
            active: summary.active,
            watchable: true,
            status: summary.status,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TelegramCallbackCommand {
    RefreshPortal,
    RefreshWork,
    Observe { thread_id: String },
    Takeover,
    Release,
    Close,
}

impl TelegramCallbackCommand {
    fn parse(data: &str) -> Option<Self> {
        match data {
            "cx:p" => Some(Self::RefreshPortal),
            "cx:w" => Some(Self::RefreshWork),
            "cx:t" => Some(Self::Takeover),
            "cx:rel" => Some(Self::Release),
            "cx:c" => Some(Self::Close),
            _ => {
                if let Some(thread_id) = data.strip_prefix("cx:o:") {
                    if thread_id.is_empty() {
                        return None;
                    }
                    return Some(Self::Observe {
                        thread_id: thread_id.to_string(),
                    });
                }
                let thread_id = data.strip_prefix("cx:a:")?;
                if thread_id.is_empty() {
                    return None;
                }
                Some(Self::Observe {
                    thread_id: thread_id.to_string(),
                })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TelegramApprovalDecision {
    AcceptOnce,
    AcceptForSession,
    Decline,
}

impl TelegramApprovalDecision {
    fn label(self) -> &'static str {
        match self {
            Self::AcceptOnce => "Approved once",
            Self::AcceptForSession => "Approved for session",
            Self::Decline => "Denied",
        }
    }
}

fn approval_callback_data(nonce: &str, decision: TelegramApprovalDecision) -> String {
    let suffix = match decision {
        TelegramApprovalDecision::AcceptOnce => "a",
        TelegramApprovalDecision::AcceptForSession => "s",
        TelegramApprovalDecision::Decline => "d",
    };
    format!("cx:ap:{nonce}:{suffix}")
}

fn parse_approval_callback_data(
    data: &str,
    expected_nonce: &str,
) -> Option<TelegramApprovalDecision> {
    let rest = data.strip_prefix("cx:ap:")?;
    let (nonce, suffix) = rest.rsplit_once(':')?;
    if nonce != expected_nonce {
        return None;
    }
    match suffix {
        "a" => Some(TelegramApprovalDecision::AcceptOnce),
        "s" => Some(TelegramApprovalDecision::AcceptForSession),
        "d" => Some(TelegramApprovalDecision::Decline),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageAccess {
    Allowed,
    AuthorizedByBind,
    DeniedBind,
    Ignored,
}

fn message_access(
    message: &TelegramMessage,
    state: &TelegramState,
    allowed: &BTreeSet<TelegramRoute>,
    bind_secret: Option<&str>,
    trust_existing: bool,
) -> MessageAccess {
    let route = TelegramRoute::from_message(message);
    if trust_existing && (route_is_trusted(&route, allowed) || route_has_binding(&route, state)) {
        return MessageAccess::Allowed;
    }

    let text = message.text.as_deref().unwrap_or_default();
    match TelegramTextCommand::parse(text) {
        TelegramTextCommand::Bind { secret } => {
            if bind_secret.is_some_and(|expected| secret.as_deref() == Some(expected)) {
                MessageAccess::AuthorizedByBind
            } else {
                MessageAccess::DeniedBind
            }
        }
        _ => MessageAccess::Ignored,
    }
}

fn route_is_trusted(route: &TelegramRoute, allowed: &BTreeSet<TelegramRoute>) -> bool {
    allowed.contains(route)
        || route.message_thread_id.is_some_and(|_| {
            allowed.contains(&TelegramRoute {
                chat_id: route.chat_id,
                message_thread_id: None,
            })
        })
}

fn route_has_binding(route: &TelegramRoute, state: &TelegramState) -> bool {
    state.active_binding_for_route(route).is_some()
        || route.message_thread_id.is_some_and(|_| {
            state.bindings.iter().any(|binding| {
                binding.chat_id == route.chat_id && binding.message_thread_id.is_none()
            })
        })
}

fn handle_message(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    message: TelegramMessage,
    options: HandleOptions<'_>,
) -> Result<Option<TelegramReply>> {
    let route = TelegramRoute::from_message(&message);
    let chat_id = route.chat_id;
    let chat_is_forum = message.chat.is_forum;
    if let Some(edit) = message.forum_topic_edited.as_ref() {
        if let Some(name) = edit.name.as_deref() {
            state.set_topic_title(&route, name);
        }
        return Ok(None);
    }
    let text = message.text.clone().unwrap_or_default();
    let command = TelegramTextCommand::parse(&text);

    if let Some(callback_data) = options.callback_data {
        return handle_callback(paths, state, &route, chat_is_forum, callback_data, &options);
    }
    if text.trim().is_empty() {
        return Ok(None);
    }

    match command {
        TelegramTextCommand::Portal => Ok(Some(
            portal_reply(
                paths,
                &route,
                options.app_server_timeout,
                None,
                options.trace_timings,
            )
            .unwrap_or_else(|err| portal_unavailable_reply(&route, err, None)),
        )),
        TelegramTextCommand::Attach { thread_id } => {
            let Some(thread_id) = thread_id.as_deref() else {
                return Ok(Some(reply(&route, "Usage: /attach <thread-id>")));
            };
            attach_app_thread(paths, state, &route, chat_is_forum, thread_id, &options)
                .or_else(|err| Ok(Some(reply(&route, portal_unavailable_message(err)))))
        }
        TelegramTextCommand::Takeover => takeover_handoff(paths, state, &route, &options, None),
        TelegramTextCommand::Status => {
            let Some(binding) = state.active_binding_for_route(&route) else {
                return Ok(Some(reply(&route, no_session_bound_message(state, &route))));
            };
            let session = session::show_session(paths, &binding.session_id)?;
            Ok(Some(reply(
                    &route,
                    format!(
                        "session: {}\nalias: {}\nroute: {}\ntopic_title: {}\nchannel: {}\ntelegram: {}\napp_thread: {}\ncwd: {}\nlease_epoch: {}\nactive_lease: {}",
                        session.session_id,
                        binding.alias.as_deref().unwrap_or("default"),
                        route.display(),
                        binding.topic_title.as_deref().unwrap_or("<unknown>"),
                        session.current_channel_id,
                        if binding.telegram_paused {
                            "paused"
                        } else {
                            "active"
                        },
                        binding.app_thread_id.as_deref().unwrap_or("<none>"),
                        binding.app_thread_cwd.as_deref().unwrap_or("<unknown>"),
                        session.lease_epoch,
                        session
                            .active_lease
                            .as_ref()
                            .map(|lease| lease.channel_id.to_string())
                            .unwrap_or_else(|| String::from("<none>"))
                    ),
                )))
        }
        TelegramTextCommand::Sessions => {
            let lines = route_session_lines(state, chat_id);
            if lines.is_empty() {
                return Ok(Some(reply(
                    &route,
                    "No Telegram sessions are bound for this chat. Send /start to create one.",
                )));
            }
            Ok(Some(reply(&route, lines.join("\n"))))
        }
        TelegramTextCommand::Release => release_handoff(paths, state, &route, None),
        TelegramTextCommand::Bind { .. } if !options.authorized_by_bind => {
            Ok(Some(reply(&route, "Invalid or disabled bind secret.")))
        }
        TelegramTextCommand::Bind { .. } => Ok(Some(reply(
            &route,
            format!(
                "Trusted Telegram route {}.",
                trusted_route_for_message(&message).display()
            ),
        ))),
        TelegramTextCommand::Start => {
            let binding = state.bind_route(paths, &route, None)?;
            session::record_channel_message(
                paths,
                RecordChannelMessageRequest {
                    session_id: binding.session_id.clone(),
                    channel_id: binding.channel_id.clone(),
                },
            )?;
            if options.acquire_lease {
                match session::acquire_lease(
                    paths,
                    AcquireLeaseRequest {
                        session_id: binding.session_id.clone(),
                        channel_id: binding.channel_id.clone(),
                        steal: options.steal,
                    },
                ) {
                    Ok(_) => {}
                    Err(err) => {
                        return Ok(Some(reply(
                                &route,
                                format!(
                                    "Session is controlled elsewhere. Use --steal on the adapter if this Telegram channel should take over.\n{err:#}"
                                ),
                            )));
                    }
                }
            }
            Ok(Some(reply(
                &route,
                format!(
                    "Bound route {} to cx session {}.",
                    route.display(),
                    binding.session_id
                ),
            )))
        }
        TelegramTextCommand::New { alias } => {
            let alias = normalize_alias(alias.as_deref())
                .unwrap_or_else(|| next_route_alias(state, &route));
            if state
                .bindings
                .iter()
                .any(|b| b.chat_id == route.chat_id && b.alias.as_deref() == Some(&alias))
            {
                return Ok(Some(reply(
                    &route,
                    format!("Session `{alias}` already exists. Use /use {alias} to switch to it."),
                )));
            }
            let effective_route = if route.message_thread_id.is_none() && chat_is_forum {
                if let Some(notifier) = options.notifier {
                    match create_forum_topic(notifier.client, notifier.token, route.chat_id, &alias)
                    {
                        Ok(topic_id) => TelegramRoute {
                            chat_id: route.chat_id,
                            message_thread_id: Some(topic_id),
                        },
                        Err(err) => {
                            return Ok(Some(reply(
                                &route,
                                format!("Failed to create forum topic: {err}"),
                            )))
                        }
                    }
                } else {
                    route.clone()
                }
            } else {
                route.clone()
            };
            let binding = match state.bind_route(paths, &effective_route, Some(&alias)) {
                Ok(binding) => binding,
                Err(err) => {
                    if let (Some(topic_id), Some(notifier)) =
                        (effective_route.message_thread_id, options.notifier)
                    {
                        let _ = delete_forum_topic(
                            notifier.client,
                            notifier.token,
                            effective_route.chat_id,
                            topic_id,
                        );
                    }
                    return Err(err);
                }
            };
            if effective_route.message_thread_id.is_some() && route.message_thread_id.is_none() {
                if let Some(b) = state.binding_for_route_mut(&effective_route, Some(&alias)) {
                    b.topic_created_by_adapter = true;
                }
            }
            Ok(Some(reply(
                &effective_route,
                format!(
                    "Created session `{alias}` for route {}: {}.",
                    effective_route.display(),
                    binding.session_id
                ),
            )))
        }
        TelegramTextCommand::Use { alias } => {
            let Some(alias) = normalize_alias(alias.as_deref()) else {
                return Ok(Some(reply(&route, "Usage: /use <name>")));
            };
            if state.binding_for_route(&route, Some(&alias)).is_none() {
                return Ok(Some(reply(
                    &route,
                    format!(
                        "No session named `{alias}` is bound to route {}.",
                        route.display()
                    ),
                )));
            }
            state.set_active_route(&route, Some(&alias));
            Ok(Some(reply(&route, format!("Using session `{alias}`."))))
        }
        TelegramTextCommand::Close { alias } => {
            let alias = normalize_alias(alias.as_deref())
                .or_else(|| state.active_alias_for_route(&route).map(str::to_string));
            close_bound_route(paths, state, &route, alias.as_deref(), &options)
        }
        TelegramTextCommand::Watch => {
            let Some(binding) = state.active_binding_for_route(&route).cloned() else {
                return Ok(Some(reply(&route, no_session_bound_message(state, &route))));
            };
            watch_bound_thread(paths, state, &route, &binding, &options, true)
        }
        TelegramTextCommand::Message => {
            if is_portal_route(&route, chat_is_forum) {
                return Ok(Some(
                    portal_reply(
                        paths,
                        &route,
                        options.app_server_timeout,
                        None,
                        options.trace_timings,
                    )
                    .unwrap_or_else(|err| portal_unavailable_reply(&route, err, None)),
                ));
            }
            let binding = state
                .active_binding_for_route(&route)
                .cloned()
                .map(Ok)
                .unwrap_or_else(|| state.bind_route(paths, &route, None))?;
            if binding.telegram_paused {
                return Ok(Some(work_panel_reply(
                    &route,
                    &binding,
                    "Telegram handoff is paused for this topic.",
                    None,
                )));
            }
            session::record_channel_message(
                paths,
                RecordChannelMessageRequest {
                    session_id: binding.session_id.clone(),
                    channel_id: binding.channel_id.clone(),
                },
            )?;
            if options.acquire_lease {
                match session::acquire_lease(
                    paths,
                    AcquireLeaseRequest {
                        session_id: binding.session_id.clone(),
                        channel_id: binding.channel_id.clone(),
                        steal: options.steal,
                    },
                ) {
                    Ok(_) => {}
                    Err(err) => {
                        return Ok(Some(reply(
                                &route,
                                format!(
                                    "Session is controlled elsewhere. Use --steal on the adapter if this Telegram channel should take over.\n{err:#}"
                                ),
                            )));
                    }
                }
            }
            let _typing = options.notifier.map(|notifier| notifier.typing(&route));
            match run_codex_turn(
                    paths,
                    state,
                    &route,
                    binding.alias.as_deref(),
                    text,
                    CodexTurnOptions {
                        timeout_secs: options.app_server_timeout,
                        notifier: options.notifier,
                        trace_timings: options.trace_timings,
                    },
                ) {
                    Ok(turn) if turn.assistant_text.trim().is_empty() => {
                        Ok(Some(reply(&route, "Codex completed without a text reply.")))
                    }
                    Ok(turn) if turn.streamed_to_telegram => Ok(None),
                    Ok(turn) => Ok(Some(reply(&route, turn.assistant_text))),
                    Err(err) => Ok(Some(reply(
                        &route,
                        format!("Codex turn failed.\n{err:#}\n\nStart app-server with `cx serve start` and keep it running, then retry."),
                    ))),
                }
        }
    }
}

fn handle_callback(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    chat_is_forum: bool,
    callback_data: &str,
    options: &HandleOptions<'_>,
) -> Result<Option<TelegramReply>> {
    let edit_message_id = options.callback_message_id;
    if callback_data.starts_with("cx:ap:") {
        return Ok(Some(reply(
            route,
            "That approval request is no longer pending.",
        )));
    }
    match TelegramCallbackCommand::parse(callback_data) {
        Some(TelegramCallbackCommand::RefreshPortal) => Ok(Some(
            portal_reply(
                paths,
                route,
                options.app_server_timeout,
                edit_message_id,
                options.trace_timings,
            )
            .unwrap_or_else(|err| portal_unavailable_reply(route, err, edit_message_id)),
        )),
        Some(TelegramCallbackCommand::RefreshWork) => {
            Ok(Some(refresh_work_panel(state, route, edit_message_id)))
        }
        Some(TelegramCallbackCommand::Observe { thread_id }) => {
            observe_portal_thread(paths, state, route, chat_is_forum, &thread_id, options)
                .or_else(|err| Ok(Some(reply(route, portal_unavailable_message(err)))))
        }
        Some(TelegramCallbackCommand::Takeover) => {
            takeover_handoff(paths, state, route, options, edit_message_id)
        }
        Some(TelegramCallbackCommand::Release) => {
            release_handoff(paths, state, route, edit_message_id)
        }
        Some(TelegramCallbackCommand::Close) => {
            close_bound_route(paths, state, route, None, options)
        }
        None => Ok(Some(reply(route, "Unsupported portal action."))),
    }
}

fn portal_reply(
    paths: &ManagerPaths,
    route: &TelegramRoute,
    _timeout_secs: f32,
    edit_message_id: Option<i64>,
    trace_timings: bool,
) -> Result<TelegramReply> {
    let portal_start = Instant::now();
    let route_log = route.display();
    let entries = list_portal_entries(paths, 8, trace_timings, &route_log)?;
    if trace_timings {
        eprintln!(
            "telegram timing route={} phase=portal_complete entries={} elapsed_ms={}",
            route_log,
            entries.len(),
            elapsed_ms(portal_start)
        );
    }
    if entries.is_empty() {
        return Ok(panel_reply_with_keyboard(
                route,
                "No Codex threads are visible. Start or resume a local cx session first, then tap Refresh.",
                portal_keyboard(&entries),
                edit_message_id,
            ));
    }
    Ok(panel_reply_with_keyboard(
        route,
        portal_text(&entries),
        portal_keyboard(&entries),
        edit_message_id,
    ))
}

fn attach_app_thread(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    chat_is_forum: bool,
    thread_id: &str,
    options: &HandleOptions<'_>,
) -> Result<Option<TelegramReply>> {
    observe_portal_thread(paths, state, route, chat_is_forum, thread_id, options)
}

fn observe_portal_thread(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    chat_is_forum: bool,
    thread_id: &str,
    options: &HandleOptions<'_>,
) -> Result<Option<TelegramReply>> {
    let entries = list_portal_entries(paths, 30, options.trace_timings, &route.display())?;
    let Some(entry) = entries.iter().find(|e| e.thread_id == thread_id) else {
        return Ok(Some(reply(
            route,
            "That Codex thread is no longer visible. Run /portal and choose again.",
        )));
    };
    if !entry.watchable {
        return Ok(Some(reply(
            route,
            "That thread cannot be observed right now. Refresh /portal and try again.",
        )));
    }

    let mut topic_created_by_adapter = false;
    let effective_route = if should_create_work_topic(state, route, chat_is_forum) {
        if let Some(notifier) = options.notifier {
            let topic_id = match create_forum_topic(
                notifier.client,
                notifier.token,
                route.chat_id,
                &work_topic_title(entry),
            ) {
                Ok(topic_id) => topic_id,
                Err(err) => {
                    return Ok(Some(reply(
                        route,
                        format!("Failed to create Telegram topic: {err:#}"),
                    )));
                }
            };
            topic_created_by_adapter = true;
            TelegramRoute {
                chat_id: route.chat_id,
                message_thread_id: Some(topic_id),
            }
        } else {
            route.clone()
        }
    } else {
        route.clone()
    };

    let mut binding =
        state.bind_app_thread(paths, &effective_route, entry, topic_created_by_adapter)?;
    if let Some(stored) = state.binding_for_route_mut(&effective_route, None) {
        stored.telegram_paused = true;
        initialize_watch_cursor(paths, stored);
        binding = stored.clone();
    }
    session::record_channel_message(
        paths,
        RecordChannelMessageRequest {
            session_id: binding.session_id.clone(),
            channel_id: binding.channel_id.clone(),
        },
    )?;
    write_state(paths, state)?;

    if let Some(notifier) = options.notifier {
        send_watch_intro(paths, state, notifier, &effective_route, &binding, entry)?;
        write_state(paths, state)?;
    }

    watch_bound_thread(paths, state, &effective_route, &binding, options, false)
}

fn takeover_handoff(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    options: &HandleOptions<'_>,
    edit_message_id: Option<i64>,
) -> Result<Option<TelegramReply>> {
    let Some(binding) = state.active_binding_for_route(route).cloned() else {
        return Ok(Some(reply(route, no_session_bound_message(state, route))));
    };
    match interrupt_binding_thread(paths, &binding, options.app_server_timeout) {
        Ok(interrupted) => {
            if let Some(stored) = state.binding_for_route_mut(route, binding.alias.as_deref()) {
                stored.telegram_paused = false;
            }
            let binding = state
                .binding_for_route(route, binding.alias.as_deref())
                .cloned()
                .unwrap_or(binding);
            Ok(Some(work_panel_reply(
                route,
                &binding,
                takeover_message(interrupted.interrupted_turn_id.as_deref()),
                edit_message_id,
            )))
        }
        Err(err) => Ok(Some(reply(route, portal_unavailable_message(err)))),
    }
}

fn release_handoff(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    edit_message_id: Option<i64>,
) -> Result<Option<TelegramReply>> {
    let Some(binding) = state.active_binding_for_route(route).cloned() else {
        return Ok(Some(reply(route, no_session_bound_message(state, route))));
    };
    if let Some(stored) = state.binding_for_route_mut(route, binding.alias.as_deref()) {
        stored.telegram_paused = true;
    }
    let mut released_lease = false;
    if let Ok(session) = session::show_session(paths, &binding.session_id) {
        if let Some(active_lease) = session.active_lease {
            if active_lease.channel_id == binding.channel_id {
                session::release_lease(
                    paths,
                    session::ReleaseLeaseRequest {
                        session_id: binding.session_id.clone(),
                        lease_token: active_lease.lease_token,
                    },
                )?;
                released_lease = true;
            }
        }
    }
    let suffix = if released_lease {
        " Lease released."
    } else {
        ""
    };
    let binding = state
        .binding_for_route(route, binding.alias.as_deref())
        .cloned()
        .unwrap_or(binding);
    Ok(Some(work_panel_reply(
        route,
        &binding,
        format!("Telegram handoff paused.{suffix} Continue on desktop."),
        edit_message_id,
    )))
}

fn release_binding_lease_if_owned(paths: &ManagerPaths, binding: &TelegramBinding) -> Result<bool> {
    let Ok(session) = session::show_session(paths, &binding.session_id) else {
        return Ok(false);
    };
    let Some(active_lease) = session.active_lease else {
        return Ok(false);
    };
    if active_lease.channel_id != binding.channel_id {
        return Ok(false);
    }
    session::release_lease(
        paths,
        session::ReleaseLeaseRequest {
            session_id: binding.session_id.clone(),
            lease_token: active_lease.lease_token,
        },
    )?;
    Ok(true)
}

fn watch_bound_thread(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    binding: &TelegramBinding,
    _options: &HandleOptions<'_>,
    show_idle_reply: bool,
) -> Result<Option<TelegramReply>> {
    let Some(_thread_id) = binding.app_thread_id.as_deref() else {
        return Ok(Some(reply(
            route,
            "This session is not bound to a Codex thread yet. Send a message first.",
        )));
    };
    if let Some(stored) = state.binding_for_route_mut(route, binding.alias.as_deref()) {
        stored.telegram_paused = true;
        initialize_watch_cursor(paths, stored);
    }
    release_binding_lease_if_owned(paths, binding)?;
    let binding = state
        .binding_for_route(route, binding.alias.as_deref())
        .cloned()
        .unwrap_or_else(|| binding.clone());

    if show_idle_reply {
        return Ok(Some(work_panel_reply(
            route,
            &binding,
            "Telegram handoff paused. Watching for desktop activity.",
            None,
        )));
    }
    Ok(None)
}

fn initialize_watch_cursor(paths: &ManagerPaths, binding: &mut TelegramBinding) {
    binding.watch_activity = None;
    binding.watch_status = None;
    if binding.watch_proxy_offset.is_none() {
        if let Ok(metadata) = fs::metadata(proxy_event_log_path(paths)) {
            binding.watch_proxy_offset = Some(metadata.len());
        }
    }
    if binding.watch_rollout_offset.is_some() {
        return;
    }
    let Some(thread_id) = binding.app_thread_id.as_deref() else {
        return;
    };
    let Some(path) = rollout_path_for_thread(paths, thread_id) else {
        return;
    };
    let Ok(metadata) = fs::metadata(&path) else {
        return;
    };
    let offset = latest_active_rollout_turn(&path)
        .ok()
        .flatten()
        .map(|(_, offset)| offset)
        .unwrap_or_else(|| metadata.len());
    binding.watch_rollout_offset = Some(offset);
}

fn proxy_event_log_path(paths: &ManagerPaths) -> PathBuf {
    paths.serve_dir().join("events").join("default.jsonl")
}

fn refresh_work_panel(
    state: &TelegramState,
    route: &TelegramRoute,
    edit_message_id: Option<i64>,
) -> TelegramReply {
    let Some(binding) = state.active_binding_for_route(route) else {
        return reply(route, no_session_bound_message(state, route));
    };
    work_panel_reply(route, binding, "cx handoff", edit_message_id)
}

fn close_bound_route(
    _paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    alias: Option<&str>,
    options: &HandleOptions<'_>,
) -> Result<Option<TelegramReply>> {
    let alias = alias
        .map(str::to_string)
        .or_else(|| state.active_alias_for_route(route).map(str::to_string));
    let Some(binding) = state.binding_for_route(route, alias.as_deref()).cloned() else {
        return Ok(Some(reply(
            route,
            missing_session_message(state, route, alias.as_deref()),
        )));
    };
    let label = alias.as_deref().unwrap_or("default").to_string();
    if let Some(thread_id) = binding.message_thread_id {
        let mut close_errors = Vec::new();
        if let Some(notifier) = options.notifier {
            if let Err(err) =
                delete_forum_topic(notifier.client, notifier.token, binding.chat_id, thread_id)
            {
                close_errors.push(format!("Failed to delete Telegram topic: {err:#}"));
            }
            state.remove_all_route_bindings(route);
            if close_errors.is_empty() {
                return Ok(None);
            }
            return Ok(Some(reply(
                route,
                format!(
                    "Unbound session `{label}` locally, but some close actions failed.\n{}",
                    close_errors.join("\n")
                ),
            )));
        }
    }
    if state.remove_route_binding(route, alias.as_deref()) {
        Ok(Some(reply(route, format!("Unbound session `{label}`."))))
    } else {
        Ok(Some(reply(
            route,
            missing_session_message(state, route, alias.as_deref()),
        )))
    }
}

fn should_create_work_topic(
    state: &TelegramState,
    route: &TelegramRoute,
    chat_is_forum: bool,
) -> bool {
    chat_is_forum
        && (route.message_thread_id.is_none() || state.active_binding_for_route(route).is_none())
}

fn interrupt_binding_thread(
    paths: &ManagerPaths,
    binding: &TelegramBinding,
    timeout_secs: f32,
) -> Result<crate::app_server::InterruptOutcome> {
    let Some(thread_id) = binding.app_thread_id.as_deref() else {
        return Ok(crate::app_server::InterruptOutcome {
            interrupted_turn_id: None,
        });
    };
    let mut client = connect_app_server(paths, timeout_secs)?;
    client.interrupt_active_turn(thread_id)
}

fn connect_app_server(paths: &ManagerPaths, timeout_secs: f32) -> Result<AppServerClient> {
    connect_app_server_with_timeout(paths, Duration::from_secs_f32(timeout_secs))
}

fn connect_app_server_with_timeout(
    paths: &ManagerPaths,
    timeout: Duration,
) -> Result<AppServerClient> {
    let server = serve::ready_app_server(paths)?;
    let mut client = AppServerClient::connect(&server.listen_url, timeout)?;
    client.initialize("cx-telegram", env!("CARGO_PKG_VERSION"))?;
    Ok(client)
}

fn list_portal_entries(
    paths: &ManagerPaths,
    limit: u64,
    trace_timings: bool,
    route_log: &str,
) -> Result<Vec<AppThreadPortalEntry>> {
    let state_start = Instant::now();
    let server = serve::registered_app_server(paths)?;
    if trace_timings {
        eprintln!(
            "telegram timing route={} phase=portal_app_server_registry elapsed_ms={}",
            route_log,
            elapsed_ms(state_start)
        );
    }
    let connect_start = Instant::now();
    let mut client = AppServerClient::connect(&server.listen_url, PORTAL_APP_SERVER_TIMEOUT)?;
    client.initialize("cx-telegram", env!("CARGO_PKG_VERSION"))?;
    if trace_timings {
        eprintln!(
            "telegram timing route={} phase=portal_app_server_connect timeout_ms={} elapsed_ms={}",
            route_log,
            PORTAL_APP_SERVER_TIMEOUT.as_millis(),
            elapsed_ms(connect_start)
        );
    }
    let list_start = Instant::now();
    let page = client.thread_list(limit)?;
    if trace_timings {
        eprintln!(
            "telegram timing route={} phase=portal_thread_list threads={} elapsed_ms={}",
            route_log,
            page.threads.len(),
            elapsed_ms(list_start)
        );
    }
    Ok(filter_portal_entries(
        page.threads.into_iter().map(AppThreadPortalEntry::from),
        limit,
    ))
}

fn filter_portal_entries<I>(entries: I, limit: u64) -> Vec<AppThreadPortalEntry>
where
    I: IntoIterator<Item = AppThreadPortalEntry>,
{
    entries
        .into_iter()
        .filter(|entry| !is_local_watch_regression_entry(entry))
        .take(limit as usize)
        .collect()
}

#[cfg(test)]
fn open_tui_portal_entries(
    paths: &ManagerPaths,
    max_candidates: u64,
) -> Result<Vec<AppThreadPortalEntry>> {
    let db_path = paths.base_codex_home.join("state_5.sqlite");
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", db_path.display()))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, title, first_user_message, cwd, archived \
                 FROM threads \
                 WHERE rollout_path <> '' \
                 ORDER BY updated_at DESC \
                 LIMIT ?1",
        )
        .with_context(|| format!("prepare open tui query for {}", db_path.display()))?;
    let rows = stmt.query_map(params![max_candidates as i64], |row| {
        let thread_id: String = row.get(0)?;
        let title: String = row.get(1)?;
        let preview: String = row.get(2)?;
        let cwd: String = row.get(3)?;
        let archived = row.get::<_, i64>(4)? != 0;
        Ok((
            AppThreadPortalEntry {
                thread_id,
                title: (!title.trim().is_empty()).then_some(title),
                preview,
                cwd,
                active: false,
                watchable: true,
                status: String::from("open-tui"),
            },
            archived,
        ))
    })?;

    let mut entries = Vec::new();
    for row in rows {
        let (mut entry, archived) = row?;
        if active_rollout_turn(paths, &entry.thread_id)
            .ok()
            .flatten()
            .is_some()
        {
            entry.status = String::from("active-tui");
        } else if archived || !rollout_file_is_open(paths, &entry.thread_id) {
            continue;
        }
        entries.push(entry);
    }
    Ok(entries)
}

fn is_local_watch_regression_entry(entry: &AppThreadPortalEntry) -> bool {
    const MARKERS: [&str; 3] = [
        "For Telegram watch E2E marker",
        "Telegram watch regression",
        "For a local watch integration test",
    ];
    MARKERS.iter().any(|marker| {
        entry
            .title
            .as_deref()
            .is_some_and(|title| title.contains(marker))
            || entry.preview.contains(marker)
    })
}

fn portal_text(entries: &[AppThreadPortalEntry]) -> String {
    let mut lines = vec![String::from(PORTAL_TOPIC_TITLE)];
    for (index, entry) in entries.iter().enumerate() {
        lines.push(format!(
            "{}. {} [{}]\n   cwd: {}",
            index + 1,
            work_topic_title(entry),
            entry.status,
            entry.cwd
        ));
    }
    lines.push(String::from(
        "Threads are observed by default. Use Take over inside a topic to interrupt desktop Codex.",
    ));
    lines.join("\n")
}

fn portal_keyboard(entries: &[AppThreadPortalEntry]) -> TelegramInlineKeyboardMarkup {
    let mut rows = Vec::new();
    for entry in entries {
        rows.push(vec![TelegramInlineKeyboardButton {
            text: format!("Watch: {}", truncate_chars(&work_topic_title(entry), 48)),
            callback_data: format!("cx:o:{}", entry.thread_id),
        }]);
    }
    rows.push(vec![TelegramInlineKeyboardButton {
        text: String::from("Refresh"),
        callback_data: String::from("cx:p"),
    }]);
    TelegramInlineKeyboardMarkup {
        inline_keyboard: rows,
    }
}

fn work_topic_title(entry: &AppThreadPortalEntry) -> String {
    let title = entry
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| {
            let preview = entry.preview.trim();
            (!preview.is_empty()).then_some(preview)
        })
        .unwrap_or("Codex thread");
    truncate_chars(title.lines().next().unwrap_or("Codex thread"), 64)
}

fn send_watch_intro(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    notifier: &TelegramNotifier<'_>,
    route: &TelegramRoute,
    binding: &TelegramBinding,
    entry: &AppThreadPortalEntry,
) -> Result<()> {
    let panel_text = watch_started_text(entry);
    match notifier.send_with_keyboard(route, &panel_text, &work_panel_keyboard(binding, route)) {
        Ok(panel_message_id) => state.remember_panel_message(route, panel_message_id),
        Err(err) => {
            eprintln!(
                "telegram watch intro delivery failed for {}: {err:#}",
                route.display()
            );
            return Ok(());
        }
    }

    if let Some(history) = rollout_history_text(paths, &entry.thread_id, 6)? {
        if let Err(err) = notifier.send_chunks(route, &history) {
            eprintln!(
                "telegram watch history delivery failed for {}: {err:#}",
                route.display()
            );
        }
    }
    Ok(())
}

fn watch_started_text(entry: &AppThreadPortalEntry) -> String {
    format!(
            "Watching Codex thread.\nTelegram handoff is paused for this topic.\nthread: {}\ncwd: {}\nstatus: {}\nLive output will stream here while the desktop turn runs.",
            entry.thread_id, entry.cwd, entry.status
        )
}

fn takeover_message(interrupted_turn_id: Option<&str>) -> String {
    match interrupted_turn_id {
        Some(turn_id) => {
            format!("Telegram handoff active. Interrupted desktop turn {turn_id}.")
        }
        None => {
            String::from("Telegram handoff active. No active desktop turn needed interruption.")
        }
    }
}

fn portal_unavailable_message(err: anyhow::Error) -> String {
    format!(
            "Codex portal is unavailable.\n{err:#}\n\nStart or restart the local service with `cx service start`, then tap Refresh."
        )
}

fn portal_unavailable_reply(
    route: &TelegramRoute,
    err: anyhow::Error,
    edit_message_id: Option<i64>,
) -> TelegramReply {
    panel_reply_with_keyboard(
        route,
        portal_unavailable_message(err),
        portal_keyboard(&[]),
        edit_message_id,
    )
}

fn work_panel_reply(
    route: &TelegramRoute,
    binding: &TelegramBinding,
    headline: impl Into<String>,
    edit_message_id: Option<i64>,
) -> TelegramReply {
    panel_reply_with_keyboard(
        route,
        work_panel_text(binding, headline.into()),
        work_panel_keyboard(binding, route),
        edit_message_id,
    )
}

fn work_panel_text(binding: &TelegramBinding, headline: String) -> String {
    let status = if binding.telegram_paused {
        "paused"
    } else {
        "telegram active"
    };
    format!(
        "{headline}\nstate: {status}\nthread: {}\ncwd: {}",
        binding.app_thread_id.as_deref().unwrap_or("<none>"),
        binding.app_thread_cwd.as_deref().unwrap_or("<unknown>")
    )
}

fn work_panel_keyboard(
    binding: &TelegramBinding,
    route: &TelegramRoute,
) -> TelegramInlineKeyboardMarkup {
    let control = if binding.telegram_paused {
        TelegramInlineKeyboardButton {
            text: String::from("Take over from desktop"),
            callback_data: String::from("cx:t"),
        }
    } else {
        TelegramInlineKeyboardButton {
            text: String::from("Release to desktop"),
            callback_data: String::from("cx:rel"),
        }
    };
    let close_text = if route.message_thread_id.is_some() {
        "Close topic"
    } else {
        "Close session"
    };
    TelegramInlineKeyboardMarkup {
        inline_keyboard: vec![
            vec![control],
            vec![
                TelegramInlineKeyboardButton {
                    text: String::from("Refresh"),
                    callback_data: String::from("cx:w"),
                },
                TelegramInlineKeyboardButton {
                    text: String::from(close_text),
                    callback_data: String::from("cx:c"),
                },
            ],
        ],
    }
}

fn is_portal_route(route: &TelegramRoute, chat_is_forum: bool) -> bool {
    chat_is_forum && route.message_thread_id.is_none()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut result = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        result.push_str("...");
    }
    result
}

struct HandleOptions<'a> {
    notifier: Option<&'a TelegramNotifier<'a>>,
    acquire_lease: bool,
    steal: bool,
    authorized_by_bind: bool,
    app_server_timeout: f32,
    trace_timings: bool,
    callback_data: Option<&'a str>,
    callback_message_id: Option<i64>,
}

struct CodexTurnOptions<'a, 'b> {
    timeout_secs: f32,
    notifier: Option<&'a TelegramNotifier<'b>>,
    trace_timings: bool,
}

fn run_codex_turn(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    alias: Option<&str>,
    prompt: String,
    options: CodexTurnOptions<'_, '_>,
) -> Result<CodexTurnOutput> {
    let timeout_secs = options.timeout_secs;
    let notifier = options.notifier;
    let trace_timings = options.trace_timings;
    let route_log = route.display();
    let ready_start = Instant::now();
    let server = serve::ready_app_server(paths)?;
    if trace_timings {
        eprintln!(
            "telegram timing route={} phase=app_server_ready elapsed_ms={}",
            route_log,
            elapsed_ms(ready_start)
        );
    }
    let connect_start = Instant::now();
    let mut client =
        AppServerClient::connect(&server.listen_url, Duration::from_secs_f32(timeout_secs))?;
    client.initialize("cx-telegram", env!("CARGO_PKG_VERSION"))?;
    if trace_timings {
        eprintln!(
            "telegram timing route={} phase=app_server_connect elapsed_ms={}",
            route_log,
            elapsed_ms(connect_start)
        );
    }

    let binding_snapshot = state
        .binding_for_route(route, alias)
        .cloned()
        .with_context(|| format!("Telegram route {} is not bound", route.display()))?;
    let thread_id = match binding_snapshot.app_thread_id.clone() {
        Some(thread_id) => {
            let resume_start = Instant::now();
            if let Err(err) =
                client.thread_resume(&thread_id, binding_snapshot.app_thread_cwd.as_deref())
            {
                eprintln!("app-server thread/resume before Telegram turn failed: {err:#}");
            }
            if trace_timings {
                eprintln!(
                    "telegram timing route={} thread_id={} phase=thread_resume elapsed_ms={}",
                    route_log,
                    thread_id,
                    elapsed_ms(resume_start)
                );
            }
            thread_id
        }
        None => {
            let start_thread_start = Instant::now();
            let thread = client.thread_start(binding_snapshot.app_thread_cwd.as_deref())?;
            let binding = state
                .binding_for_route_mut(route, alias)
                .with_context(|| format!("Telegram route {} is not bound", route.display()))?;
            binding.app_thread_id = Some(thread.upstream_thread_id.clone());
            if trace_timings {
                eprintln!(
                    "telegram timing route={} thread_id={} phase=thread_start elapsed_ms={}",
                    route_log,
                    thread.upstream_thread_id,
                    elapsed_ms(start_thread_start)
                );
            }
            thread.upstream_thread_id
        }
    };
    let mut sink = notifier
        .map(|notifier| TelegramWatchSink::new_best_effort(notifier, route.clone(), None, None));
    let turn_start = Instant::now();
    let mut first_event_seen = false;
    let turn = client.turn_start_stream(
        &thread_id,
        prompt,
        |event| {
            if trace_timings && !first_event_seen {
                first_event_seen = true;
                eprintln!(
                    "telegram timing route={} thread_id={} phase=turn_first_event elapsed_ms={}",
                    route_log,
                    thread_id,
                    elapsed_ms(turn_start)
                );
            }
            if let Some(sink) = sink.as_mut() {
                sink.push_event(event)?;
            }
            Ok(())
        },
        |approval| {
            let Some(notifier) = notifier else {
                anyhow::bail!("app-server requested approval but Telegram is unavailable");
            };
            handle_app_server_approval(paths, state, notifier, route, approval, timeout_secs)
        },
    )?;
    if trace_timings {
        eprintln!(
                "telegram timing route={} thread_id={} phase=turn_complete first_event={} elapsed_ms={}",
                route_log,
                thread_id,
                first_event_seen,
                elapsed_ms(turn_start)
            );
    }
    if let Some(sink) = sink.as_mut() {
        sink.turn_completed()?;
        let finish_start = Instant::now();
        sink.flush_pending()?;
        if trace_timings {
            eprintln!(
                    "telegram timing route={} thread_id={} phase=telegram_sink_finish sent_any={} elapsed_ms={}",
                    route_log,
                    thread_id,
                    sink.sent_any,
                    elapsed_ms(finish_start)
                );
        }
    }
    Ok(CodexTurnOutput {
        assistant_text: turn.assistant_text,
        streamed_to_telegram: sink.is_some_and(|sink| sink.sent_any()),
    })
}

struct CodexTurnOutput {
    assistant_text: String,
    streamed_to_telegram: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObserveTerminal {
    Completed,
    Aborted,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveRolloutTurn {
    path: PathBuf,
    turn_id: String,
    offset: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedRolloutTerminal {
    turn_id: String,
    terminal: ObserveTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RolloutTaskEvent {
    Started(String),
    Terminal {
        turn_id: Option<String>,
        terminal: ObserveTerminal,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RolloutObserveEvent {
    Stream(AppStreamEvent),
    Terminal {
        turn_id: Option<String>,
        terminal: ObserveTerminal,
        last_agent_message: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RolloutDrain {
    events: Vec<RolloutObserveEvent>,
    next_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RolloutHistoryItem {
    role: RolloutHistoryRole,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RolloutHistoryRole {
    User,
    Codex,
}

impl RolloutHistoryRole {
    fn label(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Codex => "Codex",
        }
    }
}

#[cfg(test)]
fn active_rollout_turn(paths: &ManagerPaths, thread_id: &str) -> Result<Option<ActiveRolloutTurn>> {
    let Some(path) = rollout_path_for_thread(paths, thread_id) else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let Some((turn_id, offset)) = latest_active_rollout_turn(&path)? else {
        return Ok(None);
    };
    let Some(pid) = pid_holding_file(&path) else {
        return Ok(None);
    };
    if !rollout_holder_is_watchable(pid, thread_id) {
        return Ok(None);
    }
    Ok(Some(ActiveRolloutTurn {
        path,
        turn_id,
        offset,
    }))
}

fn latest_active_rollout_turn(path: &Path) -> Result<Option<(String, u64)>> {
    let file = fs::File::open(path).with_context(|| format!("open rollout {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut active_turns = Vec::<(String, u64)>::new();
    let mut line = String::new();
    loop {
        let line_offset = reader
            .stream_position()
            .with_context(|| format!("read rollout position {}", path.display()))?;
        line.clear();
        let read = reader
            .read_line(&mut line)
            .with_context(|| format!("read rollout {}", path.display()))?;
        if read == 0 {
            break;
        }
        apply_rollout_task_event(&mut active_turns, rollout_task_event(&line), line_offset);
    }
    Ok(active_turns
        .last()
        .map(|(turn_id, offset)| (turn_id.clone(), *offset)))
}

fn apply_rollout_task_event(
    active_turns: &mut Vec<(String, u64)>,
    event: Option<RolloutTaskEvent>,
    offset: u64,
) {
    match event {
        Some(RolloutTaskEvent::Started(turn_id))
            if !active_turns
                .iter()
                .any(|(active_turn_id, _)| active_turn_id == &turn_id) =>
        {
            active_turns.push((turn_id, offset));
        }
        Some(RolloutTaskEvent::Started(_)) => {}
        Some(RolloutTaskEvent::Terminal {
            turn_id: Some(turn_id),
            ..
        }) => {
            active_turns.retain(|(active_turn_id, _)| active_turn_id != &turn_id);
        }
        Some(RolloutTaskEvent::Terminal { turn_id: None, .. }) => {
            active_turns.clear();
        }
        None => {}
    }
}

#[cfg(test)]
fn observe_active_rollout<F>(
    path: &Path,
    active_turn_id: &str,
    start_offset: u64,
    timeout: Duration,
    mut on_event: F,
) -> Result<ObservedRolloutTerminal>
where
    F: FnMut(AppStreamEvent) -> Result<()>,
{
    let mut file =
        fs::File::open(path).with_context(|| format!("open rollout {}", path.display()))?;
    file.seek(SeekFrom::Start(start_offset))
        .with_context(|| format!("seek rollout {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut last_activity = Instant::now();
    let mut last_sent_agent_message = None::<String>;

    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .with_context(|| format!("read rollout {}", path.display()))?;
        if read == 0 {
            if last_activity.elapsed() >= timeout {
                anyhow::bail!("timed out waiting for rollout events in {}", path.display());
            }
            thread::sleep(Duration::from_millis(200));
            continue;
        }

        last_activity = Instant::now();
        match rollout_observe_event(&line) {
            Some(RolloutObserveEvent::Stream(AppStreamEvent::AgentDelta(message))) => {
                send_rollout_agent_message(&message, &mut last_sent_agent_message, &mut on_event)?;
            }
            Some(RolloutObserveEvent::Stream(event)) => on_event(event)?,
            Some(RolloutObserveEvent::Terminal {
                turn_id,
                terminal,
                last_agent_message,
            }) if turn_id.as_deref().is_none_or(|id| id == active_turn_id) => {
                if let Some(message) = last_agent_message {
                    send_rollout_agent_message(
                        &message,
                        &mut last_sent_agent_message,
                        &mut on_event,
                    )?;
                }
                return Ok(ObservedRolloutTerminal {
                    turn_id: turn_id.unwrap_or_else(|| active_turn_id.to_string()),
                    terminal,
                });
            }
            _ => {}
        }
    }
}

fn rollout_events_since(path: &Path, start_offset: u64, max_lines: usize) -> Result<RolloutDrain> {
    let mut file =
        fs::File::open(path).with_context(|| format!("open rollout {}", path.display()))?;
    let file_len = file
        .metadata()
        .with_context(|| format!("stat rollout {}", path.display()))?
        .len();
    let start_offset = start_offset.min(file_len);
    file.seek(SeekFrom::Start(start_offset))
        .with_context(|| format!("seek rollout {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut next_offset = start_offset;

    for _ in 0..max_lines {
        let line_offset = reader
            .stream_position()
            .with_context(|| format!("read rollout position {}", path.display()))?;
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .with_context(|| format!("read rollout {}", path.display()))?;
        if read == 0 {
            next_offset = line_offset;
            break;
        }
        if !line.ends_with('\n') {
            next_offset = line_offset;
            break;
        }
        next_offset = reader
            .stream_position()
            .with_context(|| format!("read rollout position {}", path.display()))?;
        if let Some(event) = rollout_observe_event(&line) {
            events.push(event);
        }
    }

    Ok(RolloutDrain {
        events,
        next_offset,
    })
}

fn proxy_events_since(
    path: &Path,
    start_offset: u64,
    max_lines: usize,
    thread_id: &str,
) -> Result<RolloutDrain> {
    let mut file =
        fs::File::open(path).with_context(|| format!("open proxy log {}", path.display()))?;
    let file_len = file
        .metadata()
        .with_context(|| format!("stat proxy log {}", path.display()))?
        .len();
    let start_offset = start_offset.min(file_len);
    file.seek(SeekFrom::Start(start_offset))
        .with_context(|| format!("seek proxy log {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut next_offset = start_offset;

    for _ in 0..max_lines {
        let line_offset = reader
            .stream_position()
            .with_context(|| format!("read proxy log position {}", path.display()))?;
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .with_context(|| format!("read proxy log {}", path.display()))?;
        if read == 0 {
            next_offset = line_offset;
            break;
        }
        if !line.ends_with('\n') {
            next_offset = line_offset;
            break;
        }
        next_offset = reader
            .stream_position()
            .with_context(|| format!("read proxy log position {}", path.display()))?;
        let Some(event) = proxy_log_observe_event(&line, thread_id) else {
            continue;
        };
        events.push(event);
    }

    Ok(RolloutDrain {
        events,
        next_offset,
    })
}

fn proxy_log_observe_event(line: &str, thread_id: &str) -> Option<RolloutObserveEvent> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let message = value.get("message")?;
    if value.get("direction").and_then(Value::as_str) == Some("client_to_server") {
        if let Some(message) = app_server_user_message(message, thread_id) {
            return Some(RolloutObserveEvent::Stream(AppStreamEvent::UserMessage(
                message,
            )));
        }
    }
    match parse_server_event(message, thread_id, None)? {
        ParsedServerEvent::Stream(event) => Some(RolloutObserveEvent::Stream(event)),
        ParsedServerEvent::TurnCompleted { turn_id } => Some(RolloutObserveEvent::Terminal {
            turn_id: Some(turn_id),
            terminal: ObserveTerminal::Completed,
            last_agent_message: None,
        }),
    }
}

fn app_server_user_message(message: &Value, thread_id: &str) -> Option<String> {
    if message.get("method").and_then(Value::as_str) != Some("turn/start") {
        return None;
    }
    let params = message.get("params")?;
    if params
        .get("threadId")
        .or_else(|| params.get("thread_id"))
        .and_then(Value::as_str)?
        != thread_id
    {
        return None;
    }
    let input = params.get("input")?.as_array()?;
    let parts = input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn send_rollout_agent_message<F>(
    message: &str,
    last_sent_agent_message: &mut Option<String>,
    on_event: &mut F,
) -> Result<()>
where
    F: FnMut(AppStreamEvent) -> Result<()>,
{
    if message.trim().is_empty() {
        return Ok(());
    }
    if last_sent_agent_message.as_deref() == Some(message) {
        return Ok(());
    }
    on_event(AppStreamEvent::AgentDelta(message.to_string()))?;
    *last_sent_agent_message = Some(message.to_string());
    Ok(())
}

fn rollout_task_event(line: &str) -> Option<RolloutTaskEvent> {
    let top_level = serde_json::from_str::<Value>(line).ok()?;
    if top_level.get("type").and_then(Value::as_str) == Some("turn_context") {
        return top_level
            .get("payload")
            .and_then(|payload| payload.get("turn_id"))
            .and_then(Value::as_str)
            .map(|turn_id| RolloutTaskEvent::Started(turn_id.to_string()));
    }

    let value = rollout_event_payload_value(&top_level)?;
    let kind = value.get("type")?.as_str()?;
    match kind {
        "task_started" | "turn_started" => value
            .get("turn_id")
            .and_then(Value::as_str)
            .map(|turn_id| RolloutTaskEvent::Started(turn_id.to_string())),
        "task_complete" | "turn_complete" => Some(RolloutTaskEvent::Terminal {
            turn_id: value
                .get("turn_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            terminal: ObserveTerminal::Completed,
        }),
        "turn_aborted" => Some(RolloutTaskEvent::Terminal {
            turn_id: value
                .get("turn_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            terminal: ObserveTerminal::Aborted,
        }),
        _ => None,
    }
}

fn rollout_observe_event(line: &str) -> Option<RolloutObserveEvent> {
    let top_level = serde_json::from_str::<Value>(line).ok()?;
    if top_level.get("type").and_then(Value::as_str) == Some("compacted") {
        return Some(RolloutObserveEvent::Stream(AppStreamEvent::Info(
            "Context compacted".to_string(),
        )));
    }
    if top_level.get("type").and_then(Value::as_str) == Some("response_item") {
        return rollout_response_item_observe_event(top_level.get("payload")?);
    }

    let value = rollout_event_payload_value(&top_level)?;
    let kind = value.get("type")?.as_str()?;
    match kind {
        "user_message" => value.get("message").and_then(Value::as_str).map(|message| {
            RolloutObserveEvent::Stream(AppStreamEvent::UserMessage(message.to_string()))
        }),
        "task_started" | "turn_started" => {
            Some(RolloutObserveEvent::Stream(AppStreamEvent::TurnStarted))
        }
        "context_compacted" => Some(RolloutObserveEvent::Stream(AppStreamEvent::Info(
            "Context compacted".to_string(),
        ))),
        "agent_message" => value.get("message").and_then(Value::as_str).map(|message| {
            let event = match value.get("phase").and_then(Value::as_str) {
                Some("commentary") => AppStreamEvent::ReasoningDelta(message.to_string()),
                _ => AppStreamEvent::AgentDelta(message.to_string()),
            };
            RolloutObserveEvent::Stream(event)
        }),
        "patch_apply_begin" => Some(RolloutObserveEvent::Stream(AppStreamEvent::CommandStarted(
            rollout_patch_apply_command(value, CommandExecutionStatus::InProgress)?,
        ))),
        "patch_apply_end" => Some(RolloutObserveEvent::Stream(
            AppStreamEvent::CommandCompleted(rollout_patch_apply_command(
                value,
                rollout_patch_apply_status(value),
            )?),
        )),
        "exec_command_end" => Some(RolloutObserveEvent::Stream(
            AppStreamEvent::CommandCompleted(rollout_exec_command_end(value)?),
        )),
        "task_complete" | "turn_complete" => Some(RolloutObserveEvent::Terminal {
            turn_id: value
                .get("turn_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            terminal: ObserveTerminal::Completed,
            last_agent_message: value
                .get("last_agent_message")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        "turn_aborted" => Some(RolloutObserveEvent::Terminal {
            turn_id: value
                .get("turn_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            terminal: ObserveTerminal::Aborted,
            last_agent_message: None,
        }),
        _ => None,
    }
}

fn rollout_response_item_observe_event(value: &Value) -> Option<RolloutObserveEvent> {
    let kind = value.get("type")?.as_str()?;
    match kind {
        "reasoning" => rollout_reasoning_delta(value)
            .map(|text| RolloutObserveEvent::Stream(AppStreamEvent::ReasoningDelta(text))),
        "function_call" if value.get("name").and_then(Value::as_str) == Some("exec_command") => {
            Some(RolloutObserveEvent::Stream(AppStreamEvent::CommandStarted(
                rollout_exec_command_start(value)?,
            )))
        }
        "function_call" if value.get("name").and_then(Value::as_str) == Some("apply_patch") => {
            Some(RolloutObserveEvent::Stream(AppStreamEvent::CommandStarted(
                rollout_apply_patch_start(value)?,
            )))
        }
        "custom_tool_call" if value.get("name").and_then(Value::as_str) == Some("apply_patch") => {
            Some(RolloutObserveEvent::Stream(AppStreamEvent::CommandStarted(
                rollout_apply_patch_start(value)?,
            )))
        }
        "function_call" if value.get("name").and_then(Value::as_str) == Some("update_plan") => {
            Some(RolloutObserveEvent::Stream(
                AppStreamEvent::CommandCompleted(rollout_update_plan_command(value)?),
            ))
        }
        "function_call_output" | "custom_tool_call_output" => {
            let call_id = value.get("call_id").and_then(Value::as_str)?;
            let output = value.get("output").and_then(Value::as_str)?;
            let output = rollout_tool_output_body(output);
            if output.trim().is_empty() {
                None
            } else {
                Some(RolloutObserveEvent::Stream(
                    AppStreamEvent::CommandOutputDelta {
                        item_id: call_id.to_string(),
                        delta: output.to_string(),
                    },
                ))
            }
        }
        _ => None,
    }
}

fn rollout_apply_patch_start(value: &Value) -> Option<CommandExecution> {
    let call_id = value.get("call_id").and_then(Value::as_str)?;
    let patch = value
        .get("input")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .get("arguments")
                .and_then(Value::as_str)
                .map(apply_patch_arguments_text)
        })
        .unwrap_or_default();
    Some(CommandExecution {
        item_id: call_id.to_string(),
        command: "apply_patch".to_string(),
        cwd: String::new(),
        activity: Some(CommandActivity {
            verb: "Edited".to_string(),
            target: patch_activity_target(&patch),
        }),
        status: CommandExecutionStatus::InProgress,
        exit_code: None,
        duration_ms: None,
        aggregated_output: None,
    })
}

fn rollout_patch_apply_command(
    value: &Value,
    status: CommandExecutionStatus,
) -> Option<CommandExecution> {
    let changes = value.get("changes")?;
    let activity = patch_changes_activity(changes);
    let output = patch_apply_output(value);
    Some(CommandExecution {
        item_id: value.get("call_id").and_then(Value::as_str)?.to_string(),
        command: "apply_patch".to_string(),
        cwd: String::new(),
        activity: Some(activity),
        status,
        exit_code: None,
        duration_ms: None,
        aggregated_output: output,
    })
}

fn rollout_patch_apply_status(value: &Value) -> CommandExecutionStatus {
    match value.get("status").and_then(Value::as_str) {
        Some("completed") => CommandExecutionStatus::Completed,
        Some("failed") => CommandExecutionStatus::Failed,
        Some("declined") => CommandExecutionStatus::Declined,
        Some(other) => CommandExecutionStatus::Unknown(other.to_string()),
        None if value.get("success").and_then(Value::as_bool) == Some(false) => {
            CommandExecutionStatus::Failed
        }
        None => CommandExecutionStatus::Completed,
    }
}

fn patch_apply_output(value: &Value) -> Option<String> {
    let mut output = String::new();
    if let Some(stdout) = value.get("stdout").and_then(Value::as_str) {
        output.push_str(stdout);
    }
    if let Some(stderr) = value.get("stderr").and_then(Value::as_str) {
        output.push_str(stderr);
    }
    (!output.trim().is_empty()).then_some(output)
}

fn apply_patch_arguments_text(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return arguments.to_string();
    };
    value
        .get("patch")
        .or_else(|| value.get("input"))
        .or_else(|| value.get("text"))
        .and_then(Value::as_str)
        .unwrap_or(arguments)
        .to_string()
}

fn patch_changes_activity(changes: &Value) -> CommandActivity {
    let Some(changes) = changes.as_object() else {
        return CommandActivity {
            verb: "Edited".to_string(),
            target: "patch".to_string(),
        };
    };

    let mut added = 0usize;
    let mut removed = 0usize;
    let mut kinds = Vec::<&str>::new();
    let mut details = Vec::<String>::new();
    for (path, change) in changes {
        let kind = change
            .get("type")
            .or_else(|| change.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("update");
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
        let (change_added, change_removed) = patch_change_line_counts(change, kind);
        added += change_added;
        removed += change_removed;
        details.push(format!("{path} (+{change_added} -{change_removed})"));
    }

    let verb = if kinds.len() == 1 {
        match kinds[0] {
            "add" | "create" => "Added",
            "delete" | "remove" => "Deleted",
            _ => "Edited",
        }
    } else {
        "Edited"
    };
    let target = if details.len() == 1 {
        details.pop().unwrap_or_else(|| "file".to_string())
    } else {
        let noun = if details.len() == 1 { "file" } else { "files" };
        let mut target = format!("{} {noun} (+{added} -{removed})", details.len());
        for detail in details {
            target.push('\n');
            target.push_str(&detail);
        }
        target
    };

    CommandActivity {
        verb: verb.to_string(),
        target,
    }
}

fn patch_change_line_counts(change: &Value, kind: &str) -> (usize, usize) {
    if let Some(diff) = change
        .get("unified_diff")
        .or_else(|| change.get("diff"))
        .and_then(Value::as_str)
    {
        return diff_line_counts(diff);
    }
    let line_count = change
        .get("content")
        .and_then(Value::as_str)
        .map(|content| content.lines().count())
        .unwrap_or(0);
    match kind {
        "add" | "create" => (line_count, 0),
        "delete" | "remove" => (0, line_count),
        _ => (0, 0),
    }
}

fn diff_line_counts(diff: &str) -> (usize, usize) {
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

fn rollout_update_plan_command(value: &Value) -> Option<CommandExecution> {
    let call_id = value.get("call_id").and_then(Value::as_str)?;
    let arguments = value.get("arguments").and_then(Value::as_str)?;
    Some(CommandExecution {
        item_id: call_id.to_string(),
        command: "update_plan".to_string(),
        cwd: String::new(),
        activity: Some(CommandActivity {
            verb: "Plan".to_string(),
            target: update_plan_activity_target(arguments),
        }),
        status: CommandExecutionStatus::Completed,
        exit_code: None,
        duration_ms: None,
        aggregated_output: None,
    })
}

fn update_plan_activity_target(arguments: &str) -> String {
    let Ok(arguments) = serde_json::from_str::<Value>(arguments) else {
        return "Updated plan".to_string();
    };
    let mut lines = Vec::<String>::new();
    if let Some(explanation) = arguments
        .get("explanation")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|explanation| !explanation.is_empty())
    {
        lines.push(explanation.to_string());
    }
    if let Some(plan) = arguments.get("plan").and_then(Value::as_array) {
        for item in plan {
            let step = item
                .get("step")
                .and_then(Value::as_str)
                .unwrap_or("step")
                .trim();
            let marker = match item.get("status").and_then(Value::as_str) {
                Some("completed") => "✔",
                _ => "□",
            };
            let step = if marker == "✔" {
                format!("~~{step}~~")
            } else {
                step.to_string()
            };
            lines.push(format!("{marker} {step}"));
        }
    }
    if lines.is_empty() {
        "Updated plan".to_string()
    } else {
        lines.join("\n")
    }
}

fn patch_activity_target(patch: &str) -> String {
    #[derive(Default)]
    struct FilePatchSummary {
        path: String,
        added: usize,
        removed: usize,
    }

    let mut files = Vec::<FilePatchSummary>::new();
    let mut current = None::<usize>;
    for line in patch.lines() {
        if let Some(path) = line
            .strip_prefix("*** Update File: ")
            .or_else(|| line.strip_prefix("*** Add File: "))
            .or_else(|| line.strip_prefix("*** Delete File: "))
        {
            files.push(FilePatchSummary {
                path: path.trim().to_string(),
                ..Default::default()
            });
            current = Some(files.len() - 1);
            continue;
        }
        let Some(index) = current else {
            continue;
        };
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("***") {
            continue;
        }
        if line.starts_with('+') {
            files[index].added += 1;
        } else if line.starts_with('-') {
            files[index].removed += 1;
        }
    }

    if files.is_empty() {
        return "patch".to_string();
    }
    if files.len() == 1 {
        let file = &files[0];
        return format!("{} (+{} -{})", file.path, file.added, file.removed);
    }
    let added = files.iter().map(|file| file.added).sum::<usize>();
    let removed = files.iter().map(|file| file.removed).sum::<usize>();
    let mut target = format!("{} files (+{added} -{removed})", files.len());
    for file in files {
        target.push('\n');
        target.push_str(&format!(
            "{} (+{} -{})",
            file.path, file.added, file.removed
        ));
    }
    target
}

fn rollout_reasoning_delta(value: &Value) -> Option<String> {
    let mut parts = Vec::<String>::new();
    if let Some(summary) = value.get("summary").and_then(Value::as_array) {
        for item in summary {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
    }
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        for item in content {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn rollout_exec_command_start(value: &Value) -> Option<CommandExecution> {
    let call_id = value.get("call_id").and_then(Value::as_str)?;
    let arguments = value.get("arguments").and_then(Value::as_str)?;
    let arguments = serde_json::from_str::<Value>(arguments).ok()?;
    Some(CommandExecution {
        item_id: call_id.to_string(),
        command: arguments
            .get("cmd")
            .and_then(Value::as_str)
            .unwrap_or("<unknown command>")
            .to_string(),
        cwd: arguments
            .get("workdir")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>")
            .to_string(),
        activity: None,
        status: CommandExecutionStatus::InProgress,
        exit_code: None,
        duration_ms: None,
        aggregated_output: None,
    })
}

fn rollout_exec_command_end(value: &Value) -> Option<CommandExecution> {
    let call_id = value.get("call_id").and_then(Value::as_str)?;
    let exit_code = value.get("exit_code").and_then(Value::as_i64);
    Some(CommandExecution {
        item_id: call_id.to_string(),
        command: rollout_command_string(value).unwrap_or_else(|| format!("command {call_id}")),
        cwd: value
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>")
            .to_string(),
        activity: rollout_command_activity(value),
        status: rollout_command_status(value.get("status").and_then(Value::as_str), exit_code),
        exit_code,
        duration_ms: rollout_duration_ms(value),
        aggregated_output: command_end_output(value),
    })
}

fn rollout_command_activity(value: &Value) -> Option<CommandActivity> {
    let parsed = value.get("parsed_cmd")?.as_array()?;
    let mut verb = None::<&'static str>;
    let mut targets = Vec::<String>::new();
    for item in parsed {
        let item_verb = item
            .get("type")
            .and_then(Value::as_str)
            .map(command_activity_verb)
            .unwrap_or("Run");
        verb = match verb {
            Some(existing) if existing == item_verb => Some(existing),
            Some(_) => Some("Run"),
            None => Some(item_verb),
        };
        if let Some(target) = parsed_command_target(item) {
            if !targets.iter().any(|existing| existing == &target) {
                targets.push(target);
            }
        }
    }
    let verb = verb?;
    let target = if targets.is_empty() {
        "<unknown>".to_string()
    } else {
        targets.join(", ")
    };
    Some(CommandActivity {
        verb: verb.to_string(),
        target,
    })
}

fn command_activity_verb(command_type: &str) -> &'static str {
    match command_type {
        "read" => "Read",
        "write" | "edit" | "patch" => "Edited",
        "search" => "Search",
        "list" | "list_files" => "List",
        "add" | "create" => "Added",
        "delete" | "remove" => "Deleted",
        "move" | "rename" => "Move",
        "copy" => "Copy",
        "format" => "Format",
        "test" => "Test",
        "build" => "Build",
        "lint" => "Lint",
        _ => "Run",
    }
}

fn parsed_command_target(item: &Value) -> Option<String> {
    match item.get("type").and_then(Value::as_str) {
        Some("read") => parsed_read_target(item),
        Some("search") => parsed_search_target(item),
        _ => parsed_generic_command_target(item),
    }
}

fn parsed_read_target(item: &Value) -> Option<String> {
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty());
    let path = item
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty());
    if let Some(label) = skill_command_label(name, path) {
        return Some(label);
    }
    name.map(str::to_string)
        .or_else(|| path.and_then(command_path_label).map(str::to_string))
        .or_else(|| parsed_command_string(item))
}

fn parsed_search_target(item: &Value) -> Option<String> {
    let query = item
        .get("query")
        .and_then(Value::as_str)
        .filter(|query| !query.trim().is_empty());
    let path = item
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .and_then(command_path_label);
    match (query, path) {
        (Some(query), Some(path)) => Some(format!("{query} in {path}")),
        (Some(query), None) => Some(query.to_string()),
        _ => parsed_generic_command_target(item),
    }
}

fn parsed_generic_command_target(item: &Value) -> Option<String> {
    item.get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            item.get("path")
                .and_then(Value::as_str)
                .and_then(command_path_label)
                .map(str::to_string)
        })
        .or_else(|| parsed_command_string(item))
}

fn parsed_command_string(item: &Value) -> Option<String> {
    item.get("cmd")
        .and_then(Value::as_str)
        .filter(|cmd| !cmd.trim().is_empty())
        .map(str::to_string)
}

fn skill_command_label(name: Option<&str>, path: Option<&str>) -> Option<String> {
    let path = path?;
    let path = Path::new(path.trim());
    if path.file_name().and_then(|name| name.to_str()) != Some("SKILL.md") {
        return None;
    }
    let skill_name = path.parent()?.file_name()?.to_str()?;
    if skill_name.trim().is_empty() {
        return None;
    }
    let label = name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("SKILL.md");
    Some(format!("{label} ({skill_name} skill)"))
}

fn command_path_label(path: &str) -> Option<&str> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .or(Some(path))
}

fn rollout_command_status(status: Option<&str>, exit_code: Option<i64>) -> CommandExecutionStatus {
    match status {
        Some("completed") if exit_code.is_some_and(|code| code != 0) => {
            CommandExecutionStatus::Failed
        }
        Some("completed") => CommandExecutionStatus::Completed,
        Some("failed") => CommandExecutionStatus::Failed,
        Some("declined") => CommandExecutionStatus::Declined,
        Some("running" | "in_progress") => CommandExecutionStatus::InProgress,
        Some(other) => CommandExecutionStatus::Unknown(other.to_string()),
        None if exit_code.is_some_and(|code| code != 0) => CommandExecutionStatus::Failed,
        None => CommandExecutionStatus::Completed,
    }
}

fn rollout_command_string(value: &Value) -> Option<String> {
    let command = value.get("command")?.as_array()?;
    if command.len() >= 3
        && command.get(1).and_then(Value::as_str) == Some("-lc")
        && command
            .first()
            .and_then(Value::as_str)
            .is_some_and(|program| {
                program.ends_with("/zsh")
                    || program.ends_with("/bash")
                    || program == "zsh"
                    || program == "bash"
            })
    {
        return command.get(2).and_then(Value::as_str).map(str::to_string);
    }
    Some(
        command
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn rollout_duration_ms(value: &Value) -> Option<i64> {
    if let Some(ms) = value.get("duration_ms").and_then(Value::as_i64) {
        return Some(ms);
    }
    let duration = value.get("duration")?;
    let secs = duration.get("secs").and_then(Value::as_i64).unwrap_or(0);
    let nanos = duration.get("nanos").and_then(Value::as_i64).unwrap_or(0);
    Some(secs.saturating_mul(1000) + nanos / 1_000_000)
}

fn command_end_output(value: &Value) -> Option<String> {
    let mut output = String::new();
    if let Some(stdout) = value.get("stdout").and_then(Value::as_str) {
        output.push_str(stdout);
    }
    if let Some(stderr) = value.get("stderr").and_then(Value::as_str) {
        output.push_str(stderr);
    }
    if output.trim().is_empty() {
        output = value
            .get("aggregated_output")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
    }
    (!output.trim().is_empty()).then_some(output)
}

fn exec_command_output_body(output: &str) -> &str {
    output
        .split_once("\nOutput:\n")
        .map_or(output, |(_, body)| body)
}

fn rollout_tool_output_body(output: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(output) {
        if let Some(output) = value.get("output").and_then(Value::as_str) {
            return exec_command_output_body(output).to_string();
        }
    }
    exec_command_output_body(output).to_string()
}

fn rollout_event_payload(line: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    rollout_event_payload_value(&value).cloned()
}

fn rollout_event_payload_value(value: &Value) -> Option<&Value> {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    value.get("payload")
}

fn rollout_history_text(
    paths: &ManagerPaths,
    thread_id: &str,
    max_items: usize,
) -> Result<Option<String>> {
    if max_items == 0 {
        return Ok(None);
    }
    let Some(path) = rollout_path_for_thread(paths, thread_id) else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }

    let file = fs::File::open(&path).with_context(|| format!("open rollout {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut items = Vec::<RolloutHistoryItem>::new();
    for line in reader.lines() {
        let line = line.with_context(|| format!("read rollout {}", path.display()))?;
        let Some(item) = rollout_history_item(&line) else {
            continue;
        };
        items.push(item);
        if items.len() > max_items {
            items.remove(0);
        }
    }

    if items.is_empty() {
        return Ok(None);
    }

    let mut text = String::from("Recent thread history:");
    for item in items {
        text.push_str("\n\n");
        text.push_str(item.role.label());
        text.push_str(": ");
        text.push_str(&compact_history_message(&item.message));
    }
    Ok(Some(text))
}

fn rollout_history_item(line: &str) -> Option<RolloutHistoryItem> {
    let value = rollout_event_payload(line)?;
    let kind = value.get("type")?.as_str()?;
    let (role, message) = match kind {
        "user_message" => (RolloutHistoryRole::User, value.get("message")?.as_str()?),
        "agent_message" => (RolloutHistoryRole::Codex, value.get("message")?.as_str()?),
        "task_complete" | "turn_complete" => (
            RolloutHistoryRole::Codex,
            value.get("last_agent_message")?.as_str()?,
        ),
        _ => return None,
    };
    if message.trim().is_empty() {
        return None;
    }
    Some(RolloutHistoryItem {
        role,
        message: message.to_string(),
    })
}

fn compact_history_message(message: &str) -> String {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&normalized, 1800)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RolloutThreadInfo {
    path: PathBuf,
    archived: bool,
}

fn rollout_thread_info(paths: &ManagerPaths, thread_id: &str) -> Option<RolloutThreadInfo> {
    let db_path = paths.base_codex_home.join("state_5.sqlite");
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT rollout_path, archived \
                 FROM threads \
                 WHERE id = ?1 \
                 ORDER BY archived ASC \
                 LIMIT 1",
        )
        .ok()?;
    stmt.query_row(params![thread_id], |row| {
        let path: String = row.get(0)?;
        let archived = row.get::<_, i64>(1)? != 0;
        Ok(RolloutThreadInfo {
            path: PathBuf::from(path),
            archived,
        })
    })
    .ok()
}

fn rollout_path_for_thread(paths: &ManagerPaths, thread_id: &str) -> Option<PathBuf> {
    rollout_thread_info(paths, thread_id).map(|info| info.path)
}

#[cfg(test)]
fn rollout_file_is_open(paths: &ManagerPaths, thread_id: &str) -> bool {
    let Some(info) = rollout_thread_info(paths, thread_id) else {
        return false;
    };
    if info.archived {
        return false;
    }
    let Some(pid) = pid_holding_file(&info.path) else {
        return false;
    };
    rollout_holder_is_watchable(pid, thread_id)
}

#[cfg(test)]
fn rollout_holder_is_watchable(pid: u32, thread_id: &str) -> bool {
    let urls = pid_listening_urls(pid);
    if urls.is_empty() {
        return true;
    }
    urls.into_iter().any(|url| {
        app_server_has_active_turn(&url, thread_id, ROLLOUT_OWNER_PROBE_TIMEOUT).unwrap_or(false)
    })
}

#[cfg(test)]
fn app_server_has_active_turn(url: &str, thread_id: &str, timeout: Duration) -> Result<bool> {
    let mut client = AppServerClient::connect(url, timeout)?;
    client.initialize("cx-telegram", env!("CARGO_PKG_VERSION"))?;
    Ok(client.active_turn_id(thread_id)?.is_some())
}

#[cfg(test)]
fn pid_holding_file(path: &Path) -> Option<u32> {
    let output = std::process::Command::new("lsof")
        .args(["-nP", "-Fp", "--"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| line.strip_prefix('p')?.parse::<u32>().ok())
}

#[cfg(test)]
fn pid_listening_urls(pid: u32) -> Vec<String> {
    let output = std::process::Command::new("lsof")
        .args([
            "-nP",
            "-iTCP",
            "-sTCP:LISTEN",
            "-Fn",
            "-p",
            &pid.to_string(),
        ])
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    loopback_listen_urls(&text)
}

#[cfg(test)]
fn loopback_listen_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for line in text.lines() {
        let addr = line.strip_prefix('n').unwrap_or(line);
        let Some(port) = loopback_port(addr) else {
            continue;
        };
        urls.push(format!("ws://127.0.0.1:{port}"));
    }
    urls.sort();
    urls.dedup();
    urls
}

#[cfg(test)]
fn loopback_port(addr: &str) -> Option<u16> {
    addr.strip_prefix("127.0.0.1:")
        .or_else(|| addr.strip_prefix("localhost:"))
        .or_else(|| addr.strip_prefix("[::1]:"))
        .and_then(|port| port.parse::<u16>().ok())
}

struct TelegramNotifier<'a> {
    client: &'a Client,
    token: &'a str,
}

impl TelegramNotifier<'_> {
    fn ack_seen(&self, message: &TelegramMessage) {
        if let Err(err) = set_message_reaction(
            self.client,
            self.token,
            message.chat.id,
            message.message_id,
            "\u{1f440}",
        ) {
            eprintln!("telegram setMessageReaction failed: {err:#}");
        }
    }

    fn answer_callback_query(&self, callback_query_id: &str, text: Option<&str>) {
        if let Err(err) = answer_callback_query(self.client, self.token, callback_query_id, text) {
            eprintln!("telegram answerCallbackQuery failed: {err:#}");
        }
    }

    fn typing(&self, route: &TelegramRoute) -> TelegramTypingGuard {
        TelegramTypingGuard::start(self.client.clone(), self.token.to_string(), route.clone())
    }

    fn send_one(&self, route: &TelegramRoute, text: &str) -> Result<i64> {
        send_message(
            self.client,
            self.token,
            route.chat_id,
            route.message_thread_id,
            text,
            None,
        )
    }

    fn send_chunks(&self, route: &TelegramRoute, text: &str) -> Result<Option<i64>> {
        let mut first_message_id = None;
        for chunk in telegram_text_chunks(text) {
            let message_id = self.send_one(route, &chunk)?;
            if first_message_id.is_none() {
                first_message_id = Some(message_id);
            }
        }
        Ok(first_message_id)
    }

    fn send_with_keyboard(
        &self,
        route: &TelegramRoute,
        text: &str,
        reply_markup: &TelegramInlineKeyboardMarkup,
    ) -> Result<i64> {
        send_message(
            self.client,
            self.token,
            route.chat_id,
            route.message_thread_id,
            text,
            Some(reply_markup),
        )
    }

    fn edit_one(&self, route: &TelegramRoute, message_id: i64, text: &str) -> Result<()> {
        edit_message_text(
            self.client,
            self.token,
            route.chat_id,
            message_id,
            text,
            None,
        )
    }
}

fn handle_app_server_approval(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    notifier: &TelegramNotifier<'_>,
    route: &TelegramRoute,
    approval: ApprovalRequest,
    timeout_secs: f32,
) -> Result<Value> {
    let nonce = generate_bind_secret()?;
    let prompt = approval_prompt_text(&approval);
    let message_id = notifier.send_with_keyboard(route, &prompt, &approval_keyboard(&nonce))?;
    let decision = wait_for_approval_callback(
        paths,
        state,
        notifier,
        route,
        message_id,
        &nonce,
        timeout_secs,
    )?;
    let resolved = format!("{prompt}\n\n{}", decision.label());
    if let Err(err) = notifier.edit_one(route, message_id, &resolved) {
        eprintln!("telegram edit approval prompt failed: {err:#}");
    }
    approval_response_result(&approval, decision)
}

fn wait_for_approval_callback(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    notifier: &TelegramNotifier<'_>,
    route: &TelegramRoute,
    approval_message_id: i64,
    nonce: &str,
    timeout_secs: f32,
) -> Result<TelegramApprovalDecision> {
    let timeout = Duration::from_secs_f32(timeout_secs.max(1.0));
    let deadline = Instant::now() + timeout;
    loop {
        let now = Instant::now();
        if now >= deadline {
            anyhow::bail!("timed out waiting for Telegram approval");
        }
        let remaining = deadline.saturating_duration_since(now);
        let poll_timeout = remaining.as_secs().clamp(1, 10);
        let updates = get_updates(
            notifier.client,
            notifier.token,
            state.last_update_id.map(|id| id + 1),
            poll_timeout,
        )?;
        for update in updates {
            state.last_update_id = Some(
                state
                    .last_update_id
                    .map_or(update.update_id, |id| id.max(update.update_id)),
            );
            let view = update.view();
            let Some(callback_query_id) = view.callback_query_id.as_deref() else {
                continue;
            };
            let Some(message) = view.message.as_ref() else {
                notifier.answer_callback_query(callback_query_id, Some("Approval is pending."));
                continue;
            };
            let callback_route = TelegramRoute::from_message(message);
            if &callback_route != route || message.message_id != approval_message_id {
                notifier.answer_callback_query(
                    callback_query_id,
                    Some("A different cx approval is pending."),
                );
                continue;
            }
            let Some(data) = view.callback_data.as_deref() else {
                notifier.answer_callback_query(callback_query_id, Some("Missing approval action."));
                continue;
            };
            let Some(decision) = parse_approval_callback_data(data, nonce) else {
                notifier.answer_callback_query(callback_query_id, Some("Stale approval action."));
                continue;
            };
            notifier.answer_callback_query(callback_query_id, Some(decision.label()));
            write_state(paths, state)?;
            return Ok(decision);
        }
        write_state(paths, state)?;
    }
}

fn approval_keyboard(nonce: &str) -> TelegramInlineKeyboardMarkup {
    TelegramInlineKeyboardMarkup {
        inline_keyboard: vec![
            vec![
                TelegramInlineKeyboardButton {
                    text: String::from("Allow once"),
                    callback_data: approval_callback_data(
                        nonce,
                        TelegramApprovalDecision::AcceptOnce,
                    ),
                },
                TelegramInlineKeyboardButton {
                    text: String::from("Allow session"),
                    callback_data: approval_callback_data(
                        nonce,
                        TelegramApprovalDecision::AcceptForSession,
                    ),
                },
            ],
            vec![TelegramInlineKeyboardButton {
                text: String::from("Deny"),
                callback_data: approval_callback_data(nonce, TelegramApprovalDecision::Decline),
            }],
        ],
    }
}

fn approval_prompt_text(approval: &ApprovalRequest) -> String {
    let title = match approval.method.as_str() {
        "item/commandExecution/requestApproval" | "execCommandApproval" => {
            "Codex needs permission to run a command"
        }
        "item/fileChange/requestApproval" | "applyPatchApproval" => {
            "Codex needs permission to change files"
        }
        "item/permissions/requestApproval" => "Codex requests additional permissions",
        _ => "Codex requests approval",
    };
    let mut lines = vec![String::from(title)];
    if let Some(reason) = approval.params.get("reason").and_then(Value::as_str) {
        if !reason.trim().is_empty() {
            lines.push(format!("reason: {}", truncate_chars(reason.trim(), 400)));
        }
    }
    if let Some(cwd) = approval.params.get("cwd").and_then(Value::as_str) {
        lines.push(format!("cwd: {cwd}"));
    }
    if let Some(command) = approval_command_text(&approval.params) {
        lines.push(format!("command:\n{command}"));
    }
    if let Some(grant_root) = approval.params.get("grantRoot").and_then(Value::as_str) {
        lines.push(format!("grant root: {grant_root}"));
    }
    if approval.method == "item/permissions/requestApproval" {
        if let Some(permissions) = approval.params.get("permissions") {
            lines.push(format!(
                "permissions:\n{}",
                truncate_chars(&format_json_compact(permissions), 1200)
            ));
        }
    }
    lines.push(String::from(
        "Choose how cx should answer this approval request.",
    ));
    truncate_chars(&lines.join("\n"), 2800)
}

fn approval_command_text(params: &Value) -> Option<String> {
    if let Some(command) = params.get("command").and_then(Value::as_str) {
        if !command.trim().is_empty() {
            return Some(truncate_chars(command.trim(), 1200));
        }
    }
    let command = params.get("command")?.as_array()?;
    let tokens = command.iter().filter_map(Value::as_str).collect::<Vec<_>>();
    (!tokens.is_empty()).then(|| truncate_chars(&tokens.join(" "), 1200))
}

fn approval_response_result(
    approval: &ApprovalRequest,
    decision: TelegramApprovalDecision,
) -> Result<Value> {
    match approval.method.as_str() {
        "item/commandExecution/requestApproval" => Ok(json!({
            "decision": command_execution_decision(decision),
        })),
        "item/fileChange/requestApproval" => Ok(json!({
            "decision": file_change_decision(decision),
        })),
        "execCommandApproval" | "applyPatchApproval" => Ok(json!({
            "decision": legacy_review_decision(decision),
        })),
        "item/permissions/requestApproval" => {
            let permissions = if decision == TelegramApprovalDecision::Decline {
                json!({})
            } else {
                approval
                    .params
                    .get("permissions")
                    .cloned()
                    .unwrap_or_else(|| json!({}))
            };
            let scope = match decision {
                TelegramApprovalDecision::AcceptForSession => "session",
                TelegramApprovalDecision::AcceptOnce | TelegramApprovalDecision::Decline => "turn",
            };
            Ok(json!({
                "permissions": permissions,
                "scope": scope,
                "strictAutoReview": false,
            }))
        }
        method => anyhow::bail!("unsupported approval request method: {method}"),
    }
}

fn command_execution_decision(decision: TelegramApprovalDecision) -> &'static str {
    match decision {
        TelegramApprovalDecision::AcceptOnce => "accept",
        TelegramApprovalDecision::AcceptForSession => "acceptForSession",
        TelegramApprovalDecision::Decline => "decline",
    }
}

fn file_change_decision(decision: TelegramApprovalDecision) -> &'static str {
    match decision {
        TelegramApprovalDecision::AcceptOnce => "accept",
        TelegramApprovalDecision::AcceptForSession => "acceptForSession",
        TelegramApprovalDecision::Decline => "decline",
    }
}

fn legacy_review_decision(decision: TelegramApprovalDecision) -> &'static str {
    match decision {
        TelegramApprovalDecision::AcceptOnce => "approved",
        TelegramApprovalDecision::AcceptForSession => "approved_for_session",
        TelegramApprovalDecision::Decline => "denied",
    }
}

fn format_json_compact(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

struct TelegramTypingGuard {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TelegramTypingGuard {
    fn start(client: Client, token: String, route: TelegramRoute) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                if let Err(err) =
                    send_chat_action(&client, &token, route.chat_id, route.message_thread_id)
                {
                    eprintln!("telegram sendChatAction failed: {err:#}");
                }
                for _ in 0..8 {
                    if worker_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(500));
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for TelegramTypingGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct TelegramWatchSink<'a> {
    status: TelegramStatusPanel,
    thinking: TelegramThinkingPanel,
    agent: TelegramDeltaSink<'a>,
    activity: TelegramActivityPanel,
    sent_any: bool,
    best_effort_delivery: bool,
}

impl<'a> TelegramWatchSink<'a> {
    fn new_best_effort(
        notifier: &'a TelegramNotifier<'a>,
        route: TelegramRoute,
        activity: Option<TelegramActivityState>,
        status: Option<TelegramStatusState>,
    ) -> Self {
        Self {
            status: TelegramStatusPanel::from_state(status),
            thinking: TelegramThinkingPanel::new(),
            agent: TelegramDeltaSink::new(notifier, route),
            activity: TelegramActivityPanel::from_state(activity),
            sent_any: false,
            best_effort_delivery: true,
        }
    }

    fn push_event(&mut self, event: AppStreamEvent) -> Result<()> {
        match event {
            AppStreamEvent::UserMessage(message) => {
                match self
                    .agent
                    .notifier
                    .send_chunks(&self.agent.route, &user_watch_text(&message))
                {
                    Ok(Some(_)) => {
                        self.sent_any = true;
                    }
                    Ok(None) => {}
                    Err(err) if is_telegram_missing_thread_error(&err) => return Err(err),
                    Err(err) if self.best_effort_delivery => {
                        log_watch_delivery_failure(&self.agent.route, "user message", err);
                    }
                    Err(err) => return Err(err),
                }
                Ok(())
            }
            AppStreamEvent::TurnStarted => {
                self.ensure_status_started(false)?;
                self.thinking.start();
                if flush_thinking_panel(
                    &mut self.thinking,
                    self.agent.notifier,
                    &self.agent.route,
                    self.best_effort_delivery,
                    false,
                )? {
                    self.sent_any = true;
                }
                Ok(())
            }
            AppStreamEvent::Info(message) => {
                self.ensure_status_started(false)?;
                if self.thinking.is_active() {
                    self.thinking.finish();
                    if flush_thinking_panel(
                        &mut self.thinking,
                        self.agent.notifier,
                        &self.agent.route,
                        self.best_effort_delivery,
                        true,
                    )? {
                        self.sent_any = true;
                    }
                }
                match self
                    .agent
                    .notifier
                    .send_chunks(&self.agent.route, &info_watch_text(&message))
                {
                    Ok(Some(_)) => {
                        self.sent_any = true;
                    }
                    Ok(None) => {}
                    Err(err) if is_telegram_missing_thread_error(&err) => return Err(err),
                    Err(err) if self.best_effort_delivery => {
                        log_watch_delivery_failure(&self.agent.route, "info", err);
                    }
                    Err(err) => return Err(err),
                }
                Ok(())
            }
            AppStreamEvent::ReasoningStarted => {
                self.ensure_status_started(false)?;
                self.thinking.start();
                if flush_thinking_panel(
                    &mut self.thinking,
                    self.agent.notifier,
                    &self.agent.route,
                    self.best_effort_delivery,
                    false,
                )? {
                    self.sent_any = true;
                }
                Ok(())
            }
            AppStreamEvent::ReasoningDelta(delta) => {
                self.ensure_status_started(false)?;
                self.thinking.push(&delta);
                if flush_thinking_panel(
                    &mut self.thinking,
                    self.agent.notifier,
                    &self.agent.route,
                    self.best_effort_delivery,
                    false,
                )? {
                    self.sent_any = true;
                }
                Ok(())
            }
            AppStreamEvent::AgentDelta(delta) => {
                self.ensure_status_started(false)?;
                if self.thinking.is_active() {
                    self.thinking.finish();
                    if flush_thinking_panel(
                        &mut self.thinking,
                        self.agent.notifier,
                        &self.agent.route,
                        self.best_effort_delivery,
                        true,
                    )? {
                        self.sent_any = true;
                    }
                }
                if let Err(err) = self.agent.push(&delta) {
                    if is_telegram_missing_thread_error(&err) {
                        return Err(err);
                    }
                    if !self.best_effort_delivery {
                        return Err(err);
                    }
                    self.agent.last_sent_chars = self.agent.text.chars().count();
                    log_watch_delivery_failure(&self.agent.route, "agent message", err);
                }
                Ok(())
            }
            AppStreamEvent::CommandStarted(command) => {
                self.ensure_status_started(false)?;
                self.activity.apply_execution(command);
                if flush_activity_panel(
                    &mut self.activity,
                    self.agent.notifier,
                    &self.agent.route,
                    self.best_effort_delivery,
                    false,
                )? {
                    self.sent_any = true;
                }
                Ok(())
            }
            AppStreamEvent::CommandOutputDelta { item_id, delta } => {
                self.ensure_status_started(false)?;
                self.activity.apply_output_delta(&item_id, &delta);
                if flush_activity_panel(
                    &mut self.activity,
                    self.agent.notifier,
                    &self.agent.route,
                    self.best_effort_delivery,
                    false,
                )? {
                    self.sent_any = true;
                }
                Ok(())
            }
            AppStreamEvent::CommandCompleted(command) => {
                self.ensure_status_started(false)?;
                self.activity.apply_execution(command);
                if flush_activity_panel(
                    &mut self.activity,
                    self.agent.notifier,
                    &self.agent.route,
                    self.best_effort_delivery,
                    false,
                )? {
                    self.sent_any = true;
                }
                Ok(())
            }
        }
    }

    fn ensure_status_started(&mut self, force: bool) -> Result<()> {
        self.status.start();
        if flush_status_panel(
            &mut self.status,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            force,
        )? {
            self.sent_any = true;
        }
        Ok(())
    }

    fn turn_completed(&mut self) -> Result<()> {
        self.status.finish();
        self.thinking.finish();
        if flush_thinking_panel(
            &mut self.thinking,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            true,
        )? {
            self.sent_any = true;
        }
        self.activity.finish_turn();
        if flush_status_panel(
            &mut self.status,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            true,
        )? {
            self.sent_any = true;
        }
        if flush_activity_panel(
            &mut self.activity,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            true,
        )? {
            self.sent_any = true;
        }
        Ok(())
    }

    fn flush_pending(&mut self) -> Result<()> {
        if flush_status_panel(
            &mut self.status,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            false,
        )? {
            self.sent_any = true;
        }
        if flush_thinking_panel(
            &mut self.thinking,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            false,
        )? {
            self.sent_any = true;
        }
        if let Err(err) = self.agent.finish() {
            if is_telegram_missing_thread_error(&err) {
                return Err(err);
            }
            if !self.best_effort_delivery {
                return Err(err);
            }
            log_watch_delivery_failure(&self.agent.route, "agent message", err);
        }
        if flush_activity_panel(
            &mut self.activity,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            true,
        )? {
            self.sent_any = true;
        }
        Ok(())
    }

    fn sent_any(&self) -> bool {
        self.sent_any
            || self.status.sent_any()
            || self.thinking.sent_any()
            || self.agent.sent_any()
            || self.activity.sent_any()
    }

    fn activity_state(&self) -> Option<TelegramActivityState> {
        self.activity.to_state()
    }

    fn status_state(&self) -> Option<TelegramStatusState> {
        self.status.to_state()
    }
}

fn flush_thinking_panel(
    panel: &mut TelegramThinkingPanel,
    notifier: &TelegramNotifier<'_>,
    route: &TelegramRoute,
    best_effort_delivery: bool,
    force: bool,
) -> Result<bool> {
    let delivery = TelegramTranscriptDelivery { notifier, route };
    match panel.flush(&delivery, force) {
        Ok(sent) => Ok(sent),
        Err(err) if best_effort_delivery && is_telegram_missing_thread_error(&err) => Err(err),
        Err(err) if best_effort_delivery => {
            panel.mark_delivery_attempted();
            log_watch_delivery_failure(route, "thinking", err);
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

fn flush_activity_panel(
    panel: &mut TelegramActivityPanel,
    notifier: &TelegramNotifier<'_>,
    route: &TelegramRoute,
    best_effort_delivery: bool,
    force: bool,
) -> Result<bool> {
    let delivery = TelegramTranscriptDelivery { notifier, route };
    match panel.flush(&delivery, force) {
        Ok(sent) => Ok(sent),
        Err(err) if best_effort_delivery && is_telegram_missing_thread_error(&err) => Err(err),
        Err(err) if best_effort_delivery => {
            panel.mark_delivery_attempted();
            log_watch_delivery_failure(route, "activity", err);
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

fn flush_status_panel(
    panel: &mut TelegramStatusPanel,
    notifier: &TelegramNotifier<'_>,
    route: &TelegramRoute,
    best_effort_delivery: bool,
    force: bool,
) -> Result<bool> {
    let delivery = TelegramTranscriptDelivery { notifier, route };
    match panel.flush(&delivery, force) {
        Ok(sent) => Ok(sent),
        Err(err) if best_effort_delivery && is_telegram_missing_thread_error(&err) => Err(err),
        Err(err) if best_effort_delivery => {
            panel.mark_delivery_attempted();
            log_watch_delivery_failure(route, "status", err);
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

fn log_watch_delivery_failure(route: &TelegramRoute, kind: &str, err: anyhow::Error) {
    eprintln!(
        "telegram watch {kind} delivery failed for {}: {err:#}",
        route.display()
    );
}

struct TelegramTranscriptDelivery<'a, 'b> {
    notifier: &'a TelegramNotifier<'b>,
    route: &'a TelegramRoute,
}

impl TelegramTranscriptTarget for TelegramTranscriptDelivery<'_, '_> {
    fn send_one(&self, text: &str) -> Result<i64> {
        self.notifier.send_one(self.route, text)
    }

    fn edit_one(&self, message_id: i64, text: &str) -> Result<()> {
        self.notifier.edit_one(self.route, message_id, text)
    }
}

struct TelegramDeltaSink<'a> {
    notifier: &'a TelegramNotifier<'a>,
    route: TelegramRoute,
    text: String,
    message_id: Option<i64>,
    last_sent_chars: usize,
    last_flush: Instant,
    sent_any: bool,
}

impl<'a> TelegramDeltaSink<'a> {
    const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(900);
    const MIN_DELTA_CHARS: usize = 240;

    fn new(notifier: &'a TelegramNotifier<'a>, route: TelegramRoute) -> Self {
        Self {
            notifier,
            route,
            text: String::new(),
            message_id: None,
            last_sent_chars: 0,
            last_flush: Instant::now() - Self::MIN_EDIT_INTERVAL,
            sent_any: false,
        }
    }

    fn push(&mut self, delta: &str) -> Result<()> {
        self.text.push_str(delta);
        let current_chars = self.text.chars().count();
        if self.message_id.is_none()
            || current_chars.saturating_sub(self.last_sent_chars) >= Self::MIN_DELTA_CHARS
            || delta.contains('\n')
            || self.last_flush.elapsed() >= Self::MIN_EDIT_INTERVAL
        {
            self.flush(false)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.flush(true)
    }

    fn sent_any(&self) -> bool {
        self.sent_any
    }

    fn flush(&mut self, final_flush: bool) -> Result<()> {
        if self.text.trim().is_empty() {
            return Ok(());
        }
        if !final_flush
            && self.message_id.is_some()
            && self.last_flush.elapsed() < Self::MIN_EDIT_INTERVAL
        {
            return Ok(());
        }
        let chunks = telegram_text_chunks(&self.text);
        let first = chunks
            .first()
            .expect("telegram_text_chunks returns at least one chunk");
        match self.message_id {
            Some(message_id) => self.notifier.edit_one(&self.route, message_id, first)?,
            None => {
                let message_id = self.notifier.send_one(&self.route, first)?;
                self.message_id = Some(message_id);
            }
        }
        if final_flush {
            for chunk in chunks.iter().skip(1) {
                self.notifier.send_one(&self.route, chunk)?;
            }
        }
        self.last_sent_chars = self.text.chars().count();
        self.last_flush = Instant::now();
        self.sent_any = true;
        Ok(())
    }
}

fn route_session_lines(state: &TelegramState, chat_id: i64) -> Vec<String> {
    let mut lines = state
        .bindings
        .iter()
        .filter(|binding| binding.chat_id == chat_id)
        .map(|binding| {
            let route = TelegramRoute {
                chat_id: binding.chat_id,
                message_thread_id: binding.message_thread_id,
            };
            let active = state
                .active_binding_for_route(&route)
                .is_some_and(|active| {
                    active.alias == binding.alias
                        && active.message_thread_id == binding.message_thread_id
                });
            let marker = if active { "*" } else { " " };
            format!(
                "{marker} {}\n  route: {}\n  session: {}",
                binding.alias.as_deref().unwrap_or("default"),
                route.display(),
                binding.session_id
            )
        })
        .collect::<Vec<_>>();
    if !lines.is_empty() {
        lines.push(String::from(
            "Commands: /use <name>, /new <name>, /close [name], /status",
        ));
    }
    lines
}

fn no_session_bound_message(state: &TelegramState, route: &TelegramRoute) -> String {
    let available = route_aliases(state, route);
    if available.is_empty() {
        format!(
                "No cx session is bound to route {}. Send /start to create the default session or /new <name> to create a named one.",
                route.display()
            )
    } else {
        format!(
                "No active cx session is bound to route {}. Available sessions: {}. Use /use <name> or send /start to create the default session.",
                route.display(),
                available.join(", ")
            )
    }
}

fn missing_session_message(
    state: &TelegramState,
    route: &TelegramRoute,
    alias: Option<&str>,
) -> String {
    let label = alias.unwrap_or("default");
    let available = route_aliases(state, route);
    if available.is_empty() {
        format!(
                "No session `{label}` is bound to route {}. Send /start to create the default session or /new <name> to create a named one.",
                route.display()
            )
    } else {
        format!(
                "No session `{label}` is bound to route {}. Available sessions: {}. Use /close <name> or /use <name>.",
                route.display(),
                available.join(", ")
            )
    }
}

fn route_aliases(state: &TelegramState, route: &TelegramRoute) -> Vec<String> {
    state
        .bindings
        .iter()
        .filter(|binding| {
            binding.chat_id == route.chat_id && binding.message_thread_id == route.message_thread_id
        })
        .map(|binding| binding.alias.as_deref().unwrap_or("default").to_string())
        .collect()
}

fn normalize_alias(alias: Option<&str>) -> Option<String> {
    let alias = alias?.trim().to_ascii_lowercase();
    if alias.is_empty() {
        return None;
    }
    let normalized = alias
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        .collect::<String>();
    let normalized = normalized.chars().take(32).collect::<String>();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn next_route_alias(state: &TelegramState, route: &TelegramRoute) -> String {
    for index in 1.. {
        let alias = format!("session-{index}");
        if state.binding_for_route(route, Some(&alias)).is_none() {
            return alias;
        }
    }
    unreachable!("unbounded alias search should always find a free alias")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TelegramTextCommand {
    Start,
    Bind { secret: Option<String> },
    Portal,
    Attach { thread_id: Option<String> },
    Takeover,
    Watch,
    New { alias: Option<String> },
    Use { alias: Option<String> },
    Status,
    Sessions,
    Close { alias: Option<String> },
    Release,
    Message,
}

impl TelegramTextCommand {
    fn parse(text: &str) -> Self {
        let mut parts = text.split_whitespace();
        let command = parts.next().unwrap_or_default();
        let command = command.split('@').next().unwrap_or(command);
        match command {
            "/start" => Self::Start,
            "/bind" => Self::Bind {
                secret: parts.next().map(String::from),
            },
            "/portal" | "/history" => Self::Portal,
            "/attach" => Self::Attach {
                thread_id: parts.next().map(String::from),
            },
            "/takeover" | "/take" => Self::Takeover,
            "/watch" | "/observe" => Self::Watch,
            "/new" => Self::New {
                alias: parts.next().map(String::from),
            },
            "/use" => Self::Use {
                alias: parts.next().map(String::from),
            },
            "/status" => Self::Status,
            "/sessions" => Self::Sessions,
            "/close" => Self::Close {
                alias: parts.next().map(String::from),
            },
            "/release" => Self::Release,
            _ => Self::Message,
        }
    }
}

fn reply(route: &TelegramRoute, text: impl Into<String>) -> TelegramReply {
    TelegramReply {
        chat_id: route.chat_id,
        message_thread_id: route.message_thread_id,
        text: text.into(),
        reply_markup: None,
        edit_message_id: None,
        remember_panel_message: false,
    }
}

fn panel_reply_with_keyboard(
    route: &TelegramRoute,
    text: impl Into<String>,
    reply_markup: TelegramInlineKeyboardMarkup,
    edit_message_id: Option<i64>,
) -> TelegramReply {
    TelegramReply {
        chat_id: route.chat_id,
        message_thread_id: route.message_thread_id,
        text: text.into(),
        reply_markup: Some(reply_markup),
        edit_message_id,
        remember_panel_message: true,
    }
}

impl TelegramReply {
    fn route(&self) -> TelegramRoute {
        TelegramRoute {
            chat_id: self.chat_id,
            message_thread_id: self.message_thread_id,
        }
    }
}

fn log_update_summary(
    update_id: i64,
    view: &TelegramUpdateView,
    allowed: &BTreeSet<TelegramRoute>,
) {
    let chat_id = view
        .chat_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| String::from("<none>"));
    let text = view
        .message
        .as_ref()
        .and_then(|message| message.text.as_ref())
        .map(|text| {
            if text.trim().is_empty() {
                "empty"
            } else {
                "present"
            }
        })
        .unwrap_or("none");
    let route = view.message.as_ref().map(TelegramRoute::from_message);
    let allowed = route
        .as_ref()
        .map(|route| allowed.contains(route).to_string())
        .unwrap_or_else(|| String::from("false"));
    let route = route
        .map(|route| route.display())
        .unwrap_or_else(|| String::from("<none>"));
    eprintln!(
        "telegram update update_id={} source={} chat_id={} route={} text={} allowed={}",
        update_id,
        view.source.as_str(),
        chat_id,
        route,
        text,
        allowed
    );
}

fn elapsed_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn telegram_age_ms(message_date_secs: Option<i64>) -> Option<i128> {
    let message_date_secs = message_date_secs?;
    let now_ms = unix_millis() as i128;
    Some(now_ms - i128::from(message_date_secs) * 1000)
}

fn get_updates(
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

fn send_message(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_thread_id: Option<i64>,
    text: &str,
    reply_markup: Option<&TelegramInlineKeyboardMarkup>,
) -> Result<i64> {
    let url = telegram_method_url(token, "sendMessage");
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
    let response = client.post(url).form(&payload).send().map_err(|err| {
        anyhow::anyhow!("Telegram sendMessage request failed: {}", err.without_url())
    })?;
    let response = response
        .json::<TelegramResponse<TelegramSentMessage>>()
        .map_err(|err| {
            anyhow::anyhow!(
                "decode Telegram sendMessage response failed: {}",
                err.without_url()
            )
        })?;
    if !response.ok {
        anyhow::bail!(
            "Telegram sendMessage failed: {}",
            response
                .description
                .unwrap_or_else(|| "unknown error".to_string())
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

fn edit_message_text(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_id: i64,
    text: &str,
    reply_markup: Option<&TelegramInlineKeyboardMarkup>,
) -> Result<()> {
    let url = telegram_method_url(token, "editMessageText");
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
    let response = client.post(url).form(&payload).send().map_err(|err| {
        anyhow::anyhow!(
            "Telegram editMessageText request failed: {}",
            err.without_url()
        )
    })?;
    let response = response
        .json::<TelegramResponse<serde_json::Value>>()
        .map_err(|err| {
            anyhow::anyhow!(
                "decode Telegram editMessageText response failed: {}",
                err.without_url()
            )
        })?;
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
            response
                .description
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct TelegramForumTopicParams<'a> {
    chat_id: &'a str,
    name: &'a str,
}

fn create_forum_topic(client: &Client, token: &str, chat_id: i64, name: &str) -> Result<i64> {
    let url = telegram_method_url(token, "createForumTopic");
    let chat_id = chat_id.to_string();
    let payload = TelegramForumTopicParams {
        chat_id: chat_id.as_str(),
        name,
    };
    let response = client.post(url).form(&payload).send().map_err(|err| {
        anyhow::anyhow!(
            "Telegram createForumTopic request failed: {}",
            err.without_url()
        )
    })?;
    let response = response
        .json::<TelegramResponse<serde_json::Value>>()
        .map_err(|err| {
            anyhow::anyhow!(
                "decode Telegram createForumTopic response failed: {}",
                err.without_url()
            )
        })?;
    if !response.ok {
        anyhow::bail!(
            "Telegram createForumTopic failed: {}",
            response
                .description
                .unwrap_or_else(|| "unknown error".to_string())
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

fn delete_forum_topic(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_thread_id: i64,
) -> Result<()> {
    let url = telegram_method_url(token, "deleteForumTopic");
    let chat_id = chat_id.to_string();
    let message_thread_id = message_thread_id.to_string();
    let payload = TelegramDeleteForumTopicParams {
        chat_id: chat_id.as_str(),
        message_thread_id: message_thread_id.as_str(),
    };
    let response = client.post(url).form(&payload).send().map_err(|err| {
        anyhow::anyhow!(
            "Telegram deleteForumTopic request failed: {}",
            err.without_url()
        )
    })?;
    let response = response
        .json::<TelegramResponse<serde_json::Value>>()
        .map_err(|err| {
            anyhow::anyhow!(
                "decode Telegram deleteForumTopic response failed: {}",
                err.without_url()
            )
        })?;
    if !response.ok {
        anyhow::bail!(
            "Telegram deleteForumTopic failed: {}",
            response
                .description
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct TelegramEditGeneralForumTopicParams<'a> {
    chat_id: &'a str,
    name: &'a str,
}

fn edit_general_forum_topic(client: &Client, token: &str, chat_id: i64, name: &str) -> Result<()> {
    let url = telegram_method_url(token, "editGeneralForumTopic");
    let chat_id = chat_id.to_string();
    let payload = TelegramEditGeneralForumTopicParams {
        chat_id: chat_id.as_str(),
        name,
    };
    let response = client.post(url).form(&payload).send().map_err(|err| {
        anyhow::anyhow!(
            "Telegram editGeneralForumTopic request failed: {}",
            err.without_url()
        )
    })?;
    let response = response
        .json::<TelegramResponse<serde_json::Value>>()
        .map_err(|err| {
            anyhow::anyhow!(
                "decode Telegram editGeneralForumTopic response failed: {}",
                err.without_url()
            )
        })?;
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
            response
                .description
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct TelegramGeneralForumTopicParams<'a> {
    chat_id: &'a str,
}

fn unhide_general_forum_topic(client: &Client, token: &str, chat_id: i64) -> Result<()> {
    let url = telegram_method_url(token, "unhideGeneralForumTopic");
    let chat_id = chat_id.to_string();
    let payload = TelegramGeneralForumTopicParams {
        chat_id: chat_id.as_str(),
    };
    let response = client.post(url).form(&payload).send().map_err(|err| {
        anyhow::anyhow!(
            "Telegram unhideGeneralForumTopic request failed: {}",
            err.without_url()
        )
    })?;
    let response = response
        .json::<TelegramResponse<serde_json::Value>>()
        .map_err(|err| {
            anyhow::anyhow!(
                "decode Telegram unhideGeneralForumTopic response failed: {}",
                err.without_url()
            )
        })?;
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
            response
                .description
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
    Ok(())
}

fn is_telegram_noop_error(description: &str) -> bool {
    description.contains("message is not modified") || description.contains("TOPIC_NOT_MODIFIED")
}

fn is_telegram_missing_thread_error(err: &anyhow::Error) -> bool {
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

fn send_chat_action(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_thread_id: Option<i64>,
) -> Result<()> {
    let url = telegram_method_url(token, "sendChatAction");
    let chat_id = chat_id.to_string();
    let message_thread_id = message_thread_id.map(|thread_id| thread_id.to_string());
    let payload = TelegramSendChatAction {
        chat_id: chat_id.as_str(),
        action: "typing",
        message_thread_id,
    };
    let response = client.post(url).form(&payload).send().map_err(|err| {
        anyhow::anyhow!(
            "Telegram sendChatAction request failed: {}",
            err.without_url()
        )
    })?;
    let response = response
        .json::<TelegramResponse<serde_json::Value>>()
        .map_err(|err| {
            anyhow::anyhow!(
                "decode Telegram sendChatAction response failed: {}",
                err.without_url()
            )
        })?;
    if !response.ok {
        anyhow::bail!(
            "Telegram sendChatAction failed: {}",
            response
                .description
                .unwrap_or_else(|| "unknown error".to_string())
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

fn set_message_reaction(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_id: i64,
    emoji: &str,
) -> Result<()> {
    let url = telegram_method_url(token, "setMessageReaction");
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
    let response = client.post(url).form(&payload).send().map_err(|err| {
        anyhow::anyhow!(
            "Telegram setMessageReaction request failed: {}",
            err.without_url()
        )
    })?;
    let response = response
        .json::<TelegramResponse<serde_json::Value>>()
        .map_err(|err| {
            anyhow::anyhow!(
                "decode Telegram setMessageReaction response failed: {}",
                err.without_url()
            )
        })?;
    if !response.ok {
        anyhow::bail!(
            "Telegram setMessageReaction failed: {}",
            response
                .description
                .unwrap_or_else(|| "unknown error".to_string())
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

fn answer_callback_query(
    client: &Client,
    token: &str,
    callback_query_id: &str,
    text: Option<&str>,
) -> Result<()> {
    let url = telegram_method_url(token, "answerCallbackQuery");
    let payload = TelegramAnswerCallbackQuery {
        callback_query_id,
        text,
    };
    let response = client.post(url).form(&payload).send().map_err(|err| {
        anyhow::anyhow!(
            "Telegram answerCallbackQuery request failed: {}",
            err.without_url()
        )
    })?;
    let response = response
        .json::<TelegramResponse<serde_json::Value>>()
        .map_err(|err| {
            anyhow::anyhow!(
                "decode Telegram answerCallbackQuery response failed: {}",
                err.without_url()
            )
        })?;
    if !response.ok {
        anyhow::bail!(
            "Telegram answerCallbackQuery failed: {}",
            response
                .description
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
    Ok(())
}

fn sync_bot_commands(client: &Client, token: &str) -> Result<()> {
    let url = telegram_method_url(token, "setMyCommands");
    let commands = serde_json::to_string(bot_commands()).context("serialize bot commands")?;
    let response = client
        .post(url)
        .form(&[("commands", commands.as_str())])
        .send()
        .map_err(|err| {
            anyhow::anyhow!(
                "Telegram setMyCommands request failed: {}",
                err.without_url()
            )
        })?;
    let response = response
        .json::<TelegramResponse<serde_json::Value>>()
        .map_err(|err| {
            anyhow::anyhow!(
                "decode Telegram setMyCommands response failed: {}",
                err.without_url()
            )
        })?;
    if !response.ok {
        anyhow::bail!(
            "Telegram setMyCommands failed: {}",
            response
                .description
                .unwrap_or_else(|| "unknown error".to_string())
        );
    }
    Ok(())
}

fn bot_commands() -> &'static [TelegramBotCommand] {
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

fn telegram_html_text(text: &str) -> String {
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

fn send_reply(
    client: &Client,
    token: &str,
    chat_id: i64,
    message_thread_id: Option<i64>,
    text: &str,
    reply_markup: Option<&TelegramInlineKeyboardMarkup>,
) -> Result<Option<i64>> {
    let mut first_message_id = None;
    for (index, chunk) in telegram_text_chunks(text).into_iter().enumerate() {
        let markup = if index == 0 { reply_markup } else { None };
        let message_id = send_message(client, token, chat_id, message_thread_id, &chunk, markup)?;
        if first_message_id.is_none() {
            first_message_id = Some(message_id);
        }
    }
    Ok(first_message_id)
}

fn deliver_reply(client: &Client, token: &str, reply: &TelegramReply) -> Result<Option<i64>> {
    if let Some(message_id) = reply.edit_message_id {
        let chunks = telegram_text_chunks(&reply.text);
        if chunks.len() == 1 {
            edit_message_text(
                client,
                token,
                reply.chat_id,
                message_id,
                &chunks[0],
                reply.reply_markup.as_ref(),
            )?;
            return Ok(Some(message_id));
        }
        edit_message_text(
            client,
            token,
            reply.chat_id,
            message_id,
            &chunks[0],
            reply.reply_markup.as_ref(),
        )?;
        for chunk in chunks.iter().skip(1) {
            send_message(
                client,
                token,
                reply.chat_id,
                reply.message_thread_id,
                chunk,
                None,
            )?;
        }
        return Ok(Some(message_id));
    }
    send_reply(
        client,
        token,
        reply.chat_id,
        reply.message_thread_id,
        &reply.text,
        reply.reply_markup.as_ref(),
    )
}

fn telegram_text_chunks(text: &str) -> Vec<String> {
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

fn read_state(paths: &ManagerPaths) -> Result<TelegramState> {
    let path = paths.telegram_channel_state_file();
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TelegramState::empty());
        }
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let state = serde_json::from_str::<TelegramState>(&content)
        .with_context(|| format!("parse {}", path.display()))?;
    if state.schema_version != TELEGRAM_STATE_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported Telegram state schema version: {}",
            state.schema_version
        );
    }
    Ok(state)
}

fn write_state(paths: &ManagerPaths, state: &TelegramState) -> Result<()> {
    fs::create_dir_all(paths.serve_channels_dir())
        .with_context(|| format!("create {}", paths.serve_channels_dir().display()))?;
    set_private_dir_permissions(&paths.serve_dir())?;
    set_private_dir_permissions(&paths.serve_channels_dir())?;

    let path = paths.telegram_channel_state_file();
    let tmp_path = paths.serve_channels_dir().join("telegram.json.tmp");
    let content = serde_json::to_vec_pretty(state).context("serialize Telegram state")?;
    let mut file = private_open_for_write(&tmp_path)?;
    file.write_all(&content)
        .with_context(|| format!("write {}", tmp_path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("write {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path)
        .with_context(|| format!("rename {} to {}", tmp_path.display(), path.display()))?;
    set_private_file_permissions(&path)?;
    Ok(())
}

fn private_open_for_write(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn set_private_dir_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", path.display()))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use super::*;

    #[derive(Default)]
    struct CapturingTranscriptTarget {
        sent: RefCell<Vec<String>>,
        edited: RefCell<Vec<(i64, String)>>,
    }

    impl TelegramTranscriptTarget for CapturingTranscriptTarget {
        fn send_one(&self, text: &str) -> Result<i64> {
            self.sent.borrow_mut().push(text.to_string());
            Ok(self.sent.borrow().len() as i64)
        }

        fn edit_one(&self, message_id: i64, text: &str) -> Result<()> {
            self.edited
                .borrow_mut()
                .push((message_id, text.to_string()));
            Ok(())
        }
    }

    fn temp_paths(name: &str) -> ManagerPaths {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cx-telegram-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
    }

    fn text_message(chat_id: i64, message_thread_id: Option<i64>, text: &str) -> TelegramMessage {
        TelegramMessage {
            chat: TelegramChat {
                id: chat_id,
                is_forum: false,
            },
            message_id: 1,
            date: None,
            message_thread_id,
            text: Some(text.to_string()),
            forum_topic_edited: None,
        }
    }

    fn handle_options() -> HandleOptions<'static> {
        HandleOptions {
            notifier: None,
            acquire_lease: false,
            steal: false,
            authorized_by_bind: false,
            app_server_timeout: 600.0,
            trace_timings: false,
            callback_data: None,
            callback_message_id: None,
        }
    }

    #[test]
    fn telegram_state_round_trips_without_token() {
        let paths = temp_paths("state");
        let mut state = TelegramState::empty();
        state.last_update_id = Some(123);
        state.bindings.push(TelegramBinding {
            chat_id: 42,
            message_thread_id: None,
            alias: None,
            channel_id: ChannelId::parse("telegram:42").unwrap(),
            session_id: SessionId::parse("sess_manual").unwrap(),
            app_thread_id: None,
            app_thread_title: None,
            app_thread_cwd: None,
            topic_title: None,
            panel_message_id: None,
            telegram_paused: false,
            topic_created_by_adapter: false,
            watch_proxy_offset: None,
            watch_rollout_offset: None,
            watch_activity: None,
            watch_status: None,
        });

        write_state(&paths, &state).unwrap();
        let read_back = read_state(&paths).unwrap();

        assert_eq!(read_back, state);
        let content = fs::read_to_string(paths.telegram_channel_state_file()).unwrap();
        assert!(!content.contains("bot"));
        assert!(!content.contains("token"));

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn start_message_creates_session_binding() {
        let paths = temp_paths("start");
        let mut state = TelegramState::empty();
        let message = text_message(42, None, "/start");

        let reply = handle_message(&paths, &mut state, message, handle_options())
            .unwrap()
            .unwrap();

        assert_eq!(reply.chat_id, 42);
        assert!(reply.text.contains("Bound route 42 to cx session"));
        assert_eq!(state.bindings.len(), 1);
        assert_eq!(session::list_sessions(&paths).unwrap().len(), 1);

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn release_reports_missing_binding() {
        let paths = temp_paths("release-missing");
        let mut state = TelegramState::empty();
        let message = text_message(42, None, "/release");

        let reply = handle_message(&paths, &mut state, message, handle_options())
            .unwrap()
            .unwrap();

        assert_eq!(
                reply.text,
                "No cx session is bound to route 42. Send /start to create the default session or /new <name> to create a named one."
            );

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn bind_command_trusts_matching_secret_for_unknown_chat() {
        let state = TelegramState::empty();
        let trusted = BTreeSet::new();
        let message = text_message(42, None, "/bind secret-123");

        let access = message_access(&message, &state, &trusted, Some("secret-123"), true);

        assert_eq!(access, MessageAccess::AuthorizedByBind);
    }

    #[test]
    fn bind_command_rejects_wrong_secret_for_unknown_chat() {
        let state = TelegramState::empty();
        let trusted = BTreeSet::new();
        let message = text_message(42, None, "/bind wrong");

        let access = message_access(&message, &state, &trusted, Some("secret-123"), true);

        assert_eq!(access, MessageAccess::DeniedBind);
    }

    #[test]
    fn bind_command_uses_chat_scope_for_forum_messages() {
        let message = TelegramMessage {
            chat: TelegramChat {
                id: -10042,
                is_forum: true,
            },
            message_id: 1,
            date: None,
            message_thread_id: Some(99),
            text: Some("/bind secret-123".to_string()),
            forum_topic_edited: None,
        };

        assert_eq!(
            trusted_route_for_message(&message),
            TelegramRoute {
                chat_id: -10042,
                message_thread_id: None
            }
        );
    }

    #[test]
    fn bind_command_does_not_create_session_binding() {
        let paths = temp_paths("bind-trust-only");
        let mut state = TelegramState::empty();
        let mut options = handle_options();
        options.authorized_by_bind = true;
        let message = text_message(42, None, "/bind secret-123");

        let reply = handle_message(&paths, &mut state, message, options)
            .unwrap()
            .unwrap();

        assert_eq!(reply.text, "Trusted Telegram route 42.");
        assert!(state.bindings.is_empty());
        assert!(session::list_sessions(&paths).unwrap().is_empty());

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn trusted_route_trusts_forum_topic_routes() {
        let mut state = TelegramState::empty();
        state.trust_route(&TelegramRoute {
            chat_id: -10042,
            message_thread_id: None,
        });
        let trusted = trusted_chats(&state, Vec::new());
        let message = text_message(-10042, Some(99), "/portal");

        let access = message_access(&message, &state, &trusted, None, true);

        assert_eq!(access, MessageAccess::Allowed);
    }

    #[test]
    fn existing_binding_is_trusted_without_allow_chat() {
        let mut state = TelegramState::empty();
        state.bindings.push(TelegramBinding {
            chat_id: 42,
            message_thread_id: None,
            alias: None,
            channel_id: ChannelId::parse("telegram:42").unwrap(),
            session_id: SessionId::parse("sess_manual").unwrap(),
            app_thread_id: None,
            app_thread_title: None,
            app_thread_cwd: None,
            topic_title: None,
            panel_message_id: None,
            telegram_paused: false,
            topic_created_by_adapter: false,
            watch_proxy_offset: None,
            watch_rollout_offset: None,
            watch_activity: None,
            watch_status: None,
        });
        let trusted = trusted_chats(&state, Vec::new());

        assert!(trusted.contains(&TelegramRoute {
            chat_id: 42,
            message_thread_id: None
        }));
    }

    #[test]
    fn chat_level_binding_trusts_forum_topic_routes() {
        let mut state = TelegramState::empty();
        state.bindings.push(TelegramBinding {
            chat_id: -10042,
            message_thread_id: None,
            alias: None,
            channel_id: ChannelId::parse("telegram:-10042").unwrap(),
            session_id: SessionId::parse("sess_manual").unwrap(),
            app_thread_id: None,
            app_thread_title: None,
            app_thread_cwd: None,
            topic_title: None,
            panel_message_id: None,
            telegram_paused: false,
            topic_created_by_adapter: false,
            watch_proxy_offset: None,
            watch_rollout_offset: None,
            watch_activity: None,
            watch_status: None,
        });
        let trusted = trusted_chats(&state, Vec::new());
        let message = text_message(-10042, Some(99), "/new build");

        let access = message_access(&message, &state, &trusted, None, true);

        assert_eq!(access, MessageAccess::Allowed);
    }

    #[test]
    fn allow_chat_trusts_forum_topic_routes() {
        let state = TelegramState::empty();
        let trusted = trusted_chats(&state, vec![-10042]);
        let message = text_message(-10042, Some(99), "/new build");

        let access = message_access(&message, &state, &trusted, None, true);

        assert_eq!(access, MessageAccess::Allowed);
    }

    #[test]
    fn bot_commands_cover_supported_telegram_commands() {
        let commands = bot_commands()
            .iter()
            .map(|command| command.command)
            .collect::<Vec<_>>();

        assert_eq!(commands, vec!["start", "bind", "portal", "status"]);
    }

    #[test]
    fn topic_route_channel_id_includes_thread_id() {
        let route = TelegramRoute {
            chat_id: -10042,
            message_thread_id: Some(99),
        };

        assert_eq!(
            route.channel_id(Some("build")).unwrap().to_string(),
            "telegram:-10042:topic:99:session:build"
        );
    }

    #[test]
    fn topic_reply_preserves_message_thread_id() {
        let paths = temp_paths("topic-reply");
        let mut state = TelegramState::empty();
        let message = text_message(-10042, Some(99), "/new Build!");

        let reply = handle_message(&paths, &mut state, message, handle_options())
            .unwrap()
            .unwrap();

        assert_eq!(reply.chat_id, -10042);
        assert_eq!(reply.message_thread_id, Some(99));

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn new_command_creates_named_route_session_and_marks_active() {
        let paths = temp_paths("new");
        let mut state = TelegramState::empty();
        let message = text_message(42, None, "/new Build!");

        let reply = handle_message(&paths, &mut state, message, handle_options())
            .unwrap()
            .unwrap();

        assert!(reply.text.contains("Created session `build`"));
        assert!(state
            .binding_for_route(
                &TelegramRoute {
                    chat_id: 42,
                    message_thread_id: None
                },
                Some("build")
            )
            .is_some());
        assert_eq!(state.active_routes[0].alias.as_deref(), Some("build"));

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn use_command_switches_active_route_session() {
        let paths = temp_paths("use");
        let mut state = TelegramState::empty();
        let route = TelegramRoute {
            chat_id: 42,
            message_thread_id: None,
        };
        state.bind_route(&paths, &route, Some("alpha")).unwrap();
        state.bind_route(&paths, &route, Some("beta")).unwrap();

        let reply = handle_message(
            &paths,
            &mut state,
            text_message(42, None, "/use alpha"),
            handle_options(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(reply.text, "Using session `alpha`.");
        assert_eq!(state.active_routes[0].alias.as_deref(), Some("alpha"));

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn close_command_unbinds_named_route_session() {
        let paths = temp_paths("close");
        let mut state = TelegramState::empty();
        let route = TelegramRoute {
            chat_id: 42,
            message_thread_id: None,
        };
        state.bind_route(&paths, &route, Some("alpha")).unwrap();

        let reply = handle_message(
            &paths,
            &mut state,
            text_message(42, None, "/close alpha"),
            handle_options(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(reply.text, "Unbound session `alpha`.");
        assert!(state.binding_for_route(&route, Some("alpha")).is_none());

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn update_view_accepts_channel_posts() {
        let update = TelegramUpdate {
            update_id: 7,
            message: None,
            edited_message: None,
            channel_post: Some(TelegramMessage {
                chat: TelegramChat {
                    id: -10042,
                    is_forum: false,
                },
                message_id: 1,
                date: None,
                message_thread_id: Some(7),
                text: Some(String::from("/start")),
                forum_topic_edited: None,
            }),
            edited_channel_post: None,
            callback_query: None,
            my_chat_member: None,
        };

        let view = update.view();

        assert_eq!(view.source, TelegramUpdateSource::ChannelPost);
        assert_eq!(view.chat_id, Some(-10042));
        assert_eq!(
            view.message.and_then(|message| message.text),
            Some(String::from("/start"))
        );
    }

    #[test]
    fn update_view_reports_membership_updates_without_message() {
        let update = TelegramUpdate {
            update_id: 8,
            message: None,
            edited_message: None,
            channel_post: None,
            edited_channel_post: None,
            callback_query: None,
            my_chat_member: Some(TelegramChatMemberUpdated {
                chat: TelegramChat {
                    id: 42,
                    is_forum: false,
                },
            }),
        };

        let view = update.view();

        assert_eq!(view.source, TelegramUpdateSource::MyChatMember);
        assert_eq!(view.chat_id, Some(42));
        assert!(view.message.is_none());
    }

    #[test]
    fn telegram_chat_deserializes_with_is_forum() {
        let json = r#"{"id": -1003586916929, "is_forum": true}"#;
        let chat: TelegramChat = serde_json::from_str(json).unwrap();

        assert_eq!(chat.id, -1003586916929);
        assert!(chat.is_forum);
    }

    #[test]
    fn telegram_chat_defaults_is_forum_to_false() {
        let json = r#"{"id": 42}"#;
        let chat: TelegramChat = serde_json::from_str(json).unwrap();

        assert_eq!(chat.id, 42);
        assert!(!chat.is_forum);
    }

    #[test]
    fn new_in_forum_creates_session_without_api_when_no_notifier() {
        let paths = temp_paths("new-forum-nonotif");
        let mut state = TelegramState::empty();
        let message = TelegramMessage {
            chat: TelegramChat {
                id: -10042,
                is_forum: true,
            },
            message_id: 1,
            date: None,
            message_thread_id: None,
            text: Some("/new Build!".to_string()),
            forum_topic_edited: None,
        };

        let reply = handle_message(&paths, &mut state, message, handle_options())
            .unwrap()
            .unwrap();

        assert!(reply.text.contains("Created session `build`"));
        assert!(reply.text.contains("route"));
        assert_eq!(state.bindings.len(), 1);
        assert_eq!(state.bindings[0].chat_id, -10042);
        assert_eq!(state.bindings[0].message_thread_id, None);
        assert_eq!(state.bindings[0].alias.as_deref(), Some("build"));
        assert!(!state.bindings[0].topic_created_by_adapter);

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn new_inside_forum_topic_creates_session_within_topic() {
        let paths = temp_paths("new-topic");
        let mut state = TelegramState::empty();
        let message = TelegramMessage {
            chat: TelegramChat {
                id: -10042,
                is_forum: true,
            },
            message_id: 1,
            date: None,
            message_thread_id: Some(99),
            text: Some("/new config".to_string()),
            forum_topic_edited: None,
        };

        let reply = handle_message(&paths, &mut state, message, handle_options())
            .unwrap()
            .unwrap();

        assert!(reply.text.contains("Created session `config`"));
        assert_eq!(state.bindings.len(), 1);
        assert_eq!(state.bindings[0].chat_id, -10042);
        assert_eq!(state.bindings[0].message_thread_id, Some(99));
        assert_eq!(state.bindings[0].alias.as_deref(), Some("config"));

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn close_topic_binding_removes_session() {
        let paths = temp_paths("close-topic");
        let mut state = TelegramState::empty();
        let route = TelegramRoute {
            chat_id: -10042,
            message_thread_id: Some(99),
        };
        state.bind_route(&paths, &route, Some("smoke")).unwrap();
        assert!(state.binding_for_route(&route, Some("smoke")).is_some());

        let reply = handle_message(
            &paths,
            &mut state,
            TelegramMessage {
                chat: TelegramChat {
                    id: -10042,
                    is_forum: true,
                },
                message_id: 1,
                date: None,
                message_thread_id: Some(99),
                text: Some("/close smoke".to_string()),
                forum_topic_edited: None,
            },
            handle_options(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(reply.text, "Unbound session `smoke`.");
        assert!(state.binding_for_route(&route, Some("smoke")).is_none());

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn close_topic_binding_does_not_panic_without_notifier() {
        let paths = temp_paths("close-topic-nonotif");
        let mut state = TelegramState::empty();
        let route = TelegramRoute {
            chat_id: -10042,
            message_thread_id: Some(99),
        };
        state.bind_route(&paths, &route, None).unwrap();

        let reply = handle_message(
            &paths,
            &mut state,
            TelegramMessage {
                chat: TelegramChat {
                    id: -10042,
                    is_forum: true,
                },
                message_id: 1,
                date: None,
                message_thread_id: Some(99),
                text: Some("/close".to_string()),
                forum_topic_edited: None,
            },
            handle_options(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(reply.text, "Unbound session `default`.");
        assert!(state.binding_for_route(&route, None).is_none());

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn close_without_alias_unbinds_active_named_session() {
        let paths = temp_paths("close-active-named");
        let mut state = TelegramState::empty();
        let route = TelegramRoute {
            chat_id: -10042,
            message_thread_id: Some(31),
        };
        state.bind_route(&paths, &route, Some("session-2")).unwrap();

        let reply = handle_message(
            &paths,
            &mut state,
            TelegramMessage {
                chat: TelegramChat {
                    id: -10042,
                    is_forum: true,
                },
                message_id: 1,
                date: None,
                message_thread_id: Some(31),
                text: Some("/close".to_string()),
                forum_topic_edited: None,
            },
            handle_options(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(reply.text, "Unbound session `session-2`.");
        assert!(state.binding_for_route(&route, Some("session-2")).is_none());

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn close_unbinds_local_route_without_archiving_app_thread() {
        let paths = temp_paths("close-preserve-app-thread");
        let mut state = TelegramState::empty();
        let route = TelegramRoute {
            chat_id: -10042,
            message_thread_id: Some(31),
        };
        state.bind_route(&paths, &route, None).unwrap();
        state
            .binding_for_route_mut(&route, None)
            .unwrap()
            .app_thread_id = Some("thread-missing".to_string());

        let reply = handle_message(
            &paths,
            &mut state,
            TelegramMessage {
                chat: TelegramChat {
                    id: -10042,
                    is_forum: true,
                },
                message_id: 1,
                date: None,
                message_thread_id: Some(31),
                text: Some("/close".to_string()),
                forum_topic_edited: None,
            },
            handle_options(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(reply.text, "Unbound session `default`.");
        assert!(state.binding_for_route(&route, None).is_none());

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn deleting_topic_route_removes_all_local_bindings_for_that_topic() {
        let paths = temp_paths("remove-topic-route");
        let mut state = TelegramState::empty();
        let route = TelegramRoute {
            chat_id: -10042,
            message_thread_id: Some(31),
        };
        state.bind_route(&paths, &route, None).unwrap();
        state.bind_route(&paths, &route, Some("alpha")).unwrap();
        state.set_active_route(&route, Some("alpha"));

        assert!(state.remove_all_route_bindings(&route));

        assert!(state.bindings.is_empty());
        assert!(state.active_routes.is_empty());

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn portal_callback_data_parses_watch() {
        assert_eq!(
            TelegramCallbackCommand::parse("cx:p"),
            Some(TelegramCallbackCommand::RefreshPortal)
        );
        assert_eq!(
            TelegramCallbackCommand::parse("cx:w"),
            Some(TelegramCallbackCommand::RefreshWork)
        );
        assert_eq!(
            TelegramCallbackCommand::parse("cx:t"),
            Some(TelegramCallbackCommand::Takeover)
        );
        assert_eq!(
            TelegramCallbackCommand::parse("cx:rel"),
            Some(TelegramCallbackCommand::Release)
        );
        assert_eq!(
            TelegramCallbackCommand::parse("cx:c"),
            Some(TelegramCallbackCommand::Close)
        );
        assert_eq!(
            TelegramCallbackCommand::parse("cx:a:thread-123"),
            Some(TelegramCallbackCommand::Observe {
                thread_id: "thread-123".to_string()
            })
        );
        assert_eq!(
            TelegramCallbackCommand::parse("cx:o:thread-456"),
            Some(TelegramCallbackCommand::Observe {
                thread_id: "thread-456".to_string()
            })
        );
        assert_eq!(TelegramCallbackCommand::parse("noop"), None);
    }

    #[test]
    fn app_server_threads_are_watchable_even_when_not_loaded() {
        let entry = AppThreadPortalEntry::from(AppThreadSummary {
            upstream_thread_id: "thread-not-loaded".to_string(),
            title: None,
            preview: "Idle desktop thread".to_string(),
            cwd: "/Users/yupeit/dev/cx".to_string(),
            source: "cli".to_string(),
            active: false,
            status: "notLoaded".to_string(),
            created_at_unix: 1,
            updated_at_unix: 2,
        });

        assert!(entry.watchable);
        assert!(!entry.active);
        assert_eq!(entry.status, "notLoaded");
    }

    #[test]
    fn portal_keyboard_uses_watch_callbacks() {
        let entries = vec![AppThreadPortalEntry {
            thread_id: "019dfeec-78ca-7cb0-a497-cd3a79f1329a".to_string(),
            title: Some("Fix Telegram handoff".to_string()),
            preview: String::new(),
            cwd: "/Users/yupeit/dev/cx".to_string(),
            active: true,
            watchable: true,
            status: "active".to_string(),
        }];

        let keyboard = portal_keyboard(&entries);

        assert_eq!(keyboard.inline_keyboard.len(), 2);
        assert_eq!(keyboard.inline_keyboard[0].len(), 1);
        assert_eq!(
            keyboard.inline_keyboard[0][0].callback_data,
            "cx:o:019dfeec-78ca-7cb0-a497-cd3a79f1329a"
        );
        assert!(keyboard.inline_keyboard[0][0].text.starts_with("Watch:"));
        assert_eq!(keyboard.inline_keyboard[1][0].text, "Refresh");
        assert_eq!(keyboard.inline_keyboard[1][0].callback_data, "cx:p");
    }

    #[test]
    fn portal_keyboard_offers_watch_for_inactive_threads() {
        let entries = vec![AppThreadPortalEntry {
            thread_id: "thread-idle".to_string(),
            title: Some("Idle thread".to_string()),
            preview: String::new(),
            cwd: "/Users/yupeit/dev/cx".to_string(),
            active: false,
            watchable: false,
            status: "idle".to_string(),
        }];

        let keyboard = portal_keyboard(&entries);

        assert_eq!(keyboard.inline_keyboard[0].len(), 1);
        assert_eq!(
            keyboard.inline_keyboard[0][0].callback_data,
            "cx:o:thread-idle"
        );
        assert!(keyboard.inline_keyboard[0][0].text.starts_with("Watch:"));
    }

    #[test]
    fn portal_keyboard_offers_watch_for_watchable_tui_threads() {
        let entries = vec![AppThreadPortalEntry {
            thread_id: "thread-tui-active".to_string(),
            title: Some("Running in TUI".to_string()),
            preview: String::new(),
            cwd: "/Users/yupeit/dev/cx".to_string(),
            active: false,
            watchable: true,
            status: "active-tui".to_string(),
        }];

        let keyboard = portal_keyboard(&entries);

        assert_eq!(keyboard.inline_keyboard[0].len(), 1);
        assert!(keyboard.inline_keyboard[0][0].text.starts_with("Watch:"));
        assert_eq!(
            keyboard.inline_keyboard[0][0].callback_data,
            "cx:o:thread-tui-active"
        );
    }

    #[test]
    fn filter_portal_entries_keeps_registry_order_and_limits() {
        let entries = vec![
            AppThreadPortalEntry {
                thread_id: "thread-first".to_string(),
                title: Some("First".to_string()),
                preview: String::new(),
                cwd: "/Users/yupeit/dev/cx".to_string(),
                active: false,
                watchable: false,
                status: "notLoaded".to_string(),
            },
            AppThreadPortalEntry {
                thread_id: "thread-second".to_string(),
                title: Some("Second".to_string()),
                preview: String::new(),
                cwd: "/Users/yupeit/dev/cx".to_string(),
                active: true,
                watchable: true,
                status: "active".to_string(),
            },
            AppThreadPortalEntry {
                thread_id: "thread-third".to_string(),
                title: Some("Third".to_string()),
                preview: String::new(),
                cwd: "/Users/yupeit/dev/cx".to_string(),
                active: false,
                watchable: false,
                status: "notLoaded".to_string(),
            },
        ];

        let filtered = filter_portal_entries(entries, 2);

        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].thread_id, "thread-first");
        assert_eq!(filtered[1].thread_id, "thread-second");
    }

    #[test]
    fn filter_portal_entries_filters_local_watch_regression_threads() {
        let entries = vec![
            AppThreadPortalEntry {
                thread_id: "thread-test".to_string(),
                title: Some("For Telegram watch E2E marker CXE2E".to_string()),
                preview: String::new(),
                cwd: "/Users/yupeit/dev/cx".to_string(),
                active: false,
                watchable: true,
                status: "open-tui".to_string(),
            },
            AppThreadPortalEntry {
                thread_id: "thread-real".to_string(),
                title: Some("Fix Telegram close semantics".to_string()),
                preview: String::new(),
                cwd: "/Users/yupeit/dev/cx".to_string(),
                active: true,
                watchable: true,
                status: "active".to_string(),
            },
        ];

        let filtered = filter_portal_entries(entries, 10);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].thread_id, "thread-real");
    }

    #[test]
    fn loopback_listen_urls_accepts_only_loopback_addresses() {
        let urls = loopback_listen_urls(
                "p1\nn127.0.0.1:1234\nnlocalhost:4567\nn[::1]:2345\nn0.0.0.0:9999\nn127.0.0.1:not-a-port\nn127.0.0.1:1234\n",
            );

        assert_eq!(
            urls,
            vec![
                "ws://127.0.0.1:1234",
                "ws://127.0.0.1:2345",
                "ws://127.0.0.1:4567",
            ]
        );
    }

    #[test]
    fn rollout_task_events_track_active_turn_lifecycle() {
        let mut active_turns = Vec::<(String, u64)>::new();
        apply_rollout_task_event(
            &mut active_turns,
            rollout_task_event(
                r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
            ),
            42,
        );
        apply_rollout_task_event(
            &mut active_turns,
            rollout_task_event(
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"working"}}"#,
            ),
            84,
        );
        assert_eq!(active_turns, vec![("turn-1".to_string(), 42)]);

        apply_rollout_task_event(
            &mut active_turns,
            rollout_task_event(
                r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
            ),
            126,
        );
        assert!(active_turns.is_empty());
    }

    #[test]
    fn rollout_task_events_accept_turn_aliases_and_abort() {
        let mut active_turns = Vec::<(String, u64)>::new();
        apply_rollout_task_event(
            &mut active_turns,
            rollout_task_event(
                r#"{"type":"event_msg","payload":{"type":"turn_started","turn_id":"turn-2"}}"#,
            ),
            7,
        );
        assert_eq!(active_turns, vec![("turn-2".to_string(), 7)]);

        apply_rollout_task_event(
            &mut active_turns,
            rollout_task_event(
                r#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"turn-2","reason":"interrupted"}}"#,
            ),
            8,
        );
        assert!(active_turns.is_empty());
    }

    #[test]
    fn rollout_task_events_accept_turn_context_start() {
        assert_eq!(
            rollout_task_event(
                r#"{"type":"turn_context","payload":{"turn_id":"turn-context-1","cwd":"/tmp"}}"#,
            ),
            Some(RolloutTaskEvent::Started("turn-context-1".to_string()))
        );
    }

    #[test]
    fn rollout_path_for_thread_keeps_archived_active_rollouts_visible() {
        let paths = temp_paths("rollout-archived-path");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        let rollout = paths.base_codex_home.join("archived-rollout.jsonl");
        let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
        conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0)",
                [],
            )
            .unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 1)",
            params!["thread-archived-active", rollout.to_string_lossy().as_ref()],
        )
        .unwrap();

        assert_eq!(
            rollout_path_for_thread(&paths, "thread-archived-active"),
            Some(rollout)
        );
    }

    #[test]
    fn open_tui_portal_entries_ignores_archived_idle_rollouts() {
        let paths = temp_paths("rollout-archived-idle");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        let rollout = paths.base_codex_home.join("archived-idle-rollout.jsonl");
        fs::write(
            &rollout,
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
        )
        .unwrap();
        let _held = fs::File::open(&rollout).unwrap();
        let Some(pid) = pid_holding_file(&rollout) else {
            let _ = fs::remove_dir_all(paths.base_codex_home);
            return;
        };
        if !pid_listening_urls(pid).is_empty() {
            let _ = fs::remove_dir_all(paths.base_codex_home);
            return;
        }
        let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
        conn.execute(
            "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    title TEXT NOT NULL,
                    first_user_message TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    updated_at INTEGER NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0
                )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (
                    id,
                    rollout_path,
                    title,
                    first_user_message,
                    cwd,
                    updated_at,
                    archived
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "thread-archived-idle",
                rollout.to_string_lossy().as_ref(),
                "Archived idle",
                "Archived idle",
                "/tmp",
                1_i64,
                1_i64
            ],
        )
        .unwrap();

        let entries = open_tui_portal_entries(&paths, 10).unwrap();

        assert!(entries.is_empty());
        let _ = fs::remove_dir_all(paths.base_codex_home);
    }

    #[test]
    fn active_rollout_turn_requires_active_turn_proof_from_listener_holder() {
        let paths = temp_paths("rollout-listener-holder");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        let rollout = paths.base_codex_home.join("listener-holder-rollout.jsonl");
        fs::write(
            &rollout,
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
        )
        .unwrap();
        let _held = fs::File::open(&rollout).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let accept_thread = std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let Some(pid) = pid_holding_file(&rollout) else {
            let _ = fs::remove_dir_all(paths.base_codex_home);
            return;
        };
        if pid_listening_urls(pid).is_empty() {
            let _ = fs::remove_dir_all(paths.base_codex_home);
            return;
        }
        let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
        conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0)",
                [],
            )
            .unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 0)",
            params!["thread-listener-holder", rollout.to_string_lossy().as_ref()],
        )
        .unwrap();

        let active = active_rollout_turn(&paths, "thread-listener-holder").unwrap();

        assert!(active.is_none());
        let _ = accept_thread.join();
        let _ = fs::remove_dir_all(paths.base_codex_home);
    }

    #[test]
    fn open_tui_portal_entries_keeps_archived_active_rollouts_visible() {
        let paths = temp_paths("rollout-archived-active");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        let rollout = paths.base_codex_home.join("archived-active-rollout.jsonl");
        fs::write(
            &rollout,
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
        )
        .unwrap();
        let _held = fs::File::open(&rollout).unwrap();
        let Some(pid) = pid_holding_file(&rollout) else {
            let _ = fs::remove_dir_all(paths.base_codex_home);
            return;
        };
        if !pid_listening_urls(pid).is_empty() {
            let _ = fs::remove_dir_all(paths.base_codex_home);
            return;
        }
        let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
        conn.execute(
            "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    title TEXT NOT NULL,
                    first_user_message TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    updated_at INTEGER NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0
                )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (
                    id,
                    rollout_path,
                    title,
                    first_user_message,
                    cwd,
                    updated_at,
                    archived
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "thread-archived-active",
                rollout.to_string_lossy().as_ref(),
                "Archived active",
                "Archived active",
                "/tmp",
                1_i64,
                1_i64
            ],
        )
        .unwrap();

        let entries = open_tui_portal_entries(&paths, 10).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].thread_id, "thread-archived-active");
        assert_eq!(entries[0].status, "active-tui");
        let _ = fs::remove_dir_all(paths.base_codex_home);
    }

    #[test]
    fn activity_watch_text_renders_concise_running_command() {
        let mut panel = TelegramActivityPanel::new();
        panel.apply_execution(CommandExecution {
            item_id: "cmd-1".to_string(),
            command: "sed -n '1,20p' src/channel.rs".to_string(),
            cwd: "/tmp/project".to_string(),
            activity: Some(CommandActivity {
                verb: "Read".to_string(),
                target: "channel.rs".to_string(),
            }),
            status: CommandExecutionStatus::InProgress,
            exit_code: None,
            duration_ms: None,
            aggregated_output: Some("long command output that should not be sent".to_string()),
        });

        let text = activity_watch_text(&panel);

        assert!(text.contains("• **Exploring**"));
        assert!(text.contains("Read `channel.rs`"));
        assert!(!text.contains("Running:"));
        assert!(!text.contains("cwd:"));
        assert!(!text.contains("output"));
        assert!(!text.contains("long command output"));
    }

    #[test]
    fn activity_watch_text_renders_completion_metadata() {
        let mut panel = TelegramActivityPanel::new();
        panel.apply_execution(CommandExecution {
            item_id: "cmd-1".to_string(),
            command: "cargo test".to_string(),
            cwd: "/tmp/project".to_string(),
            activity: None,
            status: CommandExecutionStatus::Completed,
            exit_code: Some(0),
            duration_ms: Some(1234),
            aggregated_output: Some("ok\n".to_string()),
        });

        let text = activity_watch_text(&panel);

        assert!(text.contains("• **Ran** `cargo test` (1.2s)"));
        assert!(text.contains("  └ `ok`"));
        assert!(!text.contains("Done:"));
        assert!(!text.contains("exit: 0"));
    }

    #[test]
    fn activity_watch_text_groups_transcript_sections() {
        let mut panel = TelegramActivityPanel::new();
        panel.apply_execution(CommandExecution {
            item_id: "read-1".to_string(),
            command: "sed -n '1,80p' wiley.py".to_string(),
            cwd: "/tmp/project".to_string(),
            activity: Some(CommandActivity {
                verb: "Read".to_string(),
                target: "wiley.py, science.py, pnas.py".to_string(),
            }),
            status: CommandExecutionStatus::Completed,
            exit_code: Some(0),
            duration_ms: Some(0),
            aggregated_output: None,
        });
        panel.apply_execution(CommandExecution {
            item_id: "test-1".to_string(),
            command: "cargo test".to_string(),
            cwd: "/tmp/project".to_string(),
            activity: None,
            status: CommandExecutionStatus::Completed,
            exit_code: Some(0),
            duration_ms: Some(1240),
            aggregated_output: None,
        });

        assert_eq!(
            activity_watch_text(&panel),
            "• **Explored**\n  └ Read `wiley.py`, `science.py`, `pnas.py`\n• **Ran** `cargo test` (1.2s)"
        );
    }

    #[test]
    fn activity_watch_text_merges_adjacent_explore_items_like_tui() {
        let mut panel = TelegramActivityPanel::new();
        for (item_id, verb, target) in [
            ("read-skill", "Read", "SKILL.md (rust-systems-style skill)"),
            ("read-telegram", "Read", "telegram.rs"),
            (
                "search-telegram",
                "Search",
                "paused|telegram_paused|AcquireLease in telegram.rs",
            ),
            ("read-client", "Read", "telegram.rs, client.rs"),
        ] {
            panel.apply_execution(CommandExecution {
                item_id: item_id.to_string(),
                command: "tool call".to_string(),
                cwd: "/tmp/project".to_string(),
                activity: Some(CommandActivity {
                    verb: verb.to_string(),
                    target: target.to_string(),
                }),
                status: CommandExecutionStatus::Completed,
                exit_code: Some(0),
                duration_ms: Some(0),
                aggregated_output: None,
            });
        }

        let text = activity_watch_text(&panel);

        assert_eq!(text.matches("• **Explored**").count(), 1);
        assert_eq!(
            text,
            "• **Explored**\n  └ Read `SKILL.md` (rust-systems-style skill), `telegram.rs`\n    Search `paused|telegram_paused|AcquireLease` in `telegram.rs`\n    Read `telegram.rs`, `client.rs`"
        );
    }

    #[test]
    fn activity_panel_state_preserves_edit_target_across_drains() {
        let mut panel = TelegramActivityPanel::new();
        panel.apply_execution(CommandExecution {
            item_id: "read-1".to_string(),
            command: "sed -n '1,80p' telegram.rs".to_string(),
            cwd: "/tmp/project".to_string(),
            activity: Some(CommandActivity {
                verb: "Read".to_string(),
                target: "telegram.rs".to_string(),
            }),
            status: CommandExecutionStatus::Completed,
            exit_code: Some(0),
            duration_ms: Some(0),
            aggregated_output: None,
        });
        panel.message_id = Some(42);
        panel.last_sent_text = Some(activity_watch_text(&panel));

        let restored = TelegramActivityPanel::from_state(panel.to_state());

        assert_eq!(restored.message_id, Some(42));
        assert_eq!(
            activity_watch_text(&restored),
            "• **Explored**\n  └ Read `telegram.rs`"
        );
    }

    #[test]
    fn activity_watch_text_renders_file_changes_like_tui() {
        let mut panel = TelegramActivityPanel::new();
        panel.apply_execution(CommandExecution {
            item_id: "patch-1".to_string(),
            command: "apply patch".to_string(),
            cwd: "/tmp/project".to_string(),
            activity: Some(CommandActivity {
                verb: "Edited".to_string(),
                target: "src/channel/telegram.rs (+12 -3)".to_string(),
            }),
            status: CommandExecutionStatus::Completed,
            exit_code: None,
            duration_ms: None,
            aggregated_output: None,
        });

        assert_eq!(
            activity_watch_text(&panel),
            "• **Edited** `src/channel/telegram.rs` (+12 -3)"
        );
    }

    #[test]
    fn rollout_observe_event_reads_update_plan_as_activity() {
        let event = rollout_observe_event(
            r#"{"type":"response_item","payload":{"type":"function_call","name":"update_plan","arguments":"{\"explanation\":\"Converging on the TUI transcript model.\",\"plan\":[{\"step\":\"Mirror activity cells\",\"status\":\"completed\"},{\"step\":\"Ship release\",\"status\":\"in_progress\"}]}","call_id":"plan-1"}}"#,
        );

        assert_eq!(
            event,
            Some(RolloutObserveEvent::Stream(AppStreamEvent::CommandCompleted(
                CommandExecution {
                    item_id: "plan-1".to_string(),
                    command: "update_plan".to_string(),
                    cwd: String::new(),
                    activity: Some(CommandActivity {
                        verb: "Plan".to_string(),
                        target: "Converging on the TUI transcript model.\n✔ ~~Mirror activity cells~~\n□ Ship release".to_string(),
                    }),
                    status: CommandExecutionStatus::Completed,
                    exit_code: None,
                    duration_ms: None,
                    aggregated_output: None,
                }
            )))
        );
    }

    #[test]
    fn thinking_watch_text_marks_active_and_done_states() {
        let mut panel = TelegramThinkingPanel::new();
        panel.start();

        assert_eq!(thinking_watch_text(&panel), "Codex is working\n");

        panel.push("Checking current state.");
        panel.finish();

        assert_eq!(
            thinking_watch_text(&panel),
            "Codex\nChecking current state."
        );
    }

    #[test]
    fn thinking_panel_does_not_send_empty_working_placeholder() {
        let target = CapturingTranscriptTarget::default();
        let mut panel = TelegramThinkingPanel::new();
        panel.start();
        panel.finish();

        assert!(!panel.flush(&target, true).unwrap());
        assert!(target.sent.borrow().is_empty());
        assert!(target.edited.borrow().is_empty());
    }

    #[test]
    fn rollout_observe_event_reads_context_compaction_as_info_cell() {
        assert_eq!(
            rollout_observe_event(r#"{"type":"compacted","payload":{"message":""}}"#),
            Some(RolloutObserveEvent::Stream(AppStreamEvent::Info(
                "Context compacted".to_string()
            )))
        );
        assert_eq!(
            rollout_observe_event(r#"{"type":"event_msg","payload":{"type":"context_compacted"}}"#,),
            Some(RolloutObserveEvent::Stream(AppStreamEvent::Info(
                "Context compacted".to_string()
            )))
        );
    }

    #[test]
    fn rollout_observe_event_reads_agent_and_terminal_messages() {
        assert_eq!(
            rollout_observe_event(
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"please inspect this"}}"#,
            ),
            Some(RolloutObserveEvent::Stream(AppStreamEvent::UserMessage(
                "please inspect this".to_string()
            )))
        );
        assert_eq!(
            rollout_observe_event(
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"hello","phase":"commentary"}}"#,
            ),
            Some(RolloutObserveEvent::Stream(AppStreamEvent::ReasoningDelta(
                "hello".to_string()
            )))
        );
        assert_eq!(
            rollout_observe_event(
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"done","phase":"final_answer"}}"#,
            ),
            Some(RolloutObserveEvent::Stream(AppStreamEvent::AgentDelta(
                "done".to_string()
            )))
        );
        assert_eq!(
            rollout_observe_event(
                r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","last_agent_message":"done"}}"#,
            ),
            Some(RolloutObserveEvent::Terminal {
                turn_id: Some("turn-1".to_string()),
                terminal: ObserveTerminal::Completed,
                last_agent_message: Some("done".to_string()),
            })
        );
    }

    #[test]
    fn rollout_observe_event_reads_reasoning_summary() {
        let event = rollout_observe_event(
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"Checking the weather API"}],"content":null,"encrypted_content":null}}"#,
        );

        assert_eq!(
            event,
            Some(RolloutObserveEvent::Stream(AppStreamEvent::ReasoningDelta(
                "Checking the weather API".to_string()
            )))
        );
    }

    #[test]
    fn rollout_observe_event_reads_command_execution_events() {
        let started = rollout_observe_event(
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"echo hi\",\"workdir\":\"/tmp\"}","call_id":"call-1"}}"#,
        );
        assert_eq!(
            started,
            Some(RolloutObserveEvent::Stream(AppStreamEvent::CommandStarted(
                CommandExecution {
                    item_id: "call-1".to_string(),
                    command: "echo hi".to_string(),
                    cwd: "/tmp".to_string(),
                    activity: None,
                    status: CommandExecutionStatus::InProgress,
                    exit_code: None,
                    duration_ms: None,
                    aggregated_output: None,
                }
            )))
        );

        let output = rollout_observe_event(concat!(
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"#,
            r#""Chunk ID: abc\nOutput:\nhi\n"}}"#
        ));
        assert_eq!(
            output,
            Some(RolloutObserveEvent::Stream(
                AppStreamEvent::CommandOutputDelta {
                    item_id: "call-1".to_string(),
                    delta: "hi\n".to_string(),
                }
            ))
        );

        let completed = rollout_observe_event(
            r#"{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"call-1","turn_id":"turn-1","command":["/bin/zsh","-lc","sed -n '1,20p' src/channel.rs"],"cwd":"/tmp","parsed_cmd":[{"type":"read","cmd":"sed -n '1,20p' src/channel.rs","name":"channel.rs","path":"src/channel.rs"}],"exit_code":0,"duration":{"secs":1,"nanos":250000000},"status":"completed"}}"#,
        );
        assert_eq!(
            completed,
            Some(RolloutObserveEvent::Stream(
                AppStreamEvent::CommandCompleted(CommandExecution {
                    item_id: "call-1".to_string(),
                    command: "sed -n '1,20p' src/channel.rs".to_string(),
                    cwd: "/tmp".to_string(),
                    activity: Some(CommandActivity {
                        verb: "Read".to_string(),
                        target: "channel.rs".to_string(),
                    }),
                    status: CommandExecutionStatus::Completed,
                    exit_code: Some(0),
                    duration_ms: Some(1250),
                    aggregated_output: None,
                })
            ))
        );
    }

    #[test]
    fn rollout_observe_event_reads_custom_apply_patch_as_file_change() {
        let started = rollout_observe_event(
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"patch-1","name":"apply_patch","input":"*** Begin Patch\n*** Update File: src/channel/telegram.rs\n@@\n-old\n+new\n+another\n*** End Patch"}}"#,
        );

        assert_eq!(
            started,
            Some(RolloutObserveEvent::Stream(AppStreamEvent::CommandStarted(
                CommandExecution {
                    item_id: "patch-1".to_string(),
                    command: "apply_patch".to_string(),
                    cwd: String::new(),
                    activity: Some(CommandActivity {
                        verb: "Edited".to_string(),
                        target: "src/channel/telegram.rs (+2 -1)".to_string(),
                    }),
                    status: CommandExecutionStatus::InProgress,
                    exit_code: None,
                    duration_ms: None,
                    aggregated_output: None,
                }
            )))
        );

        let output = rollout_observe_event(
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"patch-1","output":"{\"output\":\"Success. Updated the following files:\\nM src/channel/telegram.rs\\n\",\"metadata\":{\"exit_code\":0}}"}}"#,
        );

        assert_eq!(
            output,
            Some(RolloutObserveEvent::Stream(
                AppStreamEvent::CommandOutputDelta {
                    item_id: "patch-1".to_string(),
                    delta: "Success. Updated the following files:\nM src/channel/telegram.rs\n"
                        .to_string(),
                }
            ))
        );
    }

    #[test]
    fn rollout_observe_event_reads_patch_apply_end_as_file_change() {
        let completed = rollout_observe_event(
            r#"{"type":"event_msg","payload":{"type":"patch_apply_end","call_id":"patch-1","turn_id":"turn-1","stdout":"Success. Updated the following files:\nM src/channel/telegram.rs\n","stderr":"","success":true,"changes":{"src/channel/telegram.rs":{"type":"update","unified_diff":"@@\n-old\n+new\n+another\n","move_path":null}},"status":"completed"}}"#,
        );

        assert_eq!(
            completed,
            Some(RolloutObserveEvent::Stream(
                AppStreamEvent::CommandCompleted(CommandExecution {
                    item_id: "patch-1".to_string(),
                    command: "apply_patch".to_string(),
                    cwd: String::new(),
                    activity: Some(CommandActivity {
                        verb: "Edited".to_string(),
                        target: "src/channel/telegram.rs (+2 -1)".to_string(),
                    }),
                    status: CommandExecutionStatus::Completed,
                    exit_code: None,
                    duration_ms: None,
                    aggregated_output: Some(
                        "Success. Updated the following files:\nM src/channel/telegram.rs\n"
                            .to_string()
                    ),
                })
            ))
        );
    }

    #[test]
    fn rollout_command_activity_summarizes_multiple_parsed_commands() {
        let completed = rollout_observe_event(
            r#"{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"call-1","turn_id":"turn-1","command":["/bin/zsh","-lc","sed -n '1,80p' wiley.py && sed -n '1,60p' science.py && sed -n '1,60p' pnas.py"],"cwd":"/tmp","parsed_cmd":[{"type":"read","cmd":"sed -n '1,80p' wiley.py","name":"wiley.py","path":"/tmp/wiley.py"},{"type":"read","cmd":"sed -n '1,60p' science.py","name":"science.py","path":"/tmp/science.py"},{"type":"read","cmd":"sed -n '1,60p' pnas.py","name":"pnas.py","path":"/tmp/pnas.py"}],"exit_code":0,"duration":{"secs":0,"nanos":0},"status":"completed"}}"#,
        );

        assert_eq!(
            completed,
            Some(RolloutObserveEvent::Stream(
                AppStreamEvent::CommandCompleted(CommandExecution {
                    item_id: "call-1".to_string(),
                    command: "sed -n '1,80p' wiley.py && sed -n '1,60p' science.py && sed -n '1,60p' pnas.py".to_string(),
                    cwd: "/tmp".to_string(),
                    activity: Some(CommandActivity {
                        verb: "Read".to_string(),
                        target: "wiley.py, science.py, pnas.py".to_string(),
                    }),
                    status: CommandExecutionStatus::Completed,
                    exit_code: Some(0),
                    duration_ms: Some(0),
                    aggregated_output: None,
                })
            ))
        );
    }

    #[test]
    fn rollout_command_activity_preserves_search_query() {
        let completed = rollout_observe_event(
            r#"{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"call-search","turn_id":"turn-1","command":["/bin/zsh","-lc","rg -n \"paused|telegram_paused|AcquireLease\" src/channel/telegram.rs"],"cwd":"/tmp","parsed_cmd":[{"type":"search","cmd":"rg -n \"paused|telegram_paused|AcquireLease\" src/channel/telegram.rs","query":"paused|telegram_paused|AcquireLease","path":"src/channel/telegram.rs"}],"exit_code":0,"duration":{"secs":0,"nanos":0},"status":"completed"}}"#,
        );

        assert_eq!(
            completed,
            Some(RolloutObserveEvent::Stream(
                AppStreamEvent::CommandCompleted(CommandExecution {
                    item_id: "call-search".to_string(),
                    command:
                        "rg -n \"paused|telegram_paused|AcquireLease\" src/channel/telegram.rs"
                            .to_string(),
                    cwd: "/tmp".to_string(),
                    activity: Some(CommandActivity {
                        verb: "Search".to_string(),
                        target: "paused|telegram_paused|AcquireLease in telegram.rs".to_string(),
                    }),
                    status: CommandExecutionStatus::Completed,
                    exit_code: Some(0),
                    duration_ms: Some(0),
                    aggregated_output: None,
                })
            ))
        );
    }

    #[test]
    fn rollout_command_activity_preserves_skill_read_label() {
        let completed = rollout_observe_event(
            r#"{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"call-skill","turn_id":"turn-1","command":["/bin/zsh","-lc","sed -n '1,80p' /Users/yupeit/dev/skills/skills/rust-systems-style/SKILL.md"],"cwd":"/tmp","parsed_cmd":[{"type":"read","cmd":"sed -n '1,80p' /Users/yupeit/dev/skills/skills/rust-systems-style/SKILL.md","name":"SKILL.md","path":"/Users/yupeit/dev/skills/skills/rust-systems-style/SKILL.md"}],"exit_code":0,"duration":{"secs":0,"nanos":0},"status":"completed"}}"#,
        );

        assert_eq!(
            completed,
            Some(RolloutObserveEvent::Stream(
                AppStreamEvent::CommandCompleted(CommandExecution {
                    item_id: "call-skill".to_string(),
                    command:
                        "sed -n '1,80p' /Users/yupeit/dev/skills/skills/rust-systems-style/SKILL.md"
                            .to_string(),
                    cwd: "/tmp".to_string(),
                    activity: Some(CommandActivity {
                        verb: "Read".to_string(),
                        target: "SKILL.md (rust-systems-style skill)".to_string(),
                    }),
                    status: CommandExecutionStatus::Completed,
                    exit_code: Some(0),
                    duration_ms: Some(0),
                    aggregated_output: None,
                })
            ))
        );
    }

    #[test]
    fn proxy_log_observe_event_reads_client_user_message() {
        let event = proxy_log_observe_event(
            r#"{"timestampUnixMs":1,"direction":"client_to_server","method":"turn/start","threadId":"thread-1","turnId":null,"message":{"id":7,"method":"turn/start","params":{"threadId":"thread-1","input":[{"type":"text","text":"please inspect this","text_elements":[]}],"summary":"auto"}}}"#,
            "thread-1",
        );

        assert_eq!(
            event,
            Some(RolloutObserveEvent::Stream(AppStreamEvent::UserMessage(
                "please inspect this".to_string()
            )))
        );
    }

    #[test]
    fn rollout_history_text_reads_recent_user_and_agent_messages() {
        let paths = temp_paths("rollout-history");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        let rollout = paths.base_codex_home.join("rollout.jsonl");
        fs::write(
                &rollout,
                concat!(
                    r#"{"type":"event_msg","payload":{"type":"user_message","message":"first question"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"agent_message","message":"first answer","phase":"final_answer"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"user_message","message":"second question"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"agent_message","message":"second answer","phase":"final_answer"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
                    "\n"
                ),
            )
            .unwrap();
        let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
        conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0)",
                [],
            )
            .unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 0)",
            params!["thread-history", rollout.to_string_lossy().as_ref()],
        )
        .unwrap();

        let text = rollout_history_text(&paths, "thread-history", 3)
            .unwrap()
            .unwrap();

        assert!(text.starts_with("Recent thread history:"));
        assert!(!text.contains("first question"));
        assert!(text.contains("Codex: first answer"));
        assert!(text.contains("User: second question"));
        assert!(text.contains("Codex: second answer"));
    }

    #[test]
    fn rollout_history_text_reads_task_complete_last_agent_message() {
        let paths = temp_paths("rollout-history-terminal");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        let rollout = paths.base_codex_home.join("rollout.jsonl");
        fs::write(
                &rollout,
                concat!(
                    r#"{"type":"event_msg","payload":{"type":"user_message","message":"question"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","last_agent_message":"final answer from terminal"}}"#,
                    "\n"
                ),
            )
            .unwrap();
        let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
        conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0)",
                [],
            )
            .unwrap();
        conn.execute(
            "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 0)",
            params![
                "thread-history-terminal",
                rollout.to_string_lossy().as_ref()
            ],
        )
        .unwrap();

        let text = rollout_history_text(&paths, "thread-history-terminal", 3)
            .unwrap()
            .unwrap();

        assert!(text.contains("Codex: final answer from terminal"));
    }

    #[test]
    fn rollout_events_since_reads_only_new_complete_lines() {
        let paths = temp_paths("rollout-drain");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        let rollout = paths.base_codex_home.join("rollout.jsonl");
        let first = r#"{"type":"event_msg","payload":{"type":"agent_message","message":"old"}}"#;
        let second = r#"{"type":"event_msg","payload":{"type":"agent_message","message":"new"}}"#;
        let partial =
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"partial"}"#;
        fs::write(&rollout, format!("{first}\n{second}\n{partial}")).unwrap();

        let drain = rollout_events_since(&rollout, (first.len() + 1) as u64, 10).unwrap();

        assert_eq!(drain.events.len(), 1);
        assert_eq!(
            drain.events[0],
            RolloutObserveEvent::Stream(AppStreamEvent::AgentDelta("new".to_string()))
        );
        assert_eq!(
            drain.next_offset,
            (first.len() + 1 + second.len() + 1) as u64
        );
    }

    #[test]
    fn rollout_history_item_ignores_empty_and_non_message_events() {
        assert_eq!(
            rollout_history_item(
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"  "}}"#,
            ),
            None
        );
        assert_eq!(
            rollout_history_item(
                r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
            ),
            None
        );
    }

    #[test]
    fn observe_active_rollout_streams_until_task_complete() {
        use std::io::Write as _;

        let paths = temp_paths("rollout-observe");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        let rollout = paths.base_codex_home.join("rollout.jsonl");
        fs::write(
            &rollout,
            concat!(
                r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-live"}}"#,
                "\n"
            ),
        )
        .unwrap();

        let writer_rollout = rollout.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let mut file = OpenOptions::new()
                .append(true)
                .open(writer_rollout)
                .unwrap();
            file.write_all(
                    concat!(
                        r#"{"type":"event_msg","payload":{"type":"agent_message","message":"done","phase":"final_answer"}}"#,
                        "\n",
                        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-live","last_agent_message":"done"}}"#,
                        "\n"
                    )
                    .as_bytes(),
                )
                .unwrap();
            file.flush().unwrap();
        });

        let mut messages = Vec::<String>::new();
        let start_offset = fs::metadata(&rollout).unwrap().len();
        let observed = observe_active_rollout(
            &rollout,
            "turn-live",
            start_offset,
            Duration::from_secs(2),
            |event| {
                if let AppStreamEvent::AgentDelta(message) = event {
                    messages.push(message);
                }
                Ok(())
            },
        )
        .unwrap();
        writer.join().unwrap();

        assert_eq!(messages, vec!["done".to_string()]);
        assert_eq!(
            observed,
            ObservedRolloutTerminal {
                turn_id: "turn-live".to_string(),
                terminal: ObserveTerminal::Completed,
            }
        );
    }

    #[test]
    fn observe_active_rollout_does_not_skip_events_written_after_scan() {
        use std::io::Write as _;

        let paths = temp_paths("rollout-observe-race");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        let rollout = paths.base_codex_home.join("rollout.jsonl");
        fs::write(
            &rollout,
            concat!(
                r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-live"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let start_offset = fs::metadata(&rollout).unwrap().len();
        fs::OpenOptions::new()
                .append(true)
                .open(&rollout)
                .unwrap()
                .write_all(
                    concat!(
                        r#"{"type":"event_msg","payload":{"type":"agent_message","message":"done","phase":"final_answer"}}"#,
                        "\n",
                        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-live","last_agent_message":"done"}}"#,
                        "\n"
                    )
                    .as_bytes(),
                )
                .unwrap();

        let mut messages = Vec::<String>::new();
        let observed = observe_active_rollout(
            &rollout,
            "turn-live",
            start_offset,
            Duration::from_secs(2),
            |event| {
                if let AppStreamEvent::AgentDelta(message) = event {
                    messages.push(message);
                }
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(messages, vec!["done".to_string()]);
        assert_eq!(observed.terminal, ObserveTerminal::Completed);
    }

    #[test]
    fn work_panel_keyboard_switches_between_takeover_and_release() {
        let paths = temp_paths("work-panel-keyboard");
        let mut state = TelegramState::empty();
        let route = TelegramRoute {
            chat_id: -10042,
            message_thread_id: Some(99),
        };
        state.bind_route(&paths, &route, None).unwrap();
        let active = state.active_binding_for_route(&route).unwrap().clone();

        let active_keyboard = work_panel_keyboard(&active, &route);

        assert_eq!(
            active_keyboard.inline_keyboard[0][0].text,
            "Release to desktop"
        );
        assert_eq!(
            active_keyboard.inline_keyboard[0][0].callback_data,
            "cx:rel"
        );

        state
            .binding_for_route_mut(&route, None)
            .unwrap()
            .telegram_paused = true;
        let paused = state.active_binding_for_route(&route).unwrap();
        let paused_keyboard = work_panel_keyboard(paused, &route);

        assert_eq!(
            paused_keyboard.inline_keyboard[0][0].text,
            "Take over from desktop"
        );
        assert_eq!(paused_keyboard.inline_keyboard[0][0].callback_data, "cx:t");

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn telegram_noop_errors_are_success_conditions() {
        assert!(is_telegram_noop_error(
                "Bad Request: message is not modified: specified new message content and reply markup are exactly the same as a current content and reply markup of the message"
            ));
        assert!(is_telegram_noop_error("Bad Request: TOPIC_NOT_MODIFIED"));
        assert!(!is_telegram_noop_error(
            "Bad Request: message to edit not found"
        ));
    }

    #[test]
    fn telegram_html_text_renders_code_and_escapes_plain_text() {
        assert_eq!(
            telegram_html_text("cwd: `/Users/yupeit/dev/cx` & <ok>"),
            "cwd: <code>/Users/yupeit/dev/cx</code> &amp; &lt;ok&gt;"
        );
        assert_eq!(
            telegram_html_text("• **Explored**\n  └ Read `src/main.rs`"),
            "• <b>Explored</b>\n  └ Read <code>src/main.rs</code>"
        );
        assert_eq!(
            telegram_html_text("```rust\nlet x = 1 < 2;\n```"),
            "<pre>let x = 1 &lt; 2;\n</pre>"
        );
        assert_eq!(telegram_html_text("unmatched `code"), "unmatched `code");
    }

    #[test]
    fn approval_callback_data_round_trips() {
        let nonce = "abc123";

        let data = approval_callback_data(nonce, TelegramApprovalDecision::AcceptForSession);

        assert_eq!(
            parse_approval_callback_data(&data, nonce),
            Some(TelegramApprovalDecision::AcceptForSession)
        );
        assert_eq!(parse_approval_callback_data(&data, "other"), None);
    }

    #[test]
    fn approval_response_maps_command_and_permissions_requests() {
        let command = ApprovalRequest {
            id: json!(1),
            method: "item/commandExecution/requestApproval".to_string(),
            params: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-1",
                "command": "touch /tmp/x"
            }),
        };
        let command_result =
            approval_response_result(&command, TelegramApprovalDecision::AcceptForSession).unwrap();
        assert_eq!(
            command_result,
            json!({
                "decision": "acceptForSession"
            })
        );

        let permissions = ApprovalRequest {
            id: json!("request-1"),
            method: "item/permissions/requestApproval".to_string(),
            params: json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-1",
                "cwd": "/tmp",
                "permissions": {
                    "network": { "enabled": true },
                    "fileSystem": null
                }
            }),
        };
        let permissions_result =
            approval_response_result(&permissions, TelegramApprovalDecision::AcceptOnce).unwrap();
        assert_eq!(permissions_result["scope"], json!("turn"));
        assert_eq!(
            permissions_result["permissions"],
            permissions.params["permissions"]
        );
    }

    #[test]
    fn forum_router_route_creates_work_topic_even_when_bound() {
        let paths = temp_paths("router-create-topic");
        let mut state = TelegramState::empty();
        let route = TelegramRoute {
            chat_id: -10042,
            message_thread_id: None,
        };
        state.bind_route(&paths, &route, None).unwrap();

        assert!(should_create_work_topic(&state, &route, true));

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn release_pauses_telegram_handoff_without_active_lease() {
        let paths = temp_paths("release-pauses");
        let mut state = TelegramState::empty();
        let route = TelegramRoute {
            chat_id: 42,
            message_thread_id: None,
        };
        state.bind_route(&paths, &route, None).unwrap();

        let reply = handle_message(
            &paths,
            &mut state,
            text_message(42, None, "/release"),
            handle_options(),
        )
        .unwrap()
        .unwrap();

        assert!(reply.text.contains("Telegram handoff paused."));
        assert!(state
            .active_binding_for_route(&route)
            .is_some_and(|binding| binding.telegram_paused));

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn paused_handoff_blocks_normal_message() {
        let paths = temp_paths("paused-blocks");
        let mut state = TelegramState::empty();
        let route = TelegramRoute {
            chat_id: 42,
            message_thread_id: None,
        };
        state.bind_route(&paths, &route, None).unwrap();
        state
            .binding_for_route_mut(&route, None)
            .unwrap()
            .telegram_paused = true;

        let reply = handle_message(
            &paths,
            &mut state,
            text_message(42, None, "continue this"),
            handle_options(),
        )
        .unwrap()
        .unwrap();

        assert!(reply
            .text
            .starts_with("Telegram handoff is paused for this topic."));
        assert_eq!(
            reply.reply_markup.unwrap().inline_keyboard[0][0].callback_data,
            "cx:t"
        );

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn plain_text_in_forum_router_returns_portal_panel() {
        let paths = temp_paths("router-text-panel");
        let mut state = TelegramState::empty();
        let message = TelegramMessage {
            chat: TelegramChat {
                id: -10042,
                is_forum: true,
            },
            message_id: 1,
            date: None,
            message_thread_id: None,
            text: Some("show me threads".to_string()),
            forum_topic_edited: None,
        };

        let reply = handle_message(&paths, &mut state, message, handle_options())
            .unwrap()
            .unwrap();

        assert!(reply.text.contains("Codex portal is unavailable."));
        assert!(reply.remember_panel_message);
        assert_eq!(
            reply.reply_markup.unwrap().inline_keyboard[0][0].callback_data,
            "cx:p"
        );
        assert!(state.bindings.is_empty());

        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn non_text_service_message_is_ignored() {
        let paths = temp_paths("non-text-service-message");
        let mut state = TelegramState::empty();
        let route = TelegramRoute {
            chat_id: -10042,
            message_thread_id: Some(99),
        };
        state.bind_route(&paths, &route, None).unwrap();

        let reply = handle_message(
            &paths,
            &mut state,
            TelegramMessage {
                chat: TelegramChat {
                    id: -10042,
                    is_forum: true,
                },
                message_id: 1,
                date: None,
                message_thread_id: Some(99),
                text: None,
                forum_topic_edited: None,
            },
            handle_options(),
        )
        .unwrap();

        assert!(reply.is_none());
        let _ = fs::remove_dir_all(paths.serve_dir());
    }

    #[test]
    fn forum_topic_edit_updates_title_metadata() {
        let paths = temp_paths("topic-title-edit");
        let mut state = TelegramState::empty();
        let route = TelegramRoute {
            chat_id: -10042,
            message_thread_id: Some(99),
        };
        state.bind_route(&paths, &route, None).unwrap();

        let reply = handle_message(
            &paths,
            &mut state,
            TelegramMessage {
                chat: TelegramChat {
                    id: -10042,
                    is_forum: true,
                },
                message_id: 1,
                date: None,
                message_thread_id: Some(99),
                text: None,
                forum_topic_edited: Some(TelegramForumTopicEdited {
                    name: Some("mobile handoff".to_string()),
                }),
            },
            handle_options(),
        )
        .unwrap();

        assert!(reply.is_none());
        assert_eq!(
            state
                .binding_for_route(&route, None)
                .and_then(|binding| binding.topic_title.as_deref()),
            Some("mobile handoff")
        );

        let _ = fs::remove_dir_all(paths.serve_dir());
    }
}
