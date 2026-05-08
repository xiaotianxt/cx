//! Telegram message delivery helpers.
//!
//! This module adapts channel replies and transcript panel updates to Telegram
//! send/edit/delete calls. It owns delivery mechanics, not routing policy.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use reqwest::blocking::Client;

use super::api::answer_callback_query;
use super::api::delete_message;
use super::api::edit_message_text;
use super::api::send_chat_action;
use super::api::send_message;
use super::api::set_message_reaction;
use super::api::telegram_text_chunks;
use super::api::TelegramInlineKeyboardMarkup;
use super::api::TelegramMessage;
use super::state::TelegramRoute;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TelegramReply {
    pub(super) chat_id: i64,
    pub(super) message_thread_id: Option<i64>,
    pub(super) text: String,
    pub(super) reply_markup: Option<TelegramInlineKeyboardMarkup>,
    pub(super) edit_message_id: Option<i64>,
    pub(super) remember_panel_message: bool,
}

pub(super) struct TelegramNotifier<'a> {
    pub(super) client: &'a Client,
    pub(super) token: &'a str,
}

impl TelegramNotifier<'_> {
    pub(super) fn ack_seen(&self, message: &TelegramMessage) {
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

    pub(super) fn answer_callback_query(&self, callback_query_id: &str, text: Option<&str>) {
        if let Err(err) = answer_callback_query(self.client, self.token, callback_query_id, text) {
            eprintln!("telegram answerCallbackQuery failed: {err:#}");
        }
    }

    pub(super) fn typing(&self, route: &TelegramRoute) -> TelegramTypingGuard {
        TelegramTypingGuard::start(self.client.clone(), self.token.to_string(), route.clone())
    }

    pub(super) fn send_one(&self, route: &TelegramRoute, text: &str) -> Result<i64> {
        send_message(
            self.client,
            self.token,
            route.chat_id,
            route.message_thread_id,
            text,
            None,
        )
    }

    pub(super) fn send_chunks(&self, route: &TelegramRoute, text: &str) -> Result<Option<i64>> {
        let mut first_message_id = None;
        for chunk in telegram_text_chunks(text) {
            let message_id = self.send_one(route, &chunk)?;
            if first_message_id.is_none() {
                first_message_id = Some(message_id);
            }
        }
        Ok(first_message_id)
    }

    pub(super) fn send_with_keyboard(
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

    pub(super) fn edit_one(
        &self,
        route: &TelegramRoute,
        message_id: i64,
        text: &str,
    ) -> Result<()> {
        edit_message_text(
            self.client,
            self.token,
            route.chat_id,
            message_id,
            text,
            None,
        )
    }

    pub(super) fn delete_one(&self, route: &TelegramRoute, message_id: i64) -> Result<()> {
        delete_message(self.client, self.token, route.chat_id, message_id)
    }
}

pub(super) struct TelegramTypingGuard {
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

pub(super) fn reply(route: &TelegramRoute, text: impl Into<String>) -> TelegramReply {
    TelegramReply {
        chat_id: route.chat_id,
        message_thread_id: route.message_thread_id,
        text: text.into(),
        reply_markup: None,
        edit_message_id: None,
        remember_panel_message: false,
    }
}

pub(super) fn panel_reply_with_keyboard(
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
    pub(super) fn route(&self) -> TelegramRoute {
        TelegramRoute {
            chat_id: self.chat_id,
            message_thread_id: self.message_thread_id,
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

pub(super) fn deliver_reply(
    client: &Client,
    token: &str,
    reply: &TelegramReply,
) -> Result<Option<i64>> {
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
