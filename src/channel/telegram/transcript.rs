use std::collections::BTreeMap;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use crate::app_server::CommandActivity;
use crate::app_server::CommandExecution;
use crate::app_server::CommandExecutionStatus;

use super::command_path_label;
use super::truncate_chars;
use super::unix_millis;

pub(super) trait TelegramTranscriptTarget {
    fn send_one(&self, text: &str) -> Result<i64>;
    fn edit_one(&self, message_id: i64, text: &str) -> Result<()>;
}

pub(super) struct TelegramStatusPanel {
    message_id: Option<i64>,
    turn_started_at_ms: Option<u128>,
    turn_finished_at_ms: Option<u128>,
    last_flush: Instant,
    last_flush_at_ms: Option<u128>,
    retry_after_ms: Option<u128>,
    last_sent_text: Option<String>,
    sent_any: bool,
    dirty: bool,
    active: bool,
}

impl TelegramStatusPanel {
    const MIN_EDIT_INTERVAL: Duration = Duration::from_secs(20);

    fn new() -> Self {
        Self {
            message_id: None,
            turn_started_at_ms: None,
            turn_finished_at_ms: None,
            last_flush: Instant::now() - Self::MIN_EDIT_INTERVAL,
            last_flush_at_ms: None,
            retry_after_ms: None,
            last_sent_text: None,
            sent_any: false,
            dirty: false,
            active: false,
        }
    }

    pub(super) fn from_state(state: Option<TelegramStatusState>) -> Self {
        let Some(state) = state else {
            return Self::new();
        };
        Self {
            message_id: state.message_id,
            turn_started_at_ms: state.turn_started_at_ms,
            turn_finished_at_ms: state.turn_finished_at_ms,
            last_flush: instant_from_unix_millis(state.last_flush_at_ms, Self::MIN_EDIT_INTERVAL),
            last_flush_at_ms: state.last_flush_at_ms,
            retry_after_ms: state.retry_after_ms,
            last_sent_text: state.last_sent_text,
            sent_any: false,
            dirty: state.active,
            active: state.active,
        }
    }

    pub(super) fn to_state(&self) -> Option<TelegramStatusState> {
        if self.message_id.is_none() && self.turn_started_at_ms.is_none() {
            return None;
        }
        Some(TelegramStatusState {
            message_id: self.message_id,
            turn_started_at_ms: self.turn_started_at_ms,
            turn_finished_at_ms: self.turn_finished_at_ms,
            last_flush_at_ms: self.last_flush_at_ms,
            retry_after_ms: self.retry_after_ms,
            last_sent_text: self.last_sent_text.clone(),
            active: self.active,
        })
    }

    pub(super) fn start(&mut self) {
        if self.active {
            return;
        }
        self.active = true;
        self.turn_finished_at_ms = None;
        if self.turn_started_at_ms.is_none() {
            self.turn_started_at_ms = Some(unix_millis());
        }
        self.dirty = true;
    }

    pub(super) fn finish(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        self.turn_finished_at_ms = Some(unix_millis());
        self.dirty = true;
    }

    pub(super) fn flush<T: TelegramTranscriptTarget>(
        &mut self,
        target: &T,
        force: bool,
    ) -> Result<bool> {
        if !(self.dirty || self.active && self.last_flush.elapsed() >= Self::MIN_EDIT_INTERVAL) {
            return Ok(false);
        }
        if retry_after_is_active(self.retry_after_ms) {
            return Ok(false);
        }
        if !force
            && self.message_id.is_some()
            && self.last_flush.elapsed() < Self::MIN_EDIT_INTERVAL
        {
            return Ok(false);
        }
        let text = status_watch_text(self);
        if self.last_sent_text.as_deref() == Some(text.as_str()) {
            self.dirty = false;
            return Ok(false);
        }
        match self.message_id {
            Some(message_id) => target.edit_one(message_id, &text)?,
            None => {
                self.message_id = Some(target.send_one(&text)?);
            }
        }
        self.dirty = false;
        self.record_delivery_attempt();
        self.retry_after_ms = None;
        self.last_sent_text = Some(text);
        self.sent_any = true;
        Ok(true)
    }

