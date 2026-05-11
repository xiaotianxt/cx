//! Telegram watch delivery state machine.
//!
//! This is the single owner of live transcript panel flushing and tail-message
//! placement. The driver feeds events in; this module decides what to edit or
//! send to match the Codex TUI history model.

use std::time::Duration;
use std::time::Instant;

use anyhow::Result;

use crate::app_server::AppStreamEvent;

use super::api::is_telegram_missing_thread_error;
use super::api::telegram_text_chunks;
use super::state::TelegramRoute;
use super::transcript::info_watch_text;
use super::transcript::user_watch_text;
use super::transcript::TelegramActivityPanel;
use super::transcript::TelegramActivityState;
use super::transcript::TelegramStatusPanel;
use super::transcript::TelegramStatusState;
use super::transcript::TelegramThinkingPanel;
use super::transcript::TelegramThinkingState;
use super::transcript::TelegramTranscriptTarget;
use super::transcript::TelegramTurnTerminal;
use super::TelegramNotifier;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WatchEvent {
    Stream(AppStreamEvent),
    Terminal {
        turn_id: Option<String>,
        terminal: WatchTerminal,
        duration_ms: Option<i64>,
        last_agent_message: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WatchTerminal {
    Completed,
    Aborted,
}

pub(super) struct WatchSendResult {
    pub(super) activity: Option<TelegramActivityState>,
    pub(super) thinking: Option<TelegramThinkingState>,
    pub(super) status: Option<TelegramStatusState>,
    pub(super) last_agent_message: Option<String>,
}

pub(super) fn send_watch_events(
    route: &TelegramRoute,
    notifier: &TelegramNotifier<'_>,
    events: Vec<WatchEvent>,
    activity: Option<TelegramActivityState>,
    thinking: Option<TelegramThinkingState>,
    status: Option<TelegramStatusState>,
    last_agent_message: Option<String>,
) -> Result<WatchSendResult> {
    let mut sink =
        TelegramWatchSink::new_best_effort(notifier, route.clone(), activity, thinking, status);
    let mut last_sent_agent_message = last_agent_message;
    let mut pending_agent_message = None::<String>;
    for event in events {
        match event {
            WatchEvent::Stream(AppStreamEvent::TurnStarted) => {
                last_sent_agent_message = None;
                pending_agent_message = None;
                sink.push_event(AppStreamEvent::TurnStarted)?;
            }
            WatchEvent::Stream(AppStreamEvent::AgentDelta(message)) => {
                push_agent_message_if_new(
                    &mut sink,
                    &message,
                    last_sent_agent_message.as_deref(),
                    &mut pending_agent_message,
                )?;
            }
            WatchEvent::Stream(event) => sink.push_event(event)?,
            WatchEvent::Terminal {
                duration_ms,
                terminal,
                last_agent_message,
                ..
            } => {
                if let Some(message) = last_agent_message {
                    push_agent_message_if_new(
                        &mut sink,
                        &message,
                        last_sent_agent_message.as_deref(),
                        &mut pending_agent_message,
                    )?;
                }
                sink.turn_completed(duration_ms, telegram_turn_terminal(terminal))?;
            }
        }
    }
    sink.flush_pending()?;
    if sink.assistant_completed_text_sent() {
        if let Some(message) = pending_agent_message {
            last_sent_agent_message = Some(message);
        }
    }
    Ok(WatchSendResult {
        activity: sink.activity_state(),
        thinking: sink.thinking_state(),
        status: sink.status_state(),
        last_agent_message: last_sent_agent_message,
    })
}

fn push_agent_message_if_new(
    sink: &mut TelegramWatchSink<'_>,
    message: &str,
    last_sent_agent_message: Option<&str>,
    pending_agent_message: &mut Option<String>,
) -> Result<()> {
    if message.trim().is_empty()
        || last_sent_agent_message == Some(message)
        || pending_agent_message.as_deref() == Some(message)
    {
        return Ok(());
    }
    sink.push_event(AppStreamEvent::AgentDelta(message.to_string()))?;
    *pending_agent_message = Some(message.to_string());
    Ok(())
}

fn telegram_turn_terminal(terminal: WatchTerminal) -> TelegramTurnTerminal {
    match terminal {
        WatchTerminal::Completed => TelegramTurnTerminal::Done,
        WatchTerminal::Aborted => TelegramTurnTerminal::Interrupted,
    }
}

pub(super) struct TelegramWatchSink<'a> {
    status: TelegramStatusPanel,
    thinking: TelegramThinkingPanel,
    agent: TelegramDeltaSink<'a>,
    pub(super) activity: TelegramActivityPanel,
    sent_any: bool,
    best_effort_delivery: bool,
    pub(super) status_needs_tail: bool,
}

impl<'a> TelegramWatchSink<'a> {
    pub(super) fn new_best_effort(
        notifier: &'a TelegramNotifier<'a>,
        route: TelegramRoute,
        activity: Option<TelegramActivityState>,
        thinking: Option<TelegramThinkingState>,
        status: Option<TelegramStatusState>,
    ) -> Self {
        let status = TelegramStatusPanel::from_state(status);
        Self {
            status,
            thinking: TelegramThinkingPanel::from_state(thinking),
            agent: TelegramDeltaSink::new(notifier, route),
            activity: TelegramActivityPanel::from_state(activity),
            sent_any: false,
            best_effort_delivery: true,
            status_needs_tail: false,
        }
    }

    fn mark_status_not_tail(&mut self) {
        if self.status.is_active() || self.status.message_id().is_some() {
            self.status_needs_tail = true;
        }
    }

    fn ensure_status_tail(&mut self, force: bool) -> Result<()> {
        if !self.status_needs_tail {
            return Ok(());
        }
        if let Some(message_id) = self.status.message_id() {
            match self
                .agent
                .notifier
                .delete_one(&self.agent.route, message_id)
            {
                Ok(()) => {}
                Err(err) if is_telegram_missing_thread_error(&err) => return Err(err),
                Err(err) if self.best_effort_delivery => {
                    if let Some(delay) = telegram_retry_after_delay(&err) {
                        self.status.defer_delivery_for(delay);
                        log_watch_delivery_failure(&self.agent.route, "status delete", err);
                        return Ok(());
                    }
                    log_watch_delivery_failure(&self.agent.route, "status delete", err);
                }
                Err(err) => return Err(err),
            }
            self.status.clear_message_id();
        }
        if flush_status_panel(
            &mut self.status,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            force,
        )? {
            self.sent_any = true;
            self.status_needs_tail = false;
        }
        Ok(())
    }

    pub(super) fn seal_activity_cell(&mut self) -> Result<()> {
        if !self.activity.has_content() {
            return Ok(());
        }
        let had_message = self.activity.message_id.is_some();
        let was_dirty = self.activity.is_dirty();
        let sent = flush_activity_panel(
            &mut self.activity,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            true,
        )?;
        if sent {
            self.sent_any = true;
            if !had_message {
                self.mark_status_not_tail();
            }
        }
        if was_dirty && !sent {
            return Ok(());
        }
        self.activity = TelegramActivityPanel::new();
        Ok(())
    }

    pub(super) fn push_event(&mut self, event: AppStreamEvent) -> Result<()> {
        match event {
            AppStreamEvent::UserMessage(message) => {
                self.seal_activity_cell()?;
                match self
                    .agent
                    .notifier
                    .send_chunks(&self.agent.route, &user_watch_text(&message))
                {
                    Ok(Some(_)) => {
                        self.sent_any = true;
                        self.mark_status_not_tail();
                    }
                    Ok(None) => {}
                    Err(err) if is_telegram_missing_thread_error(&err) => return Err(err),
                    Err(err) if self.best_effort_delivery => {
                        log_watch_delivery_failure(&self.agent.route, "user message", err);
                    }
                    Err(err) => return Err(err),
                }
                self.ensure_status_tail(false)?;
                Ok(())
            }
            AppStreamEvent::TurnStarted => {
                self.seal_activity_cell()?;
                self.start_status_turn(true)?;
                self.thinking.start_new_turn();
                let had_thinking = self.thinking.message_id().is_some();
                if flush_thinking_panel(
                    &mut self.thinking,
                    self.agent.notifier,
                    &self.agent.route,
                    self.best_effort_delivery,
                    false,
                )? {
                    self.sent_any = true;
                    if !had_thinking {
                        self.mark_status_not_tail();
                    }
                }
                self.ensure_status_tail(false)?;
                Ok(())
            }
            AppStreamEvent::Info(message) => {
                self.seal_activity_cell()?;
                self.ensure_status_started(false)?;
                if self.thinking.is_active() {
                    self.thinking.finish();
                    let had_thinking = self.thinking.message_id().is_some();
                    if flush_thinking_panel(
                        &mut self.thinking,
                        self.agent.notifier,
                        &self.agent.route,
                        self.best_effort_delivery,
                        true,
                    )? {
                        self.sent_any = true;
                        if !had_thinking {
                            self.mark_status_not_tail();
                        }
                    }
                }
                match self
                    .agent
                    .notifier
                    .send_chunks(&self.agent.route, &info_watch_text(&message))
                {
                    Ok(Some(_)) => {
                        self.sent_any = true;
                        self.mark_status_not_tail();
                    }
                    Ok(None) => {}
                    Err(err) if is_telegram_missing_thread_error(&err) => return Err(err),
                    Err(err) if self.best_effort_delivery => {
                        log_watch_delivery_failure(&self.agent.route, "info", err);
                    }
                    Err(err) => return Err(err),
                }
                self.ensure_status_tail(false)?;
                Ok(())
            }
            AppStreamEvent::ReasoningStarted => {
                self.seal_activity_cell()?;
                self.ensure_status_started(false)?;
                self.thinking.ensure_active();
                let had_thinking = self.thinking.message_id().is_some();
                if flush_thinking_panel(
                    &mut self.thinking,
                    self.agent.notifier,
                    &self.agent.route,
                    self.best_effort_delivery,
                    false,
                )? {
                    self.sent_any = true;
                    if !had_thinking {
                        self.mark_status_not_tail();
                    }
                }
                self.ensure_status_tail(false)?;
                Ok(())
            }
            AppStreamEvent::ReasoningDelta(delta) => {
                self.seal_activity_cell()?;
                self.ensure_status_started(false)?;
                self.thinking.push(&delta);
                let had_thinking = self.thinking.message_id().is_some();
                if flush_thinking_panel(
                    &mut self.thinking,
                    self.agent.notifier,
                    &self.agent.route,
                    self.best_effort_delivery,
                    false,
                )? {
                    self.sent_any = true;
                    if !had_thinking {
                        self.mark_status_not_tail();
                    }
                }
                self.ensure_status_tail(false)?;
                Ok(())
            }
            AppStreamEvent::AgentDelta(delta) => {
                self.seal_activity_cell()?;
                self.ensure_status_started(false)?;
                if self.thinking.is_active() {
                    self.thinking.finish();
                    let had_thinking = self.thinking.message_id().is_some();
                    if flush_thinking_panel(
                        &mut self.thinking,
                        self.agent.notifier,
                        &self.agent.route,
                        self.best_effort_delivery,
                        true,
                    )? {
                        self.sent_any = true;
                        if !had_thinking {
                            self.mark_status_not_tail();
                        }
                    }
                }
                let had_agent = self.agent.message_id.is_some();
                if let Err(err) = self.agent.push(&delta) {
                    if is_telegram_missing_thread_error(&err) {
                        return Err(err);
                    }
                    if !self.best_effort_delivery {
                        return Err(err);
                    }
                    self.agent.mark_delivery_attempted();
                    log_watch_delivery_failure(&self.agent.route, "agent message", err);
                }
                if !had_agent && self.agent.message_id.is_some() {
                    self.mark_status_not_tail();
                }
                self.ensure_status_tail(false)?;
                Ok(())
            }
            AppStreamEvent::CommandStarted(command) => {
                self.ensure_status_started(false)?;
                let had_activity = self.activity.message_id.is_some();
                self.activity.apply_execution(command);
                if flush_activity_panel(
                    &mut self.activity,
                    self.agent.notifier,
                    &self.agent.route,
                    self.best_effort_delivery,
                    false,
                )? {
                    self.sent_any = true;
                    if !had_activity {
                        self.mark_status_not_tail();
                    }
                }
                self.ensure_status_tail(false)?;
                Ok(())
            }
            AppStreamEvent::CommandOutputDelta { item_id, delta } => {
                self.ensure_status_started(false)?;
                let had_activity = self.activity.message_id.is_some();
                self.activity.apply_output_delta(&item_id, &delta);
                if flush_activity_panel(
                    &mut self.activity,
                    self.agent.notifier,
                    &self.agent.route,
                    self.best_effort_delivery,
                    false,
                )? {
                    self.sent_any = true;
                    if !had_activity {
                        self.mark_status_not_tail();
                    }
                }
                self.ensure_status_tail(false)?;
                Ok(())
            }
            AppStreamEvent::CommandCompleted(command) => {
                self.ensure_status_started(false)?;
                let had_activity = self.activity.message_id.is_some();
                self.activity.apply_execution(command);
                if flush_activity_panel(
                    &mut self.activity,
                    self.agent.notifier,
                    &self.agent.route,
                    self.best_effort_delivery,
                    false,
                )? {
                    self.sent_any = true;
                    if !had_activity {
                        self.mark_status_not_tail();
                    }
                }
                self.ensure_status_tail(false)?;
                Ok(())
            }
        }
    }

    pub(super) fn begin_pending_turn(&mut self) -> Result<()> {
        self.seal_activity_cell()?;
        self.start_status_turn(true)
    }

    fn ensure_status_started(&mut self, force: bool) -> Result<()> {
        self.status.ensure_active();
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

    fn start_status_turn(&mut self, force: bool) -> Result<()> {
        self.status.start_new_turn();
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

    pub(super) fn turn_completed(
        &mut self,
        duration_ms: Option<i64>,
        terminal: TelegramTurnTerminal,
    ) -> Result<()> {
        self.status.finish_with_terminal(duration_ms, terminal);
        self.thinking.finish();
        let had_thinking = self.thinking.message_id().is_some();
        if flush_thinking_panel(
            &mut self.thinking,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            true,
        )? {
            self.sent_any = true;
            if !had_thinking {
                self.mark_status_not_tail();
            }
        }
        self.activity.finish_turn();
        let had_activity = self.activity.message_id.is_some();
        if flush_activity_panel(
            &mut self.activity,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            true,
        )? {
            self.sent_any = true;
            if !had_activity {
                self.mark_status_not_tail();
            }
        }
        if self.status_needs_tail {
            self.ensure_status_tail(true)?;
        } else if flush_status_panel(
            &mut self.status,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            true,
        )? {
            self.sent_any = true;
        }
        Ok(())
    }

    pub(super) fn flush_pending(&mut self) -> Result<()> {
        let had_thinking = self.thinking.message_id().is_some();
        if flush_thinking_panel(
            &mut self.thinking,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            false,
        )? {
            self.sent_any = true;
            if !had_thinking {
                self.mark_status_not_tail();
            }
        }
        let had_agent = self.agent.message_id.is_some();
        if let Err(err) = self.agent.finish() {
            if is_telegram_missing_thread_error(&err) {
                return Err(err);
            }
            if !self.best_effort_delivery {
                return Err(err);
            }
            log_watch_delivery_failure(&self.agent.route, "agent message", err);
        }
        if !had_agent && self.agent.message_id.is_some() {
            self.mark_status_not_tail();
        }
        let had_activity = self.activity.message_id.is_some();
        if flush_activity_panel(
            &mut self.activity,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            true,
        )? {
            self.sent_any = true;
            if !had_activity {
                self.mark_status_not_tail();
            }
        }
        if self.status_needs_tail {
            self.ensure_status_tail(false)?;
        } else if flush_status_panel(
            &mut self.status,
            self.agent.notifier,
            &self.agent.route,
            self.best_effort_delivery,
            false,
        )? {
            self.sent_any = true;
        }
        Ok(())
    }

    pub(super) fn sent_any(&self) -> bool {
        self.sent_any
            || self.status.sent_any()
            || self.thinking.sent_any()
            || self.agent.sent_any()
            || self.activity.sent_any()
    }

    pub(super) fn assistant_completed_text_sent(&self) -> bool {
        self.agent.completed_text_sent()
    }

    pub(super) fn activity_state(&self) -> Option<TelegramActivityState> {
        self.activity.to_state()
    }

    pub(super) fn thinking_state(&self) -> Option<TelegramThinkingState> {
        self.thinking.to_state()
    }

    pub(super) fn status_state(&self) -> Option<TelegramStatusState> {
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
            if let Some(delay) = telegram_retry_after_delay(&err) {
                panel.defer_delivery_for(delay);
            } else {
                panel.mark_delivery_attempted();
            }
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
            if let Some(delay) = telegram_retry_after_delay(&err) {
                panel.defer_delivery_for(delay);
            } else {
                panel.mark_delivery_attempted();
            }
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
            if let Some(delay) = telegram_retry_after_delay(&err) {
                panel.defer_delivery_for(delay);
            } else {
                panel.mark_delivery_attempted();
            }
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

pub(super) fn telegram_retry_after_delay(err: &anyhow::Error) -> Option<Duration> {
    let text = format!("{err:#}");
    let marker = "retry after ";
    let start = text.find(marker)? + marker.len();
    let seconds = text[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(seconds.saturating_add(1)))
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
    completed_text_sent: bool,
}

impl<'a> TelegramDeltaSink<'a> {
    const MIN_EDIT_INTERVAL: Duration = Duration::from_secs(3);
    const MIN_DELTA_CHARS: usize = 600;

    fn new(notifier: &'a TelegramNotifier<'a>, route: TelegramRoute) -> Self {
        Self {
            notifier,
            route,
            text: String::new(),
            message_id: None,
            last_sent_chars: 0,
            last_flush: Instant::now() - Self::MIN_EDIT_INTERVAL,
            sent_any: false,
            completed_text_sent: false,
        }
    }

    fn push(&mut self, delta: &str) -> Result<()> {
        self.text.push_str(delta);
        self.completed_text_sent = false;
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

    fn completed_text_sent(&self) -> bool {
        self.completed_text_sent
    }

    fn mark_delivery_attempted(&mut self) {
        self.last_flush = Instant::now();
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
        self.completed_text_sent = final_flush;
        Ok(())
    }
}
