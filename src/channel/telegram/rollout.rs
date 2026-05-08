//! Codex rollout and proxy-log observation.
//!
//! This module translates local Codex transcript/proxy records into the stream
//! events consumed by Telegram watch delivery.

use std::collections::BTreeSet;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;
#[cfg(test)]
use std::thread;
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use serde_json::Value;

use crate::app_server::parse_server_event;
#[cfg(test)]
use crate::app_server::AppServerClient;
use crate::app_server::AppStreamEvent;
use crate::app_server::CommandActivity;
use crate::app_server::CommandExecution;
use crate::app_server::CommandExecutionStatus;
use crate::app_server::ParsedServerEvent;
use crate::paths::ManagerPaths;

use super::state::TelegramPendingApproval;
use super::truncate_chars;
#[cfg(test)]
use super::ROLLOUT_OWNER_PROBE_TIMEOUT;

const ROLLOUT_REVERSE_CHUNK_SIZE: usize = 64 * 1024;
const ROLLOUT_MAX_REVERSE_LINE: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ObserveTerminal {
    Completed,
    Aborted,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ActiveRolloutTurn {
    pub(super) path: PathBuf,
    pub(super) turn_id: String,
    pub(super) offset: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ObservedRolloutTerminal {
    pub(super) turn_id: String,
    pub(super) terminal: ObserveTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RolloutTaskEvent {
    Started(String),
    Terminal {
        turn_id: Option<String>,
        terminal: ObserveTerminal,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RolloutObserveEvent {
    Stream(AppStreamEvent),
    Terminal {
        turn_id: Option<String>,
        terminal: ObserveTerminal,
        duration_ms: Option<i64>,
        last_agent_message: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RolloutDrain {
    pub(super) events: Vec<RolloutObserveEvent>,
    pub(super) next_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RolloutHistoryItem {
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
pub(super) fn active_rollout_turn(
    paths: &ManagerPaths,
    thread_id: &str,
) -> Result<Option<ActiveRolloutTurn>> {
    let Some(path) = rollout_path_for_thread(paths, thread_id) else {
        return Ok(None);
    };
    active_rollout_turn_for_path(&path, thread_id)
}

#[cfg(test)]
pub(super) fn active_rollout_turn_for_path(
    path: &Path,
    thread_id: &str,
) -> Result<Option<ActiveRolloutTurn>> {
    if !path.exists() {
        return Ok(None);
    }
    let Some((turn_id, offset)) = latest_active_rollout_turn(path)? else {
        return Ok(None);
    };
    let Some(pid) = pid_holding_file(path) else {
        return Ok(None);
    };
    if !rollout_holder_is_watchable(pid, thread_id) {
        return Ok(None);
    }
    Ok(Some(ActiveRolloutTurn {
        path: path.to_path_buf(),
        turn_id,
        offset,
    }))
}

pub(super) fn latest_active_rollout_turn(path: &Path) -> Result<Option<(String, u64)>> {
    let mut file =
        fs::File::open(path).with_context(|| format!("open rollout {}", path.display()))?;
    let file_len = file
        .metadata()
        .with_context(|| format!("stat rollout {}", path.display()))?
        .len();
    let mut cursor = file_len;
    let mut carry = Vec::<u8>::new();
    let mut closed_turns = BTreeSet::<String>::new();
    let mut discarding_oversize_line = false;

    while cursor > 0 {
        let read_len = cursor.min(ROLLOUT_REVERSE_CHUNK_SIZE as u64) as usize;
        let chunk_start = cursor - read_len as u64;
        let mut chunk = vec![0; read_len];
        file.seek(SeekFrom::Start(chunk_start))
            .with_context(|| format!("seek rollout {}", path.display()))?;
        file.read_exact(&mut chunk)
            .with_context(|| format!("read rollout {}", path.display()))?;

        if discarding_oversize_line {
            let Some(newline) = chunk.iter().rposition(|byte| *byte == b'\n') else {
                cursor = chunk_start;
                continue;
            };
            chunk.truncate(newline);
            discarding_oversize_line = false;
        }

        if !carry.is_empty() {
            chunk.extend_from_slice(&carry);
            carry.clear();
        }

        let mut line_end = chunk.len();
        while let Some(newline) = chunk[..line_end].iter().rposition(|byte| *byte == b'\n') {
            let line_start = newline + 1;
            if let Some(active) = reverse_active_rollout_line(
                &chunk[line_start..line_end],
                chunk_start + line_start as u64,
                &mut closed_turns,
            ) {
                return Ok(active);
            }
            line_end = newline;
        }

        if chunk_start == 0 {
            if let Some(active) =
                reverse_active_rollout_line(&chunk[..line_end], 0, &mut closed_turns)
            {
                return Ok(active);
            }
            break;
        }

        if line_end > ROLLOUT_MAX_REVERSE_LINE {
            discarding_oversize_line = true;
        } else {
            carry.extend_from_slice(&chunk[..line_end]);
        }
        cursor = chunk_start;
    }

    Ok(None)
}

fn reverse_active_rollout_line(
    line: &[u8],
    offset: u64,
    closed_turns: &mut BTreeSet<String>,
) -> Option<Option<(String, u64)>> {
    if line.is_empty()
        || line.len() > ROLLOUT_MAX_REVERSE_LINE
        || !looks_like_rollout_task_line(line)
    {
        return None;
    }
    let line = std::str::from_utf8(line).ok()?;
    match rollout_task_event(line)? {
        RolloutTaskEvent::Started(turn_id) if !closed_turns.contains(&turn_id) => {
            Some(Some((turn_id, offset)))
        }
        RolloutTaskEvent::Started(_) => None,
        RolloutTaskEvent::Terminal {
            turn_id: Some(turn_id),
            ..
        } => {
            closed_turns.insert(turn_id);
            None
        }
        RolloutTaskEvent::Terminal { turn_id: None, .. } => Some(None),
    }
}

fn looks_like_rollout_task_line(line: &[u8]) -> bool {
    bytes_contains(line, b"task_started")
        || bytes_contains(line, b"turn_started")
        || bytes_contains(line, b"task_complete")
        || bytes_contains(line, b"turn_complete")
        || bytes_contains(line, b"turn_aborted")
        || bytes_contains(line, b"turn_context")
}

fn bytes_contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
pub(super) fn apply_rollout_task_event(
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
pub(super) fn observe_active_rollout<F>(
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
                duration_ms: _,
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

pub(super) fn rollout_events_since(
    path: &Path,
    start_offset: u64,
    max_lines: usize,
) -> Result<RolloutDrain> {
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

pub(super) fn proxy_events_since(
    path: &Path,
    start_offset: u64,
    max_lines: usize,
    thread_id: &str,
    pending_approvals: &mut Vec<TelegramPendingApproval>,
    include_stream_events: bool,
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
        if let Some(event) = proxy_log_approval_observe_event(&line, thread_id, pending_approvals) {
            events.push(event);
            continue;
        }
        if include_stream_events {
            if let Some(event) = proxy_log_observe_event(&line, thread_id) {
                events.push(event);
            }
        }
    }

    Ok(RolloutDrain {
        events,
        next_offset,
    })
}

pub(super) fn proxy_log_approval_observe_event(
    line: &str,
    thread_id: &str,
    pending_approvals: &mut Vec<TelegramPendingApproval>,
) -> Option<RolloutObserveEvent> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let connection_id = value
        .get("connectionId")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let direction = value.get("direction").and_then(Value::as_str)?;
    let message = value.get("message")?;
    match direction {
        "server_to_client" => {
            remember_proxy_approval_request(message, connection_id, thread_id, pending_approvals);
            None
        }
        "client_to_server" => {
            proxy_approval_response_event(message, connection_id, pending_approvals)
        }
        _ => None,
    }
}

fn remember_proxy_approval_request(
    message: &Value,
    connection_id: u64,
    thread_id: &str,
    pending_approvals: &mut Vec<TelegramPendingApproval>,
) {
    if message.get("method").and_then(Value::as_str)
        != Some("item/commandExecution/requestApproval")
        && message.get("method").and_then(Value::as_str) != Some("execCommandApproval")
    {
        return;
    }
    let Some(params) = message.get("params") else {
        return;
    };
    if params
        .get("threadId")
        .or_else(|| params.get("thread_id"))
        .and_then(Value::as_str)
        != Some(thread_id)
    {
        return;
    }
    let Some(request_id) = message.get("id").map(proxy_request_id_key) else {
        return;
    };
    let Some(command) = proxy_approval_command(params) else {
        return;
    };
    pending_approvals.retain(|pending| {
        pending.connection_id != connection_id || pending.request_id != request_id
    });
    pending_approvals.push(TelegramPendingApproval {
        connection_id,
        request_id,
        command,
    });
    if pending_approvals.len() > 32 {
        let drain_count = pending_approvals.len() - 32;
        pending_approvals.drain(0..drain_count);
    }
}

fn proxy_approval_response_event(
    message: &Value,
    connection_id: u64,
    pending_approvals: &mut Vec<TelegramPendingApproval>,
) -> Option<RolloutObserveEvent> {
    let request_id = message.get("id").map(proxy_request_id_key)?;
    let decision = proxy_approval_decision(message.get("result")?)?;
    let pending_index = pending_approvals.iter().position(|pending| {
        pending.connection_id == connection_id && pending.request_id == request_id
    })?;
    let pending = pending_approvals.remove(pending_index);
    Some(RolloutObserveEvent::Stream(AppStreamEvent::Info(
        proxy_approval_decision_text(decision, &pending.command),
    )))
}

fn proxy_request_id_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| id.to_string())
}

fn proxy_approval_command(params: &Value) -> Option<String> {
    if let Some(command) = params.get("command").and_then(Value::as_str) {
        return Some(truncate_exec_snippet(command));
    }
    let command = params.get("command")?.as_array()?;
    let command = rollout_command_string(&serde_json::json!({ "command": command }))?;
    Some(truncate_exec_snippet(&command))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyApprovalDecision {
    Accept,
    AcceptForSession,
    Decline,
    Cancel,
}

fn proxy_approval_decision(result: &Value) -> Option<ProxyApprovalDecision> {
    match result.get("decision").and_then(Value::as_str)? {
        "accept" | "approved" => Some(ProxyApprovalDecision::Accept),
        "acceptForSession" | "approved_for_session" => {
            Some(ProxyApprovalDecision::AcceptForSession)
        }
        "decline" | "denied" => Some(ProxyApprovalDecision::Decline),
        "cancel" => Some(ProxyApprovalDecision::Cancel),
        _ => None,
    }
}

fn proxy_approval_decision_text(decision: ProxyApprovalDecision, command: &str) -> String {
    match decision {
        ProxyApprovalDecision::Accept => {
            format!("✔ You approved codex to run {command} this time")
        }
        ProxyApprovalDecision::AcceptForSession => {
            format!("✔ You approved codex to run {command} every time this session")
        }
        ProxyApprovalDecision::Decline => {
            format!("✗ You did not approve codex to run {command}")
        }
        ProxyApprovalDecision::Cancel => {
            format!("✗ You canceled the request to run {command}")
        }
    }
}

fn truncate_exec_snippet(command: &str) -> String {
    let first_line = command
        .split_once('\n')
        .map(|(first, _)| format!("{first} ..."))
        .unwrap_or_else(|| command.to_string());
    truncate_chars(&first_line, 80)
}

pub(super) fn proxy_log_observe_event(line: &str, thread_id: &str) -> Option<RolloutObserveEvent> {
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
            duration_ms: None,
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

#[cfg(test)]
pub(super) fn send_rollout_agent_message<F>(
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

pub(super) fn rollout_task_event(line: &str) -> Option<RolloutTaskEvent> {
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

pub(super) fn rollout_observe_event(line: &str) -> Option<RolloutObserveEvent> {
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
        "agent_message" => value
            .get("message")
            .and_then(Value::as_str)
            .map(visible_agent_message_event),
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
            duration_ms: rollout_duration_ms(value),
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
            duration_ms: rollout_duration_ms(value),
            last_agent_message: None,
        }),
        _ => None,
    }
}

fn rollout_response_item_observe_event(value: &Value) -> Option<RolloutObserveEvent> {
    let kind = value.get("type")?.as_str()?;
    match kind {
        "message" if value.get("role").and_then(Value::as_str) == Some("assistant") => {
            rollout_message_text(value).map(visible_agent_message_event)
        }
        "message" if value.get("role").and_then(Value::as_str) == Some("user") => {
            rollout_message_text(value).map(|text| {
                RolloutObserveEvent::Stream(AppStreamEvent::UserMessage(text.to_string()))
            })
        }
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

fn visible_agent_message_event(message: &str) -> RolloutObserveEvent {
    RolloutObserveEvent::Stream(AppStreamEvent::AgentDelta(message.to_string()))
}

fn rollout_message_text(value: &Value) -> Option<&str> {
    if let Some(text) = value.get("text").and_then(Value::as_str) {
        return (!text.trim().is_empty()).then_some(text);
    }
    value
        .get("content")?
        .as_array()?
        .iter()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("output_text"))
        .and_then(|item| item.get("text").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
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
        cwd: rollout_workdir(value),
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
        cwd: rollout_workdir(value),
        activity: Some(activity),
        status,
        exit_code: None,
        duration_ms: None,
        aggregated_output: output,
    })
}

fn rollout_workdir(value: &Value) -> String {
    value
        .get("cwd")
        .or_else(|| value.get("workdir"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
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
        details.push(format!(
            "{} (+{change_added} -{change_removed})",
            patch_change_display_path(path, change)
        ));
    }

    let verb = if kinds.len() == 1 {
        match kinds[0] {
            "add" | "create" => "Added",
            "delete" | "remove" => "Deleted",
            "move" | "rename" => "Moved",
            _ => "Edited",
        }
    } else {
        "Edited"
    };
    let target = if details.len() == 1 {
        details.pop().unwrap_or_else(|| "file".to_string())
    } else {
        let mut target = format!("{} files (+{added} -{removed})", details.len());
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

fn patch_change_display_path(path: &str, change: &Value) -> String {
    let Some(move_path) = change
        .get("move_path")
        .or_else(|| change.get("movePath"))
        .or_else(|| change.get("new_path"))
        .or_else(|| change.get("newPath"))
        .and_then(Value::as_str)
        .filter(|move_path| !move_path.trim().is_empty())
    else {
        return path.to_string();
    };
    format!("{} -> {}", path.trim(), move_path.trim())
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

#[derive(Default)]
struct FilePatchSummary {
    path: String,
    move_path: Option<String>,
    added: usize,
    removed: usize,
}

fn patch_activity_target(patch: &str) -> String {
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
        if let Some(move_path) = line.strip_prefix("*** Move to: ") {
            if let Some(index) = current {
                files[index].move_path = Some(move_path.trim().to_string());
            }
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
        return format!(
            "{} (+{} -{})",
            file_patch_display_path(file),
            file.added,
            file.removed
        );
    }
    let added = files.iter().map(|file| file.added).sum::<usize>();
    let removed = files.iter().map(|file| file.removed).sum::<usize>();
    let mut target = format!("{} files (+{added} -{removed})", files.len());
    for file in files {
        target.push('\n');
        target.push_str(&format!(
            "{} (+{} -{})",
            file_patch_display_path(&file),
            file.added,
            file.removed
        ));
    }
    target
}

fn file_patch_display_path(file: &FilePatchSummary) -> String {
    match file.move_path.as_deref() {
        Some(move_path) if !move_path.trim().is_empty() => {
            format!("{} -> {}", file.path, move_path.trim())
        }
        _ => file.path.clone(),
    }
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
        "move" | "rename" => "Moved",
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

pub(super) fn command_path_label(path: &str) -> Option<&str> {
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

pub(super) fn rollout_history_text(
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

pub(super) fn rollout_history_item(line: &str) -> Option<RolloutHistoryItem> {
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

pub(super) fn codex_state_db_paths(paths: &ManagerPaths) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    push_unique_existing_path(
        &mut candidates,
        paths.base_codex_home.join("state_5.sqlite"),
    );
    if let Ok(entries) = fs::read_dir(&paths.slots_dir) {
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let slot_home = entry.path().join("home");
            push_unique_existing_path(&mut candidates, slot_home.join("state_5.sqlite"));
            push_unique_existing_path(&mut candidates, slot_home.join(".codex/state_5.sqlite"));
        }
    }
    candidates
}

fn push_unique_existing_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !path.exists() {
        return;
    }
    let key = path.canonicalize().unwrap_or_else(|_| path.clone());
    if paths
        .iter()
        .all(|existing| existing.canonicalize().unwrap_or_else(|_| existing.clone()) != key)
    {
        paths.push(path);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RolloutThreadInfo {
    path: PathBuf,
    archived: bool,
}

fn rollout_thread_info(paths: &ManagerPaths, thread_id: &str) -> Option<RolloutThreadInfo> {
    for db_path in codex_state_db_paths(paths) {
        if let Some(info) = rollout_thread_info_from_db(&db_path, thread_id) {
            return Some(info);
        }
    }
    None
}

fn rollout_thread_info_from_db(db_path: &Path, thread_id: &str) -> Option<RolloutThreadInfo> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
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

pub(super) fn rollout_path_for_thread(paths: &ManagerPaths, thread_id: &str) -> Option<PathBuf> {
    rollout_thread_info(paths, thread_id).map(|info| info.path)
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
pub(super) fn pid_holding_file(path: &Path) -> Option<u32> {
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
pub(super) fn pid_listening_urls(pid: u32) -> Vec<String> {
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
pub(super) fn loopback_listen_urls(text: &str) -> Vec<String> {
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