    pub(super) fn mark_delivery_attempted(&mut self) {
        self.record_delivery_attempt();
    }

    pub(super) fn defer_delivery_for(&mut self, delay: Duration) {
        self.record_delivery_attempt();
        let retry_after = unix_millis().saturating_add(delay.as_millis());
        self.retry_after_ms = Some(
            self.retry_after_ms
                .map(|existing| existing.max(retry_after))
                .unwrap_or(retry_after),
        );
    }

    pub(super) fn sent_any(&self) -> bool {
        self.sent_any
    }

    fn record_delivery_attempt(&mut self) {
        self.last_flush = Instant::now();
        self.last_flush_at_ms = Some(unix_millis());
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct TelegramStatusState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_started_at_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_finished_at_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_flush_at_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_sent_text: Option<String>,
    #[serde(default)]
    active: bool,
}

pub(super) fn status_watch_text(panel: &TelegramStatusPanel) -> String {
    let started = panel.turn_started_at_ms.unwrap_or_else(unix_millis);
    let now = panel.turn_finished_at_ms.unwrap_or_else(unix_millis);
    let elapsed = now.saturating_sub(started);
    if panel.active {
        format!(
            "• **Working** ({} • esc to interrupt)",
            format_duration_ms(elapsed as i64)
        )
    } else {
        format!("• **Done** ({})", format_duration_ms(elapsed as i64))
    }
}

pub(super) struct TelegramThinkingPanel {
    text: String,
    message_id: Option<i64>,
    last_flush: Instant,
    retry_after_ms: Option<u128>,
    last_sent_text: Option<String>,
    last_sent_chars: usize,
    sent_any: bool,
    dirty: bool,
    active: bool,
    done: bool,
}

impl TelegramThinkingPanel {
    const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(900);
    const MIN_DELTA_CHARS: usize = 160;

    pub(super) fn new() -> Self {
        Self {
            text: String::new(),
            message_id: None,
            last_flush: Instant::now() - Self::MIN_EDIT_INTERVAL,
            retry_after_ms: None,
            last_sent_text: None,
            last_sent_chars: 0,
            sent_any: false,
            dirty: false,
            active: false,
            done: false,
        }
    }

    pub(super) fn start(&mut self) {
        if self.done {
            return;
        }
        self.active = true;
    }

    pub(super) fn push(&mut self, delta: &str) {
        if self.done {
            return;
        }
        self.active = true;
        self.text.push_str(delta);
        self.dirty = true;
    }

    pub(super) fn finish(&mut self) {
        if !self.active || self.done {
            return;
        }
        self.active = false;
        self.done = true;
        self.dirty |= !self.text.trim().is_empty();
    }

    pub(super) fn is_active(&self) -> bool {
        self.active && !self.done && !self.text.trim().is_empty()
    }

    pub(super) fn flush<T: TelegramTranscriptTarget>(
        &mut self,
        target: &T,
        force: bool,
    ) -> Result<bool> {
        if !self.dirty {
            return Ok(false);
        }
        if self.text.trim().is_empty() {
            self.dirty = false;
            return Ok(false);
        }
        if retry_after_is_active(self.retry_after_ms) {
            return Ok(false);
        }
        let current_chars = self.text.chars().count();
        if !force
            && self.message_id.is_some()
            && self.last_flush.elapsed() < Self::MIN_EDIT_INTERVAL
            && current_chars.saturating_sub(self.last_sent_chars) < Self::MIN_DELTA_CHARS
        {
            return Ok(false);
        }

        let text = thinking_watch_text(self);
        if self.last_sent_text.as_deref() == Some(text.as_str()) {
            self.dirty = false;
            return Ok(false);
        }

        match self.message_id {
            Some(message_id) => target.edit_one(message_id, &text)?,
            None => {
                self.message_id = Some(target.send_one(&text)?);
            }
        }
        self.dirty = false;
        self.last_flush = Instant::now();
        self.retry_after_ms = None;
        self.last_sent_chars = current_chars;
        self.last_sent_text = Some(text);
        self.sent_any = true;
        Ok(true)
    }

    pub(super) fn mark_delivery_attempted(&mut self) {
        self.last_flush = Instant::now();
    }

    pub(super) fn defer_delivery_for(&mut self, delay: Duration) {
        self.mark_delivery_attempted();
        let retry_after = unix_millis().saturating_add(delay.as_millis());
        self.retry_after_ms = Some(
            self.retry_after_ms
                .map(|existing| existing.max(retry_after))
                .unwrap_or(retry_after),
        );
    }

    pub(super) fn sent_any(&self) -> bool {
        self.sent_any
    }
}

pub(super) fn thinking_watch_text(panel: &TelegramThinkingPanel) -> String {
    let title = if panel.done {
        "Codex"
    } else {
        "Codex is working"
    };
    let body = panel.text.trim();
    truncate_chars(&format!("{title}\n{body}"), 1800)
}

pub(super) fn info_watch_text(message: &str) -> String {
    truncate_chars(&format!("• {}", message.trim()), 1800)
}

pub(super) fn user_watch_text(message: &str) -> String {
    truncate_chars(&format!("You\n{}", message.trim()), 1800)
}

pub(super) struct TelegramActivityPanel {
    order: Vec<String>,
    items: BTreeMap<String, TelegramActivityItem>,
    pub(super) message_id: Option<i64>,
    last_flush: Instant,
    last_flush_at_ms: Option<u128>,
    retry_after_ms: Option<u128>,
    pub(super) last_sent_text: Option<String>,
    sent_any: bool,
    dirty: bool,
}

impl TelegramActivityPanel {
    const MAX_ITEMS: usize = 8;
    const MIN_EDIT_INTERVAL: Duration = Duration::from_secs(2);

    pub(super) fn new() -> Self {
        Self {
            order: Vec::new(),
            items: BTreeMap::new(),
            message_id: None,
            last_flush: Instant::now() - Self::MIN_EDIT_INTERVAL,
            last_flush_at_ms: None,
            retry_after_ms: None,
            last_sent_text: None,
            sent_any: false,
            dirty: false,
        }
    }

    pub(super) fn from_state(state: Option<TelegramActivityState>) -> Self {
        let Some(state) = state else {
            return Self::new();
        };
        Self {
            order: state.order,
            items: state.items,
            message_id: state.message_id,
            last_flush: instant_from_unix_millis(state.last_flush_at_ms, Self::MIN_EDIT_INTERVAL),
            last_flush_at_ms: state.last_flush_at_ms,
            retry_after_ms: state.retry_after_ms,
            last_sent_text: state.last_sent_text,
            sent_any: false,
            dirty: false,
        }
    }

    pub(super) fn to_state(&self) -> Option<TelegramActivityState> {
        if self.order.is_empty() && self.message_id.is_none() {
            return None;
        }
        Some(TelegramActivityState {
            order: self.order.clone(),
            items: self.items.clone(),
            message_id: self.message_id,
            last_flush_at_ms: self.last_flush_at_ms,
            retry_after_ms: self.retry_after_ms,
            last_sent_text: self.last_sent_text.clone(),
        })
    }

    pub(super) fn apply_execution(&mut self, command: CommandExecution) {
        let item_id = command.item_id.clone();
        let previous_output = self.items.get(&item_id).map(|item| item.output.clone());
        if !self.items.contains_key(&command.item_id) {
            self.order.push(command.item_id.clone());
        }
        let mut item = TelegramActivityItem::from(command);
        if item.output.is_empty() {
            if let Some(output) = previous_output {
                item.output = output;
            }
        }
        self.items.insert(item_id, item);
        while self.order.len() > Self::MAX_ITEMS {
            if let Some(item_id) = self.order.first().cloned() {
                self.order.remove(0);
                self.items.remove(&item_id);
            }
        }
        self.dirty = true;
    }

    pub(super) fn apply_output_delta(&mut self, item_id: &str, delta: &str) {
        let Some(item) = self.items.get_mut(item_id) else {
            return;
        };
        item.push_output(delta);
        self.dirty = true;
    }

    pub(super) fn finish_turn(&mut self) {
        let mut changed = false;
        for item in self.items.values_mut() {
            if matches!(item.status, CommandExecutionStatus::InProgress) {
                item.status = CommandExecutionStatus::Completed;
                changed = true;
            }
        }
        self.dirty |= changed;
    }

    pub(super) fn flush<T: TelegramTranscriptTarget>(
        &mut self,
        target: &T,
        force: bool,
    ) -> Result<bool> {
        if !self.dirty {
            return Ok(false);
        }
        if retry_after_is_active(self.retry_after_ms) {
            return Ok(false);
        }
        if !force
            && self.message_id.is_some()
            && self.last_flush.elapsed() < Self::MIN_EDIT_INTERVAL
        {
            return Ok(false);
        }

        let text = activity_watch_text(self);
        if self.last_sent_text.as_deref() == Some(text.as_str()) {
            self.dirty = false;
            return Ok(false);
        }

        match self.message_id {
            Some(message_id) => target.edit_one(message_id, &text)?,
            None => {
                self.message_id = Some(target.send_one(&text)?);
            }
        }
        self.dirty = false;
        self.record_delivery_attempt();
        self.retry_after_ms = None;
        self.last_sent_text = Some(text);
        self.sent_any = true;
        Ok(true)
    }

    pub(super) fn mark_delivery_attempted(&mut self) {
        self.record_delivery_attempt();
    }

    pub(super) fn defer_delivery_for(&mut self, delay: Duration) {
        self.record_delivery_attempt();
        let retry_after = unix_millis().saturating_add(delay.as_millis());
        self.retry_after_ms = Some(
            self.retry_after_ms
                .map(|existing| existing.max(retry_after))
                .unwrap_or(retry_after),
        );
    }

    pub(super) fn sent_any(&self) -> bool {
        self.sent_any
    }

    fn record_delivery_attempt(&mut self) {
        self.last_flush = Instant::now();
        self.last_flush_at_ms = Some(unix_millis());
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct TelegramActivityItem {
    verb: String,
    target: String,
    command: String,
    status: CommandExecutionStatus,
    exit_code: Option<i64>,
    duration_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    output: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct TelegramActivityState {
    #[serde(default)]
    order: Vec<String>,
    #[serde(default)]
    items: BTreeMap<String, TelegramActivityItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_flush_at_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_sent_text: Option<String>,
}

fn instant_from_unix_millis(timestamp_ms: Option<u128>, default_age: Duration) -> Instant {
    let now = Instant::now();
    let Some(timestamp_ms) = timestamp_ms else {
        return now - default_age;
    };
    let age_ms = unix_millis().saturating_sub(timestamp_ms);
    let age = Duration::from_millis(age_ms.min(u128::from(u64::MAX)) as u64);
    now.checked_sub(age).unwrap_or(now - default_age)
}

fn retry_after_is_active(retry_after_ms: Option<u128>) -> bool {
    retry_after_ms.is_some_and(|retry_after_ms| unix_millis() < retry_after_ms)
}

impl From<CommandExecution> for TelegramActivityItem {
    fn from(command: CommandExecution) -> Self {
        let activity = command_activity(&command);
        Self {
            verb: activity.verb,
            target: truncate_chars(activity.target.trim(), 300),
            command: truncate_chars(command.command.trim(), 240),
            status: command.status,
            exit_code: command.exit_code,
            duration_ms: command.duration_ms,
            output: command
                .aggregated_output
                .as_deref()
                .map(activity_output_text)
                .unwrap_or_default(),
        }
    }
}

impl TelegramActivityItem {
    fn push_output(&mut self, delta: &str) {
        if delta.trim().is_empty() {
            return;
        }
        if !self.output.is_empty() && !self.output.ends_with('\n') {
            self.output.push('\n');
        }
        self.output.push_str(delta.trim_end());
        self.output = activity_output_text(&self.output);
    }
}

pub(super) fn activity_watch_text(panel: &TelegramActivityPanel) -> String {
    let mut lines = Vec::<String>::new();
    let mut explore_group = Vec::<&TelegramActivityItem>::new();
    for item_id in &panel.order {
        if let Some(item) = panel.items.get(item_id) {
            if activity_item_is_explore(item) {
                explore_group.push(item);
            } else {
                if !explore_group.is_empty() {
                    lines.extend(activity_explore_group_lines(&explore_group));
                    explore_group.clear();
                }
                lines.extend(activity_item_lines(item));
            }
        }
    }
    if !explore_group.is_empty() {
        lines.extend(activity_explore_group_lines(&explore_group));
    }
    truncate_chars(&lines.join("\n"), 1800)
}

fn activity_item_lines(item: &TelegramActivityItem) -> Vec<String> {
    if item.verb == "Plan" {
        return activity_plan_lines(item);
    }
    if activity_item_is_explore(item) {
        return activity_explore_group_lines(&[item]);
    }
    if activity_item_is_file_change(item) {
        return activity_file_change_lines(item);
    }
    activity_command_lines(item)
}

fn activity_plan_lines(item: &TelegramActivityItem) -> Vec<String> {
    let mut lines = vec![String::from("• **Updated Plan**")];
    for (index, detail) in item
        .target
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
    {
        let prefix = if index == 0 { "  └ " } else { "    " };
        lines.push(format!("{prefix}{}", detail.trim()));
    }
    lines
}

fn activity_item_is_explore(item: &TelegramActivityItem) -> bool {
    matches!(item.verb.as_str(), "Explore" | "Read" | "List" | "Search")
}

fn activity_item_is_file_change(item: &TelegramActivityItem) -> bool {
    matches!(item.verb.as_str(), "Added" | "Edited" | "Deleted")
}

fn activity_explore_group_lines(items: &[&TelegramActivityItem]) -> Vec<String> {
    let Some(first) = items.first() else {
        return Vec::new();
    };
    let title = activity_explore_group_title(items);
    let metadata = if items.len() == 1 {
        activity_metadata(first)
    } else {
        String::new()
    };
    let mut lines = vec![format!("• **{title}**{metadata}")];
    let detail_lines = activity_explore_group_detail_lines(items);
    for (index, detail) in detail_lines.iter().enumerate() {
        let prefix = if index == 0 { "  └ " } else { "    " };
        lines.push(format!("{prefix}{detail}"));
    }
    lines
}

fn activity_explore_group_title(items: &[&TelegramActivityItem]) -> &'static str {
    if items
        .iter()
        .any(|item| matches!(item.status, CommandExecutionStatus::InProgress))
    {
        return "Exploring";
    }
    if items
        .iter()
        .any(|item| matches!(item.status, CommandExecutionStatus::Failed))
    {
        return "Failed exploring";
    }
    if items
        .iter()
        .all(|item| matches!(item.status, CommandExecutionStatus::Declined))
    {
        return "Declined exploring";
    }
    "Explored"
}

fn activity_file_change_lines(item: &TelegramActivityItem) -> Vec<String> {
    let title = match &item.status {
        CommandExecutionStatus::InProgress => "Editing",
        CommandExecutionStatus::Completed => item.verb.as_str(),
        CommandExecutionStatus::Failed => "Failed editing",
        CommandExecutionStatus::Declined => "Declined editing",
        CommandExecutionStatus::Unknown(_) => item.verb.as_str(),
    };
    let mut target_lines = item.target.lines();
    let first = target_lines.next().unwrap_or("file");
    let mut lines = vec![format!(
        "• **{title}** {}{}",
        activity_code_summary(first),
        activity_metadata(item)
    )];
    for (index, detail) in target_lines.enumerate() {
        let prefix = if index == 0 { "  └ " } else { "    " };
        lines.push(format!("{prefix}{}", activity_code_summary(detail)));
    }
    lines
}

fn activity_command_lines(item: &TelegramActivityItem) -> Vec<String> {
    let title = match &item.status {
        CommandExecutionStatus::InProgress => "Running",
        CommandExecutionStatus::Completed => "Ran",
        CommandExecutionStatus::Failed => "Failed",
        CommandExecutionStatus::Declined => "Declined",
        CommandExecutionStatus::Unknown(_) => "Ran",
    };
    let target = if item.verb == "Tool" {
        activity_code_summary(&item.target)
    } else if !item.command.is_empty() {
        activity_code_summary(&item.command)
    } else {
        activity_code_summary(&item.target)
    };
    let mut lines = vec![format!("• **{title}** {target}{}", activity_metadata(item))];
    for (index, output) in activity_output_lines(&item.output).iter().enumerate() {
        let prefix = if index == 0 { "  └ " } else { "    " };
        lines.push(format!("{prefix}{output}"));
    }
    lines
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExploreDetail {
    verb: String,
    target: String,
}

fn activity_explore_group_detail_lines(items: &[&TelegramActivityItem]) -> Vec<String> {
    let mut merged = Vec::<ExploreDetail>::new();
    for item in items {
        for detail in activity_explore_details(item) {
            if detail.verb == "Read" {
                if let Some(last) = merged.last_mut().filter(|last| last.verb == "Read") {
                    merge_activity_targets(&mut last.target, &detail.target);
                    continue;
                }
            }
            merged.push(detail);
        }
    }
    merged
        .iter()
        .map(activity_format_explore_detail)
        .collect::<Vec<_>>()
}

fn activity_explore_details(item: &TelegramActivityItem) -> Vec<ExploreDetail> {
    if item.verb == "Explore" {
        return item
            .target
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(activity_parse_action_line)
            .collect();
    }
    vec![ExploreDetail {
        verb: item.verb.clone(),
        target: item.target.clone(),
    }]
}

fn activity_parse_action_line(line: &str) -> ExploreDetail {
    let trimmed = line.trim();
    for verb in ["Read", "List", "Search", "Run"] {
        let prefix = format!("{verb} ");
        if let Some(target) = trimmed.strip_prefix(prefix.as_str()) {
            return ExploreDetail {
                verb: verb.to_string(),
                target: target.to_string(),
            };
        }
    }
    ExploreDetail {
        verb: "Run".to_string(),
        target: trimmed.to_string(),
    }
}

fn activity_format_explore_detail(detail: &ExploreDetail) -> String {
    match detail.verb.as_str() {
        "Read" => format!("Read {}", activity_code_targets(&detail.target)),
        "List" => format!("List {}", activity_code_targets(&detail.target)),
        "Search" => format!("Search {}", activity_code_search_target(&detail.target)),
        "Run" => format!("Run {}", activity_code_summary(&detail.target)),
        verb => format!("{verb} {}", activity_code_targets(&detail.target)),
    }
}

fn merge_activity_targets(existing: &mut String, next: &str) {
    let mut targets = split_activity_targets(existing);
    for target in split_activity_targets(next) {
        if !targets.iter().any(|existing| existing == &target) {
            targets.push(target);
        }
    }
    *existing = targets.join(", ");
}

fn split_activity_targets(target: &str) -> Vec<String> {
    target
        .split(", ")
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(str::to_string)
        .collect()
}

fn activity_code_targets(target: &str) -> String {
    target
        .split(", ")
        .map(activity_code_summary)
        .collect::<Vec<_>>()
        .join(", ")
}

fn activity_code_search_target(target: &str) -> String {
    let trimmed = target.trim();
    let trimmed = trimmed.strip_prefix("Search ").unwrap_or(trimmed).trim();
    if let Some((query, path)) = trimmed.rsplit_once(" in ") {
        let query = query.trim();
        let path = path.trim();
        if !query.is_empty() && !path.is_empty() {
            return format!(
                "{} in {}",
                activity_code_summary(query),
                activity_code_summary(path)
            );
        }
    }
    activity_code_summary(trimmed)
}

fn activity_output_text(output: &str) -> String {
    let lines = output
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .take(12)
        .collect::<Vec<_>>();
    truncate_chars(&lines.join("\n"), 700)
}

fn activity_output_lines(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(activity_code_summary)
        .collect()
}

fn activity_code_summary(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "`unknown`".to_string();
    }
    if let Some((path, counts)) = split_diff_count_suffix(trimmed) {
        return format!("`{}` {}", path, counts);
    }
    if let Some((path, suffix)) = split_skill_annotation_suffix(trimmed) {
        return format!("`{}` {}", path, suffix);
    }
    format!("`{trimmed}`")
}

fn split_diff_count_suffix(text: &str) -> Option<(&str, &str)> {
    let start = text.rfind(" (+")?;
    let suffix = &text[start + 1..];
    if !suffix.starts_with("(+") || !suffix.ends_with(')') || !suffix.contains(" -") {
        return None;
    }
    Some((text[..start].trim(), suffix))
}

fn split_skill_annotation_suffix(text: &str) -> Option<(&str, &str)> {
    let start = text.rfind(" (")?;
    let suffix = &text[start + 1..];
    if !suffix.ends_with(" skill)") {
        return None;
    }
    let path = text[..start].trim();
    if path.is_empty() {
        return None;
    }
    Some((path, suffix))
}

fn activity_metadata(item: &TelegramActivityItem) -> String {
    let mut metadata = Vec::new();
    if let CommandExecutionStatus::Unknown(status) = &item.status {
        metadata.push(status.clone());
    }
    if let Some(exit_code) = item.exit_code.filter(|code| *code != 0) {
        metadata.push(format!("exit {exit_code}"));
    }
    if let Some(duration_ms) = item.duration_ms.filter(|duration| *duration > 0) {
        metadata.push(format_duration_ms(duration_ms));
    }
    if metadata.is_empty() {
        String::new()
    } else {
        format!(" ({})", metadata.join(", "))
    }
}

fn command_activity(command: &CommandExecution) -> CommandActivity {
    if let Some(activity) = command.activity.as_ref() {
        return activity.clone();
    }
    command_activity_from_shell(command.command.trim())
}

fn command_activity_from_shell(command: &str) -> CommandActivity {
    let trimmed = command.trim();
    let first_word = trimmed.split_whitespace().next().unwrap_or("");
    let verb = match first_word {
        "cargo" if trimmed.starts_with("cargo test") => "Test",
        "cargo" if trimmed.starts_with("cargo build") => "Build",
        "cargo" if trimmed.starts_with("cargo clippy") => "Lint",
        "cargo" if trimmed.starts_with("cargo fmt") => "Format",
        "git"
            if trimmed.starts_with("git status")
                || trimmed.starts_with("git diff")
                || trimmed.starts_with("git log")
                || trimmed.starts_with("git show") =>
        {
            "Read"
        }
        "rg" | "grep" => "Search",
        "sed" | "cat" | "tail" | "head" | "ls" | "find" | "wc" => "Read",
        "git" if trimmed.starts_with("git add") => "Stage",
        "git" if trimmed.starts_with("git commit") => "Commit",
        "git" if trimmed.starts_with("git push") => "Push",
        "mkdir" | "touch" | "cp" | "mv" | "rsync" | "install" => "Write",
        _ => "Run",
    };
    CommandActivity {
        verb: verb.to_string(),
        target: command_activity_target(verb, trimmed),
    }
}

fn command_activity_target(verb: &str, command: &str) -> String {
    if verb == "Read" && command.starts_with("git ") {
        return command
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
    }
    if verb == "Read" || verb == "Write" {
        if let Some(path) = command
            .split_whitespace()
            .last()
            .and_then(command_path_label)
        {
            return path.to_string();
        }
    }
    command.to_string()
}

fn format_duration_ms(duration_ms: i64) -> String {
    if duration_ms >= 1000 {
        format!("{:.1}s", duration_ms as f64 / 1000.0)
    } else {
        format!("{duration_ms}ms")
    }
}
