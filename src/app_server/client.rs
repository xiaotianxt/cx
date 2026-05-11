use std::collections::VecDeque;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use super::protocol;
use super::protocol::ServerMessage;
use super::transport::LoopbackWsUrl;
use super::transport::WebSocket;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitializeInfo {
    pub user_agent: String,
    pub codex_home: String,
    pub platform_family: String,
    pub platform_os: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThreadListInfo {
    pub thread_count: usize,
    pub has_next_cursor: bool,
    pub has_backwards_cursor: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThreadListPage {
    pub threads: Vec<AppThreadSummary>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ThreadListFilter<'a> {
    pub limit: u64,
    pub cwd: Option<&'a str>,
    pub archived: Option<bool>,
    pub source_kinds: Option<&'a [&'a str]>,
    pub use_state_db_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppThreadSummary {
    pub upstream_thread_id: String,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub preview: String,
    pub cwd: String,
    pub path: Option<String>,
    pub source: String,
    pub status: String,
    pub active_turn_id: Option<String>,
    pub active: bool,
    pub created_at_unix: i64,
    pub updated_at_unix: i64,
    pub broker_subscriber_count: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AppThreadRead {
    pub summary: AppThreadSummary,
    pub turns: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartedThread {
    pub upstream_thread_id: String,
    pub path: Option<String>,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartedTurn {
    pub turn_id: String,
    pub assistant_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SteeredTurn {
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterruptOutcome {
    pub interrupted_turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppStreamEvent {
    UserMessage(String),
    TurnStarted,
    Info(String),
    ReasoningStarted,
    ReasoningDelta(String),
    AgentDelta(String),
    CommandStarted(CommandExecution),
    CommandOutputDelta { item_id: String, delta: String },
    CommandCompleted(CommandExecution),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandExecution {
    pub item_id: String,
    pub command: String,
    pub cwd: String,
    pub activity: Option<CommandActivity>,
    pub status: CommandExecutionStatus,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub aggregated_output: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandActivity {
    pub verb: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CommandExecutionStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApprovalRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ParsedServerEvent {
    Stream(AppStreamEvent),
    ApprovalRequest(ApprovalRequest),
    TurnCompleted { turn_id: String },
}

#[derive(Debug, Clone, Copy)]
struct ApprovalScope<'a> {
    thread_id: &'a str,
    turn_id: Option<&'a str>,
}

const MAX_QUEUED_APPROVALS: usize = 64;

impl ThreadListInfo {
    fn from_response(response: &Value) -> Self {
        let thread_count = response
            .get("data")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        Self {
            thread_count,
            has_next_cursor: !response.get("nextCursor").is_none_or(Value::is_null),
            has_backwards_cursor: !response.get("backwardsCursor").is_none_or(Value::is_null),
        }
    }
}

pub(crate) struct AppServerClient {
    websocket: WebSocket,
    next_request_id: u64,
    queued_approvals: VecDeque<ApprovalRequest>,
    closed_reason: Option<String>,
}

impl AppServerClient {
    pub(crate) fn connect(url: &str, timeout: Duration) -> Result<Self> {
        let url = LoopbackWsUrl::parse(url)?;
        let websocket = WebSocket::connect(&url, timeout)?;
        Ok(Self {
            websocket,
            next_request_id: 1,
            queued_approvals: VecDeque::new(),
            closed_reason: None,
        })
    }

    pub(crate) fn initialize(
        &mut self,
        client_name: &str,
        client_version: &str,
    ) -> Result<InitializeInfo> {
        let response = self.request(
            "initialize",
            protocol::InitializeParams {
                client_info: protocol::ClientInfo {
                    name: client_name.to_string(),
                    version: client_version.to_string(),
                },
                capabilities: serde_json::Map::from_iter([(
                    String::from("experimentalApi"),
                    Value::Bool(true),
                )]),
            },
        )?;
        let response = serde_json::from_value::<protocol::InitializeResponse>(response)
            .context("decode initialize response")?;
        Ok(InitializeInfo {
            user_agent: response.user_agent,
            codex_home: response.codex_home,
            platform_family: response.platform_family,
            platform_os: response.platform_os,
        })
    }

    pub(crate) fn thread_list_probe(&mut self) -> Result<ThreadListInfo> {
        let response = self.request(
            "thread/list",
            protocol::ThreadListParams {
                cursor: None,
                limit: Some(1),
                sort_key: Some(String::from("updated_at")),
                sort_direction: Some(String::from("desc")),
                model_providers: None,
                source_kinds: None,
                archived: Some(false),
                cwd: None,
                use_state_db_only: true,
                search_term: None,
            },
        )?;
        Ok(ThreadListInfo::from_response(&response))
    }

    pub(crate) fn thread_list(&mut self, limit: u64) -> Result<ThreadListPage> {
        self.thread_list_filtered(ThreadListFilter {
            limit,
            cwd: None,
            archived: Some(false),
            source_kinds: None,
            use_state_db_only: true,
        })
    }

    pub(crate) fn thread_list_filtered(
        &mut self,
        filter: ThreadListFilter<'_>,
    ) -> Result<ThreadListPage> {
        let response = self.request(
            "thread/list",
            protocol::ThreadListParams {
                cursor: None,
                limit: Some(filter.limit.min(u64::from(u32::MAX)) as u32),
                sort_key: Some(String::from("updated_at")),
                sort_direction: Some(String::from("desc")),
                model_providers: None,
                source_kinds: filter
                    .source_kinds
                    .map(|kinds| kinds.iter().map(|kind| (*kind).to_string()).collect()),
                archived: filter.archived,
                cwd: filter
                    .cwd
                    .map(|cwd| protocol::ThreadListCwdFilter::One(cwd.to_string())),
                use_state_db_only: filter.use_state_db_only,
                search_term: None,
            },
        )?;
        let response = serde_json::from_value::<protocol::ThreadListResponse>(response)
            .context("decode thread/list response")?;
        Ok(ThreadListPage {
            threads: response
                .data
                .into_iter()
                .map(AppThreadSummary::from)
                .collect(),
            next_cursor: response.next_cursor,
            backwards_cursor: response.backwards_cursor,
        })
    }

    pub(crate) fn thread_start(&mut self, cwd: Option<&str>) -> Result<StartedThread> {
        let response = self.request(
            "thread/start",
            protocol::ThreadStartParams {
                session_start_source: Some(String::from("startup")),
                cwd: cwd.map(str::to_string),
            },
        )?;
        let response = serde_json::from_value::<protocol::ThreadStartResponse>(response)
            .context("decode thread/start response")?;
        Ok(StartedThread {
            upstream_thread_id: response.thread.id.clone(),
            path: response.thread.path,
            cwd: response.thread.cwd,
        })
    }

    pub(crate) fn thread_resume(&mut self, thread_id: &str, cwd: Option<&str>) -> Result<()> {
        self.thread_resume_with_path(thread_id, None, cwd, false)
            .map(|_| ())
    }

    pub(crate) fn thread_resume_with_path(
        &mut self,
        thread_id: &str,
        path: Option<&str>,
        cwd: Option<&str>,
        exclude_turns: bool,
    ) -> Result<AppThreadRead> {
        let response = self.request(
            "thread/resume",
            protocol::ThreadResumeParams {
                thread_id: thread_id.to_string(),
                path: path.map(str::to_string),
                cwd: cwd.map(str::to_string),
                exclude_turns,
            },
        )?;
        let response = serde_json::from_value::<protocol::ThreadResumeResponse>(response)
            .context("decode thread/resume response")?;
        let turns = response.thread.turns.clone();
        Ok(AppThreadRead {
            summary: AppThreadSummary::from(response.thread),
            turns,
        })
    }

    pub(crate) fn thread_read(
        &mut self,
        thread_id: &str,
        include_turns: bool,
    ) -> Result<AppThreadRead> {
        let response = self.request(
            "thread/read",
            protocol::ThreadReadParams {
                thread_id: thread_id.to_string(),
                include_turns,
            },
        )?;
        let response = serde_json::from_value::<protocol::ThreadReadResponse>(response)
            .context("decode thread/read response")?;
        let turns = response.thread.turns.clone();
        Ok(AppThreadRead {
            summary: AppThreadSummary::from(response.thread),
            turns,
        })
    }

    pub(crate) fn thread_unsubscribe(&mut self, thread_id: &str) -> Result<()> {
        let _response = self.request(
            "thread/unsubscribe",
            protocol::ThreadUnsubscribeParams {
                thread_id: thread_id.to_string(),
            },
        )?;
        Ok(())
    }

    pub(crate) fn interrupt_active_turn(&mut self, thread_id: &str) -> Result<InterruptOutcome> {
        let Some(turn_id) = self.active_turn_id(thread_id)? else {
            return Ok(InterruptOutcome {
                interrupted_turn_id: None,
            });
        };
        let _response = self.request(
            "turn/interrupt",
            protocol::TurnInterruptParams {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.clone(),
            },
        )?;
        Ok(InterruptOutcome {
            interrupted_turn_id: Some(turn_id),
        })
    }

    pub(crate) fn turn_start_stream<F>(
        &mut self,
        thread_id: &str,
        prompt: String,
        mut on_event: F,
        mut on_approval: impl FnMut(ApprovalRequest) -> Result<Value>,
    ) -> Result<StartedTurn>
    where
        F: FnMut(AppStreamEvent) -> Result<()>,
    {
        self.ensure_open()?;
        self.handle_queued_approvals(
            ApprovalScope {
                thread_id,
                turn_id: None,
            },
            &mut on_approval,
        )?;
        let request_id = self.send_request(
            "turn/start",
            protocol::TurnStartParams {
                thread_id: thread_id.to_string(),
                input: vec![protocol::UserInput::Text {
                    text: prompt,
                    text_elements: Vec::new(),
                }],
                summary: Some(String::from("auto")),
            },
        )?;
        self.collect_turn_start(request_id, thread_id, &mut on_event, &mut on_approval)
    }

    pub(crate) fn turn_steer_with_approval(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        prompt: String,
        mut on_approval: impl FnMut(ApprovalRequest) -> Result<Value>,
    ) -> Result<SteeredTurn> {
        self.ensure_open()?;
        let scope = ApprovalScope {
            thread_id,
            turn_id: Some(turn_id),
        };
        self.handle_queued_approvals(scope, &mut on_approval)?;
        let response = self.request_with_approval(
            "turn/steer",
            protocol::TurnSteerParams {
                thread_id: thread_id.to_string(),
                input: vec![protocol::UserInput::Text {
                    text: prompt,
                    text_elements: Vec::new(),
                }],
                expected_turn_id: turn_id.to_string(),
            },
            scope,
            &mut on_approval,
        )?;
        let response = serde_json::from_value::<protocol::TurnSteerResponse>(response)
            .context("decode turn/steer response")?;
        Ok(SteeredTurn {
            turn_id: response.turn_id,
        })
    }

    pub(crate) fn drain_thread_events<F>(
        &mut self,
        thread_id: &str,
        turn_id: Option<&str>,
        max_events: usize,
        mut on_event: F,
        mut on_approval: impl FnMut(ApprovalRequest) -> Result<Option<Value>>,
    ) -> Result<usize>
    where
        F: FnMut(ParsedServerEvent) -> Result<()>,
    {
        self.ensure_open()?;
        let mut count = 0usize;
        let scope = ApprovalScope { thread_id, turn_id };
        while count < max_events {
            let Some(approval) = self.pop_queued_approval(scope) else {
                break;
            };
            if let Some(result) = on_approval(approval.clone())? {
                self.send_success_response(approval.id.clone(), result)?;
            }
            on_event(ParsedServerEvent::ApprovalRequest(approval))?;
            count += 1;
        }
        while count < max_events {
            let Some(frame) = self.websocket.read_text_optional()? else {
                return Ok(count);
            };
            let value =
                serde_json::from_str::<Value>(&frame).context("decode app-server event frame")?;
            if let Some(event) = parse_server_event(&value, thread_id, turn_id) {
                if let ParsedServerEvent::ApprovalRequest(approval) = &event {
                    if let Some(result) = on_approval(approval.clone())? {
                        self.send_success_response(approval.id.clone(), result)?;
                    }
                }
                on_event(event)?;
                count += 1;
                continue;
            }
            let message = serde_json::from_value::<ServerMessage>(value)
                .context("decode app-server drain message")?;
            if let ServerMessage::Request(request) = message {
                if let Some(approval) = ApprovalRequest::from_server_request(&request) {
                    self.enqueue_approval(approval)?;
                } else {
                    self.send_error_response(
                        request.id,
                        -32601,
                        format!("unsupported app-server request: {}", request.method),
                    )?;
                }
            }
        }
        Ok(count)
    }

    pub(crate) fn active_turn_id(&mut self, thread_id: &str) -> Result<Option<String>> {
        Ok(self.thread_read(thread_id, true)?.summary.active_turn_id)
    }

    fn request<P>(&mut self, method: &'static str, params: P) -> Result<Value>
    where
        P: Serialize,
    {
        let id = self.send_request(method, params)?;
        self.read_response(method, id, None, None)
    }

    fn request_with_approval<P>(
        &mut self,
        method: &'static str,
        params: P,
        scope: ApprovalScope<'_>,
        on_approval: &mut impl FnMut(ApprovalRequest) -> Result<Value>,
    ) -> Result<Value>
    where
        P: Serialize,
    {
        let id = self.send_request(method, params)?;
        self.read_response(method, id, Some(scope), Some(on_approval))
    }

    fn send_request<P>(&mut self, method: &'static str, params: P) -> Result<u64>
    where
        P: Serialize,
    {
        self.ensure_open()?;
        let id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .context("app-server request id overflow")?;
        let request = protocol::ClientRequest { id, method, params };
        let text = serde_json::to_string(&request).context("encode app-server request")?;
        self.websocket.send_text(&text)?;
        Ok(id)
    }

    fn read_response(
        &mut self,
        method: &'static str,
        id: u64,
        approval_scope: Option<ApprovalScope<'_>>,
        mut on_approval: Option<&mut dyn FnMut(ApprovalRequest) -> Result<Value>>,
    ) -> Result<Value> {
        loop {
            let text = self.websocket.read_text()?;
            let message = serde_json::from_str::<ServerMessage>(&text)
                .with_context(|| format!("decode app-server message for {method}"))?;
            match message {
                ServerMessage::Response(response) if response.id == id => {
                    if let Some(error) = response.error {
                        let mut message = format!(
                            "app-server request {method} failed: {} ({})",
                            error.message, error.code
                        );
                        if let Some(data) = error.data {
                            message.push_str(&format!(": {data}"));
                        }
                        anyhow::bail!(message);
                    }
                    return response.result.with_context(|| {
                        format!("app-server response for {method} omitted result")
                    });
                }
                ServerMessage::Response(_) => {}
                ServerMessage::Request(request) => {
                    if let Some(approval) = ApprovalRequest::from_server_request(&request) {
                        if approval_scope.is_some_and(|scope| approval.matches_scope(scope)) {
                            let Some(on_approval) = on_approval.as_deref_mut() else {
                                self.enqueue_approval(approval)?;
                                continue;
                            };
                            let id = approval.id.clone();
                            let result = match on_approval(approval) {
                                Ok(result) => result,
                                Err(err) => {
                                    self.send_error_response(
                                        id,
                                        -32000,
                                        format!("approval failed: {err:#}"),
                                    )?;
                                    return Err(err);
                                }
                            };
                            self.send_success_response(id, result)?;
                        } else {
                            self.enqueue_approval(approval)?;
                        }
                    } else {
                        self.send_error_response(
                            request.id,
                            -32601,
                            format!(
                                "unsupported app-server request during {method}: {}",
                                request.method
                            ),
                        )?;
                    }
                }
                ServerMessage::Notification { .. } => {}
            }
        }
    }

    fn handle_queued_approvals(
        &mut self,
        scope: ApprovalScope<'_>,
        on_approval: &mut impl FnMut(ApprovalRequest) -> Result<Value>,
    ) -> Result<()> {
        while let Some(approval) = self.pop_queued_approval(scope) {
            let id = approval.id.clone();
            let result = match on_approval(approval) {
                Ok(result) => result,
                Err(err) => {
                    self.send_error_response(id, -32000, format!("approval failed: {err:#}"))?;
                    return Err(err);
                }
            };
            self.send_success_response(id, result)?;
        }
        Ok(())
    }

    fn enqueue_approval(&mut self, approval: ApprovalRequest) -> Result<()> {
        if self.queued_approvals.len() >= MAX_QUEUED_APPROVALS {
            return self.close_with_reason(format!(
                "approval queue is full (limit {MAX_QUEUED_APPROVALS})"
            ));
        }
        self.queued_approvals.push_back(approval);
        Ok(())
    }

    fn pop_queued_approval(&mut self, scope: ApprovalScope<'_>) -> Option<ApprovalRequest> {
        let mut retained = VecDeque::new();
        let mut found = None;
        while let Some(approval) = self.queued_approvals.pop_front() {
            if found.is_none() && approval.matches_scope(scope) {
                found = Some(approval);
                break;
            }
            retained.push_back(approval);
        }
        retained.append(&mut self.queued_approvals);
        self.queued_approvals = retained;
        found
    }

    fn close_with_reason<T>(&mut self, reason: String) -> Result<T> {
        if self.closed_reason.is_none() {
            self.closed_reason = Some(reason.clone());
            let _ = self.websocket.shutdown();
        }
        anyhow::bail!("app-server client is closed: {reason}");
    }

    fn ensure_open(&self) -> Result<()> {
        if let Some(reason) = self.closed_reason.as_deref() {
            anyhow::bail!("app-server client is closed: {reason}");
        }
        Ok(())
    }

    fn collect_turn_start<F>(
        &mut self,
        request_id: u64,
        thread_id: &str,
        on_event: &mut F,
        on_approval: &mut impl FnMut(ApprovalRequest) -> Result<Value>,
    ) -> Result<StartedTurn>
    where
        F: FnMut(AppStreamEvent) -> Result<()>,
    {
        let mut turn_id = None::<String>;
        let mut assistant_text = String::new();
        let mut completed = false;
        let mut start_response_received = false;
        loop {
            let frame = self.websocket.read_text()?;
            let message = serde_json::from_str::<ServerMessage>(&frame)
                .context("decode app-server turn notification")?;
            match message {
                ServerMessage::Response(response) if response.id == request_id => {
                    if let Some(error) = response.error {
                        anyhow::bail!(
                            "app-server request turn/start failed: {} ({})",
                            error.message,
                            error.code
                        );
                    }
                    let response = response
                        .result
                        .context("app-server response for turn/start omitted result")?;
                    let response = serde_json::from_value::<protocol::TurnStartResponse>(response)
                        .context("decode turn/start response")?;
                    let response_turn_id = response.turn.id;
                    start_response_received = true;
                    if completed {
                        return Ok(StartedTurn {
                            turn_id: response_turn_id,
                            assistant_text,
                        });
                    }
                    turn_id = Some(response_turn_id);
                }
                ServerMessage::Notification { method, params } => {
                    if method == "turn/started" {
                        if let Some(started_turn_id) = started_turn_id(&params, thread_id) {
                            if turn_id.as_deref().is_none_or(|id| id == started_turn_id) {
                                turn_id = Some(started_turn_id.to_string());
                                on_event(AppStreamEvent::TurnStarted)?;
                            }
                        }
                    } else if method == "thread/compacted" {
                        if notification_params(&params, thread_id, turn_id.as_deref()).is_some() {
                            on_event(AppStreamEvent::Info("Context compacted".to_string()))?;
                        }
                    } else if method == "item/reasoning/summaryPartAdded" {
                        if notification_params(&params, thread_id, turn_id.as_deref()).is_some() {
                            on_event(AppStreamEvent::ReasoningStarted)?;
                        }
                    } else if method == "item/reasoning/summaryTextDelta"
                        || method == "item/reasoning/textDelta"
                        || method == "item/plan/delta"
                    {
                        if let Some(delta) =
                            notification_text_delta(&params, thread_id, turn_id.as_deref())
                        {
                            on_event(AppStreamEvent::ReasoningDelta(delta))?;
                        }
                    } else if method == "item/agentMessage/delta" {
                        if let Some(delta) =
                            notification_delta(&params, thread_id, turn_id.as_deref())
                        {
                            assistant_text.push_str(delta);
                            on_event(AppStreamEvent::AgentDelta(delta.to_string()))?;
                        }
                    } else if method == "item/started" {
                        if let Some(command) =
                            notification_command_execution(&params, thread_id, turn_id.as_deref())
                        {
                            on_event(AppStreamEvent::CommandStarted(command))?;
                        }
                    } else if method == "item/commandExecution/outputDelta" {
                        if let Some((item_id, delta)) = notification_command_output_delta(
                            &params,
                            thread_id,
                            turn_id.as_deref(),
                        ) {
                            on_event(AppStreamEvent::CommandOutputDelta { item_id, delta })?;
                        }
                    } else if method == "item/completed" {
                        if let Some(command) =
                            notification_command_execution(&params, thread_id, turn_id.as_deref())
                        {
                            on_event(AppStreamEvent::CommandCompleted(command))?;
                        }
                    } else if method == "turn/completed" {
                        if let Some(completed_turn_id) = completed_turn_id(&params, thread_id) {
                            if turn_id.as_deref().is_none_or(|id| id == completed_turn_id) {
                                completed = true;
                                if start_response_received {
                                    let turn_id = turn_id
                                        .clone()
                                        .unwrap_or_else(|| completed_turn_id.to_string());
                                    return Ok(StartedTurn {
                                        turn_id,
                                        assistant_text,
                                    });
                                }
                            }
                        }
                    } else if method == "error" {
                        if let Some(params) = params {
                            anyhow::bail!("app-server turn error: {params}");
                        }
                        anyhow::bail!("app-server turn error");
                    }
                }
                ServerMessage::Response(_) => {}
                ServerMessage::Request(request) => {
                    if let Some(approval) = ApprovalRequest::from_server_request(&request) {
                        let scope = ApprovalScope {
                            thread_id,
                            turn_id: turn_id.as_deref(),
                        };
                        if !approval.matches_scope(scope) {
                            self.enqueue_approval(approval)?;
                            continue;
                        }
                        let id = approval.id.clone();
                        let result = match on_approval(approval) {
                            Ok(result) => result,
                            Err(err) => {
                                self.send_error_response(
                                    id,
                                    -32000,
                                    format!("approval failed: {err:#}"),
                                )?;
                                return Err(err);
                            }
                        };
                        self.send_success_response(id, result)?;
                    } else {
                        self.send_error_response(
                            request.id,
                            -32601,
                            format!("unsupported app-server request: {}", request.method),
                        )?;
                    }
                }
            }
        }
    }

    fn send_success_response(&mut self, id: Value, result: Value) -> Result<()> {
        let response = json!({
            "id": id,
            "result": result,
        });
        self.websocket
            .send_text(&serde_json::to_string(&response).context("encode app-server response")?)
    }

    fn send_error_response(&mut self, id: Value, code: i64, message: String) -> Result<()> {
        let response = json!({
            "id": id,
            "error": {
                "code": code,
                "message": message,
            },
        });
        self.websocket.send_text(
            &serde_json::to_string(&response).context("encode app-server error response")?,
        )
    }
}

impl ApprovalRequest {
    fn from_server_request(request: &protocol::ServerRequest) -> Option<Self> {
        if !is_approval_request_method(&request.method) {
            return None;
        }
        Some(Self {
            id: request.id.clone(),
            method: request.method.clone(),
            params: request.params.clone().unwrap_or(Value::Null),
        })
    }

    fn matches_scope(&self, scope: ApprovalScope<'_>) -> bool {
        if self.params.get("threadId").and_then(Value::as_str) != Some(scope.thread_id) {
            return false;
        }
        match (
            scope.turn_id,
            self.params.get("turnId").and_then(Value::as_str),
        ) {
            (Some(expected), Some(actual)) => actual == expected,
            _ => true,
        }
    }
}

fn is_approval_request_method(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "execCommandApproval"
            | "applyPatchApproval"
    )
}

pub(crate) fn parse_server_event(
    message: &Value,
    thread_id: &str,
    turn_id: Option<&str>,
) -> Option<ParsedServerEvent> {
    let message = serde_json::from_value::<ServerMessage>(message.clone()).ok()?;
    match message {
        ServerMessage::Request(request)
            if is_approval_request_method(&request.method)
                && request_params_match_thread(&request.params, thread_id, turn_id) =>
        {
            Some(ParsedServerEvent::ApprovalRequest(ApprovalRequest {
                id: request.id,
                method: request.method,
                params: request.params.unwrap_or(Value::Null),
            }))
        }
        ServerMessage::Notification { method, params }
            if method == "turn/started" || method == "task/started" =>
        {
            started_turn_id(&params, thread_id)
                .filter(|started| turn_id.is_none_or(|expected| *started == expected))
                .map(|_| ParsedServerEvent::Stream(AppStreamEvent::TurnStarted))
        }
        ServerMessage::Notification { method, params }
            if method == "item/reasoning/summaryPartAdded"
                && notification_params(&params, thread_id, turn_id).is_some() =>
        {
            Some(ParsedServerEvent::Stream(AppStreamEvent::ReasoningStarted))
        }
        ServerMessage::Notification { method, params }
            if method == "thread/compacted"
                && notification_params(&params, thread_id, turn_id).is_some() =>
        {
            Some(ParsedServerEvent::Stream(AppStreamEvent::Info(
                "Context compacted".to_string(),
            )))
        }
        ServerMessage::Notification { method, params }
            if method == "item/reasoning/summaryTextDelta"
                || method == "item/reasoning/textDelta"
                || method == "item/plan/delta" =>
        {
            notification_text_delta(&params, thread_id, turn_id)
                .map(AppStreamEvent::ReasoningDelta)
                .map(ParsedServerEvent::Stream)
        }
        ServerMessage::Notification { method, params } if method == "item/agentMessage/delta" => {
            notification_delta(&params, thread_id, turn_id)
                .map(str::to_string)
                .map(AppStreamEvent::AgentDelta)
                .map(ParsedServerEvent::Stream)
        }
        ServerMessage::Notification { method, params } if method == "item/started" => {
            notification_command_execution(&params, thread_id, turn_id)
                .map(AppStreamEvent::CommandStarted)
                .map(ParsedServerEvent::Stream)
        }
        ServerMessage::Notification { method, params }
            if method == "item/commandExecution/outputDelta" =>
        {
            notification_command_output_delta(&params, thread_id, turn_id)
                .map(|(item_id, delta)| AppStreamEvent::CommandOutputDelta { item_id, delta })
                .map(ParsedServerEvent::Stream)
        }
        ServerMessage::Notification { method, params } if method == "item/completed" => {
            notification_command_execution(&params, thread_id, turn_id)
                .map(AppStreamEvent::CommandCompleted)
                .map(ParsedServerEvent::Stream)
        }
        ServerMessage::Notification { method, params } if method == "turn/completed" => {
            completed_turn_id(&params, thread_id)
                .filter(|completed| turn_id.is_none_or(|expected| *completed == expected))
                .map(|completed| ParsedServerEvent::TurnCompleted {
                    turn_id: completed.to_string(),
                })
        }
        _ => None,
    }
}

fn request_params_match_thread(
    params: &Option<Value>,
    thread_id: &str,
    turn_id: Option<&str>,
) -> bool {
    let Some(params) = params else {
        return false;
    };
    if params.get("threadId").and_then(Value::as_str) != Some(thread_id) {
        return false;
    }
    match (turn_id, params.get("turnId").and_then(Value::as_str)) {
        (Some(expected), Some(actual)) => actual == expected,
        _ => true,
    }
}

fn notification_delta<'a>(
    params: &'a Option<Value>,
    thread_id: &str,
    turn_id: Option<&str>,
) -> Option<&'a str> {
    let params = notification_params(params, thread_id, turn_id)?;
    params.get("delta").and_then(Value::as_str)
}

fn notification_command_execution(
    params: &Option<Value>,
    thread_id: &str,
    turn_id: Option<&str>,
) -> Option<CommandExecution> {
    let params = notification_params(params, thread_id, turn_id)?;
    command_execution_from_item(params.get("item")?)
}

fn notification_command_output_delta(
    params: &Option<Value>,
    thread_id: &str,
    turn_id: Option<&str>,
) -> Option<(String, String)> {
    let params = notification_params(params, thread_id, turn_id)?;
    let item_id = params.get("itemId")?.as_str()?.to_string();
    let delta = params.get("delta")?.as_str()?.to_string();
    Some((item_id, delta))
}

fn notification_text_delta(
    params: &Option<Value>,
    thread_id: &str,
    turn_id: Option<&str>,
) -> Option<String> {
    let params = notification_params(params, thread_id, turn_id)?;
    if let Some(delta) = params.get("delta").and_then(Value::as_str) {
        return Some(delta.to_string());
    }
    let encoded = params.get("deltaBase64").and_then(Value::as_str)?;
    let bytes = STANDARD.decode(encoded).ok()?;
    String::from_utf8(bytes).ok()
}

fn notification_params<'a>(
    params: &'a Option<Value>,
    thread_id: &str,
    turn_id: Option<&str>,
) -> Option<&'a Value> {
    let params = params.as_ref()?;
    if params.get("threadId").and_then(Value::as_str)? != thread_id {
        return None;
    }
    if let Some(turn_id) = turn_id {
        if params.get("turnId").and_then(Value::as_str)? != turn_id {
            return None;
        }
    }
    Some(params)
}

fn command_execution_from_item(item: &Value) -> Option<CommandExecution> {
    match item.get("type").and_then(Value::as_str)? {
        "commandExecution" => command_execution_activity_from_item(item),
        "fileChange" => file_change_activity_from_item(item),
        "plan" => plan_activity_from_item(item),
        "mcpToolCall" => mcp_tool_activity_from_item(item),
        "dynamicToolCall" => dynamic_tool_activity_from_item(item),
        "webSearch" => web_search_activity_from_item(item),
        "imageView" => image_view_activity_from_item(item),
        "imageGeneration" => image_generation_activity_from_item(item),
        _ => None,
    }
}

fn command_execution_activity_from_item(item: &Value) -> Option<CommandExecution> {
    Some(CommandExecution {
        item_id: item.get("id")?.as_str()?.to_string(),
        command: item.get("command")?.as_str()?.to_string(),
        cwd: item.get("cwd")?.as_str()?.to_string(),
        activity: command_activity_from_actions(item.get("commandActions")),
        status: command_execution_status(item.get("status")?.as_str()?),
        exit_code: item.get("exitCode").and_then(Value::as_i64),
        duration_ms: item.get("durationMs").and_then(Value::as_i64),
        aggregated_output: item
            .get("aggregatedOutput")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn plan_activity_from_item(item: &Value) -> Option<CommandExecution> {
    Some(CommandExecution {
        item_id: item.get("id")?.as_str()?.to_string(),
        command: "update_plan".to_string(),
        cwd: String::new(),
        activity: Some(CommandActivity {
            verb: "Plan".to_string(),
            target: item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("Updated plan")
                .to_string(),
        }),
        status: CommandExecutionStatus::Completed,
        exit_code: None,
        duration_ms: None,
        aggregated_output: None,
    })
}

fn file_change_activity_from_item(item: &Value) -> Option<CommandExecution> {
    let changes = item.get("changes")?.as_array()?;
    let status = command_execution_status(item.get("status")?.as_str()?);
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut kinds = Vec::<&str>::new();
    let mut details = Vec::<String>::new();

    for change in changes {
        let path = change.get("path").and_then(Value::as_str).unwrap_or("file");
        let kind = change
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("update");
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
        let (change_added, change_removed) =
            diff_line_counts(change.get("diff").and_then(Value::as_str).unwrap_or(""));
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

    Some(CommandExecution {
        item_id: item.get("id")?.as_str()?.to_string(),
        command: "apply patch".to_string(),
        cwd: item
            .get("cwd")
            .or_else(|| item.get("workdir"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        activity: Some(CommandActivity {
            verb: verb.to_string(),
            target,
        }),
        status,
        exit_code: None,
        duration_ms: None,
        aggregated_output: None,
    })
}

fn mcp_tool_activity_from_item(item: &Value) -> Option<CommandExecution> {
    let server = item.get("server").and_then(Value::as_str).unwrap_or("mcp");
    let tool = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
    Some(CommandExecution {
        item_id: item.get("id")?.as_str()?.to_string(),
        command: format!("{server}.{tool}"),
        cwd: String::new(),
        activity: Some(CommandActivity {
            verb: "Tool".to_string(),
            target: format!("{server}.{tool}"),
        }),
        status: command_execution_status(item.get("status")?.as_str()?),
        exit_code: None,
        duration_ms: item.get("durationMs").and_then(Value::as_i64),
        aggregated_output: None,
    })
}

fn dynamic_tool_activity_from_item(item: &Value) -> Option<CommandExecution> {
    let namespace = item.get("namespace").and_then(Value::as_str);
    let tool = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
    let target = namespace
        .map(|namespace| format!("{namespace}.{tool}"))
        .unwrap_or_else(|| tool.to_string());
    let success = item.get("success").and_then(Value::as_bool);
    let status = match (item.get("status").and_then(Value::as_str), success) {
        (Some("completed"), Some(false)) => CommandExecutionStatus::Failed,
        (Some(status), _) => command_execution_status(status),
        _ => CommandExecutionStatus::Unknown("unknown".to_string()),
    };
    Some(CommandExecution {
        item_id: item.get("id")?.as_str()?.to_string(),
        command: target.clone(),
        cwd: String::new(),
        activity: Some(CommandActivity {
            verb: "Tool".to_string(),
            target,
        }),
        status,
        exit_code: None,
        duration_ms: item.get("durationMs").and_then(Value::as_i64),
        aggregated_output: None,
    })
}

fn web_search_activity_from_item(item: &Value) -> Option<CommandExecution> {
    let target = item
        .get("query")
        .and_then(Value::as_str)
        .or_else(|| {
            item.get("action")
                .and_then(|action| action.get("query"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            item.get("action")
                .and_then(|action| action.get("url"))
                .and_then(Value::as_str)
        })
        .unwrap_or("web");
    Some(CommandExecution {
        item_id: item.get("id")?.as_str()?.to_string(),
        command: target.to_string(),
        cwd: String::new(),
        activity: Some(CommandActivity {
            verb: "Search".to_string(),
            target: target.to_string(),
        }),
        status: CommandExecutionStatus::Completed,
        exit_code: None,
        duration_ms: None,
        aggregated_output: None,
    })
}

fn image_view_activity_from_item(item: &Value) -> Option<CommandExecution> {
    let path = item.get("path").and_then(Value::as_str).unwrap_or("image");
    Some(CommandExecution {
        item_id: item.get("id")?.as_str()?.to_string(),
        command: path.to_string(),
        cwd: String::new(),
        activity: Some(CommandActivity {
            verb: "View".to_string(),
            target: path.to_string(),
        }),
        status: CommandExecutionStatus::Completed,
        exit_code: None,
        duration_ms: None,
        aggregated_output: None,
    })
}

fn image_generation_activity_from_item(item: &Value) -> Option<CommandExecution> {
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let target = item
        .get("savedPath")
        .and_then(Value::as_str)
        .or_else(|| item.get("result").and_then(Value::as_str))
        .unwrap_or("image");
    Some(CommandExecution {
        item_id: item.get("id")?.as_str()?.to_string(),
        command: target.to_string(),
        cwd: String::new(),
        activity: Some(CommandActivity {
            verb: "Generate".to_string(),
            target: target.to_string(),
        }),
        status: command_execution_status(status),
        exit_code: None,
        duration_ms: None,
        aggregated_output: None,
    })
}

fn command_execution_status(status: &str) -> CommandExecutionStatus {
    match status {
        "inProgress" => CommandExecutionStatus::InProgress,
        "completed" => CommandExecutionStatus::Completed,
        "failed" => CommandExecutionStatus::Failed,
        "declined" => CommandExecutionStatus::Declined,
        other => CommandExecutionStatus::Unknown(other.to_string()),
    }
}

fn command_activity_from_actions(actions: Option<&Value>) -> Option<CommandActivity> {
    let actions = actions?.as_array()?;
    let mut verb = None::<&'static str>;
    let mut lines = Vec::<String>::new();
    let mut read_targets = Vec::<String>::new();

    for action in actions {
        let action_verb = match action.get("type").and_then(Value::as_str) {
            Some("read") => "Read",
            Some("listFiles") => "List",
            Some("search") => "Search",
            _ => return None,
        };
        verb = match verb {
            Some(existing) if existing == action_verb => Some(existing),
            Some(_) => Some("Explore"),
            None => Some(action_verb),
        };
        match action_verb {
            "Read" => {
                let target = command_action_read_label(action);
                if !read_targets.iter().any(|existing| existing == &target) {
                    read_targets.push(target);
                }
            }
            "List" => {
                let target = action
                    .get("path")
                    .and_then(Value::as_str)
                    .or_else(|| action.get("command").and_then(Value::as_str))
                    .unwrap_or("files");
                lines.push(format!("List {target}"));
            }
            "Search" => {
                let target = match (
                    action.get("query").and_then(Value::as_str),
                    action.get("path").and_then(Value::as_str),
                ) {
                    (Some(query), Some(path)) => {
                        format!("{query} in {}", command_action_path_label(path))
                    }
                    (Some(query), None) => query.to_string(),
                    _ => action
                        .get("command")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| "search".to_string()),
                };
                lines.push(format!("Search {target}"));
            }
            _ => {}
        }
    }

    let verb = verb?;
    if !read_targets.is_empty() {
        let target = read_targets.join(", ");
        if verb == "Read" {
            return Some(CommandActivity {
                verb: "Read".to_string(),
                target,
            });
        }
        lines.push(format!("Read {target}"));
    }
    if verb == "Explore" {
        Some(CommandActivity {
            verb: "Explore".to_string(),
            target: lines.join("\n"),
        })
    } else {
        let prefix = format!("{verb} ");
        let target = lines
            .iter()
            .map(|line| line.strip_prefix(prefix.as_str()).unwrap_or(line.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        Some(CommandActivity {
            verb: verb.to_string(),
            target,
        })
    }
}

fn command_action_read_label(action: &Value) -> String {
    let name = action
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty());
    let path = action
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty());
    if let Some(label) = skill_read_label(name, path) {
        return label;
    }
    name.or(path).unwrap_or("file").to_string()
}

fn command_action_path_label(path: &str) -> &str {
    let path = path.trim();
    if path.is_empty() {
        return path;
    }
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

fn skill_read_label(name: Option<&str>, path: Option<&str>) -> Option<String> {
    let path = path?;
    let path = std::path::Path::new(path.trim());
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

fn completed_turn_id<'a>(params: &'a Option<Value>, thread_id: &str) -> Option<&'a str> {
    let Some(params) = params else {
        return None;
    };
    if params.get("threadId").and_then(Value::as_str) != Some(thread_id) {
        return None;
    }
    params
        .get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
}

fn started_turn_id<'a>(params: &'a Option<Value>, thread_id: &str) -> Option<&'a str> {
    let Some(params) = params else {
        return None;
    };
    if params.get("threadId").and_then(Value::as_str) != Some(thread_id) {
        return None;
    }
    params
        .get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
}

impl From<protocol::ThreadSummary> for AppThreadSummary {
    fn from(thread: protocol::ThreadSummary) -> Self {
        let status = status_type(&thread.status);
        let active_turn_id = latest_in_progress_turn_id_from_turns(&thread.turns);
        Self {
            upstream_thread_id: thread.id,
            session_id: thread.session_id,
            title: thread.name,
            preview: thread.preview,
            cwd: thread.cwd,
            path: thread.path,
            source: source_kind(&thread.source),
            active_turn_id,
            active: status == "active",
            status,
            created_at_unix: thread.created_at,
            updated_at_unix: thread.updated_at,
            broker_subscriber_count: thread.broker_subscriber_count,
        }
    }
}

fn latest_in_progress_turn_id_from_turns(turns: &[Value]) -> Option<String> {
    turns
        .iter()
        .rev()
        .find(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))?
        .get("id")?
        .as_str()
        .map(str::to_string)
}

fn status_type(status: &Value) -> String {
    status
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn source_kind(source: &Value) -> String {
    if let Some(source) = source.as_str() {
        return source.to_string();
    }
    if source.get("custom").is_some() {
        return "custom".to_string();
    }
    if source.get("subAgent").is_some() {
        return "subAgent".to_string();
    }
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::io::Read;
    use std::io::Write;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::sync::mpsc;
    use std::thread;

    use anyhow::Context as _;
    use base64::Engine as _;
    use sha1::Digest as _;

    use super::*;

    #[test]
    fn thread_list_info_reads_cursor_presence() {
        let value = serde_json::json!({
            "data": [{ "id": "thread-1" }],
            "nextCursor": "next",
            "backwardsCursor": null
        });

        let info = ThreadListInfo::from_response(&value);

        assert_eq!(
            info,
            ThreadListInfo {
                thread_count: 1,
                has_next_cursor: true,
                has_backwards_cursor: false,
            }
        );
    }

    #[test]
    fn thread_summary_maps_private_protocol_to_stable_fields() {
        let thread = protocol::ThreadSummary {
            id: String::from("thread-1"),
            session_id: Some(String::from("thread-1")),
            path: Some(String::from("/tmp/thread.jsonl")),
            name: Some(String::from("Fix build")),
            preview: String::from("please fix"),
            cwd: String::from("/tmp/project"),
            source: serde_json::json!({ "custom": "cx" }),
            status: serde_json::json!({ "type": "active", "activeFlags": [] }),
            created_at: 10,
            updated_at: 20,
            turns: Vec::new(),
            broker_subscriber_count: Some(2),
        };

        let summary = AppThreadSummary::from(thread);

        assert_eq!(summary.upstream_thread_id, "thread-1");
        assert_eq!(summary.source, "custom");
        assert_eq!(summary.status, "active");
        assert!(summary.active);
        assert_eq!(summary.broker_subscriber_count, Some(2));
    }

    #[test]
    fn server_request_is_not_misread_as_empty_response() {
        let message = serde_json::from_value::<ServerMessage>(json!({
            "id": 7,
            "method": "item/commandExecution/requestApproval",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-1",
                "command": "touch /tmp/x"
            }
        }))
        .unwrap();

        match message {
            ServerMessage::Request(request) => {
                assert_eq!(request.id, json!(7));
                assert_eq!(request.method, "item/commandExecution/requestApproval");
                assert!(is_approval_request_method(&request.method));
            }
            other => panic!("expected server request, got {other:?}"),
        }
    }

    #[test]
    fn sync_response_wait_handles_approval_request() {
        let (listener, url) = listen_ws_url();
        let (approval_tx, approval_rx) = mpsc::channel();
        let server = thread::spawn(move || -> Result<()> {
            let mut stream = accept_client_websocket(listener)?;
            let steer_request = serde_json::from_str::<Value>(&read_client_text(&mut stream)?)?;
            assert_eq!(
                steer_request.get("method").and_then(Value::as_str),
                Some("turn/steer")
            );
            let request_id = steer_request.get("id").cloned().unwrap();

            write_server_text(
                &mut stream,
                &json!({
                    "id": "approval-1",
                    "method": "item/commandExecution/requestApproval",
                    "params": {
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "command": "true"
                    }
                })
                .to_string(),
            )?;

            let approval_response = serde_json::from_str::<Value>(&read_client_text(&mut stream)?)?;
            approval_tx.send(approval_response).unwrap();
            write_server_text(
                &mut stream,
                &json!({
                    "id": request_id,
                    "result": {"turnId": "turn-1"}
                })
                .to_string(),
            )?;
            Ok(())
        });

        let mut client = AppServerClient::connect(&url, Duration::from_secs(2)).unwrap();
        let steered = client
            .turn_steer_with_approval("thread-1", "turn-1", "continue".to_string(), |approval| {
                assert_eq!(approval.id, json!("approval-1"));
                Ok(json!({"decision": "approved"}))
            })
            .unwrap();

        assert_eq!(steered.turn_id, "turn-1");
        let approval_response = approval_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(approval_response.get("id"), Some(&json!("approval-1")));
        assert_eq!(
            approval_response
                .pointer("/result/decision")
                .and_then(Value::as_str),
            Some("approved")
        );
        server.join().unwrap().unwrap();
    }

    #[test]
    fn sync_response_wait_handles_thread_scoped_approval_during_turn_steer() {
        let (listener, url) = listen_ws_url();
        let (approval_tx, approval_rx) = mpsc::channel();
        let server = thread::spawn(move || -> Result<()> {
            let mut stream = accept_client_websocket(listener)?;
            let steer_request = serde_json::from_str::<Value>(&read_client_text(&mut stream)?)?;
            assert_eq!(
                steer_request.get("method").and_then(Value::as_str),
                Some("turn/steer")
            );
            let request_id = steer_request.get("id").cloned().unwrap();

            write_server_text(
                &mut stream,
                &json!({
                    "id": "approval-thread",
                    "method": "item/commandExecution/requestApproval",
                    "params": {
                        "threadId": "thread-1",
                        "command": "true"
                    }
                })
                .to_string(),
            )?;

            let approval_response = serde_json::from_str::<Value>(&read_client_text(&mut stream)?)?;
            approval_tx.send(approval_response).unwrap();
            write_server_text(
                &mut stream,
                &json!({
                    "id": request_id,
                    "result": {"turnId": "turn-1"}
                })
                .to_string(),
            )?;
            Ok(())
        });

        let mut client = AppServerClient::connect(&url, Duration::from_secs(2)).unwrap();
        let steered = client
            .turn_steer_with_approval("thread-1", "turn-1", "continue".to_string(), |approval| {
                assert_eq!(approval.id, json!("approval-thread"));
                assert_eq!(approval.params.get("turnId"), None);
                Ok(json!({"decision": "approved"}))
            })
            .unwrap();

        assert_eq!(steered.turn_id, "turn-1");
        let approval_response = approval_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(approval_response.get("id"), Some(&json!("approval-thread")));
        assert_eq!(
            approval_response
                .pointer("/result/decision")
                .and_then(Value::as_str),
            Some("approved")
        );
        server.join().unwrap().unwrap();
    }

    #[test]
    fn sync_response_wait_queues_cross_thread_approval() {
        let (listener, url) = listen_ws_url();
        let (queued_response_tx, queued_response_rx) = mpsc::channel();
        let server = thread::spawn(move || -> Result<()> {
            let mut stream = accept_client_websocket(listener)?;
            let steer_request = serde_json::from_str::<Value>(&read_client_text(&mut stream)?)?;
            assert_eq!(
                steer_request.get("method").and_then(Value::as_str),
                Some("turn/steer")
            );
            let request_id = steer_request.get("id").cloned().unwrap();

            write_server_text(
                &mut stream,
                &json!({
                    "id": "approval-b",
                    "method": "item/commandExecution/requestApproval",
                    "params": {
                        "threadId": "thread-b",
                        "turnId": "turn-b",
                        "command": "false"
                    }
                })
                .to_string(),
            )?;
            stream.set_read_timeout(Some(Duration::from_millis(200)))?;
            let cross_thread_response = read_client_text_optional(&mut stream)?;
            anyhow::ensure!(
                cross_thread_response.is_none(),
                "cross-thread approval was answered during turn/steer: {cross_thread_response:?}"
            );
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;

            write_server_text(
                &mut stream,
                &json!({
                    "id": "approval-a",
                    "method": "item/commandExecution/requestApproval",
                    "params": {
                        "threadId": "thread-a",
                        "turnId": "turn-a",
                        "command": "true"
                    }
                })
                .to_string(),
            )?;
            let approval_a_response =
                serde_json::from_str::<Value>(&read_client_text(&mut stream)?)?;
            assert_eq!(approval_a_response.get("id"), Some(&json!("approval-a")));

            write_server_text(
                &mut stream,
                &json!({
                    "id": request_id,
                    "result": {"turnId": "turn-a"}
                })
                .to_string(),
            )?;

            let approval_b_response =
                serde_json::from_str::<Value>(&read_client_text(&mut stream)?)?;
            queued_response_tx.send(approval_b_response).unwrap();
            Ok(())
        });

        let mut client = AppServerClient::connect(&url, Duration::from_secs(2)).unwrap();
        let mut handled_ids = Vec::new();
        let steered = client
            .turn_steer_with_approval("thread-a", "turn-a", "continue".to_string(), |approval| {
                handled_ids.push(approval.id.clone());
                Ok(json!({"decision": "approved-a"}))
            })
            .unwrap();
        assert_eq!(steered.turn_id, "turn-a");
        assert_eq!(handled_ids, vec![json!("approval-a")]);

        let mut drained_ids = Vec::new();
        let drained = client
            .drain_thread_events(
                "thread-b",
                Some("turn-b"),
                1,
                |event| {
                    if let ParsedServerEvent::ApprovalRequest(approval) = event {
                        drained_ids.push(approval.id);
                    }
                    Ok(())
                },
                |approval| {
                    assert_eq!(approval.id, json!("approval-b"));
                    Ok(Some(json!({"decision": "approved-b"})))
                },
            )
            .unwrap();

        assert_eq!(drained, 1);
        assert_eq!(drained_ids, vec![json!("approval-b")]);
        let queued_response = queued_response_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(queued_response.get("id"), Some(&json!("approval-b")));
        assert_eq!(
            queued_response
                .pointer("/result/decision")
                .and_then(Value::as_str),
            Some("approved-b")
        );
        server.join().unwrap().unwrap();
    }

    #[test]
    fn sync_response_wait_queues_approval_request_without_rejecting() {
        let (listener, url) = listen_ws_url();
        let (extra_tx, extra_rx) = mpsc::channel();
        let server = thread::spawn(move || -> Result<()> {
            let mut stream = accept_client_websocket(listener)?;
            let list_request = serde_json::from_str::<Value>(&read_client_text(&mut stream)?)?;
            assert_eq!(
                list_request.get("method").and_then(Value::as_str),
                Some("thread/list")
            );
            let request_id = list_request.get("id").cloned().unwrap();

            write_server_text(
                &mut stream,
                &json!({
                    "id": "approval-queued",
                    "method": "item/commandExecution/requestApproval",
                    "params": {
                        "threadId": "thread-1",
                        "turnId": "turn-1",
                        "command": "true"
                    }
                })
                .to_string(),
            )?;
            write_server_text(
                &mut stream,
                &json!({
                    "id": request_id,
                    "result": {
                        "data": [],
                        "nextCursor": null,
                        "backwardsCursor": null
                    }
                })
                .to_string(),
            )?;

            stream.set_read_timeout(Some(Duration::from_millis(200)))?;
            extra_tx
                .send(read_client_text_optional(&mut stream)?)
                .unwrap();
            Ok(())
        });

        let mut client = AppServerClient::connect(&url, Duration::from_secs(2)).unwrap();
        let info = client.thread_list_probe().unwrap();
        drop(client);

        assert_eq!(info.thread_count, 0);
        assert_eq!(extra_rx.recv_timeout(Duration::from_secs(2)).unwrap(), None);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn queued_approval_limit_disconnects_without_approval_response() {
        let (listener, url) = listen_ws_url();
        let (response_tx, response_rx) = mpsc::channel();
        let server = thread::spawn(move || -> Result<()> {
            let mut stream = accept_client_websocket(listener)?;
            let list_request = serde_json::from_str::<Value>(&read_client_text(&mut stream)?)?;
            assert_eq!(
                list_request.get("method").and_then(Value::as_str),
                Some("thread/list")
            );

            for index in 0..=MAX_QUEUED_APPROVALS {
                write_server_text(
                    &mut stream,
                    &json!({
                        "id": format!("approval-{index}"),
                        "method": "item/commandExecution/requestApproval",
                        "params": {
                            "threadId": "thread-overflow",
                            "turnId": "turn-overflow",
                            "command": "true"
                        }
                    })
                    .to_string(),
                )?;
            }

            stream.set_read_timeout(Some(Duration::from_millis(200)))?;
            response_tx
                .send(read_client_text_optional(&mut stream)?)
                .unwrap();
            Ok(())
        });

        let mut client = AppServerClient::connect(&url, Duration::from_secs(2)).unwrap();
        let err = client.thread_list_probe().unwrap_err();

        assert!(
            format!("{err:#}").contains("approval queue is full"),
            "unexpected error: {err:#}"
        );
        let retry_err = client.thread_list_probe().unwrap_err();
        assert!(
            format!("{retry_err:#}").contains("app-server client is closed"),
            "unexpected retry error: {retry_err:#}"
        );
        assert_eq!(
            response_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            None
        );
        server.join().unwrap().unwrap();
    }

    #[test]
    fn notification_command_execution_reads_started_item() {
        let params = Some(json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "item": {
                "type": "commandExecution",
                "id": "cmd-1",
                "command": "printf hello",
                "cwd": "/tmp/project",
                "processId": null,
                "source": "agent",
                "status": "inProgress",
                "commandActions": [],
                "aggregatedOutput": null,
                "exitCode": null,
                "durationMs": null
            },
            "startedAtMs": 10
        }));

        assert_eq!(
            notification_command_execution(&params, "thread-1", Some("turn-1")),
            Some(CommandExecution {
                item_id: "cmd-1".to_string(),
                command: "printf hello".to_string(),
                cwd: "/tmp/project".to_string(),
                activity: None,
                status: CommandExecutionStatus::InProgress,
                exit_code: None,
                duration_ms: None,
                aggregated_output: None,
            })
        );
    }

    #[test]
    fn notification_command_execution_summarizes_command_actions() {
        let params = Some(json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "item": {
                "type": "commandExecution",
                "id": "cmd-1",
                "command": "sed -n '1,20p' wiley.py && sed -n '1,20p' science.py",
                "cwd": "/tmp/project",
                "processId": null,
                "source": "agent",
                "status": "completed",
                "commandActions": [
                    {"type": "read", "command": "sed -n '1,20p' wiley.py", "name": "wiley.py", "path": "/tmp/project/wiley.py"},
                    {"type": "read", "command": "sed -n '1,20p' science.py", "name": "science.py", "path": "/tmp/project/science.py"}
                ],
                "aggregatedOutput": null,
                "exitCode": 0,
                "durationMs": 12
            },
            "completedAtMs": 10
        }));

        let event = notification_command_execution(&params, "thread-1", Some("turn-1")).unwrap();

        assert_eq!(
            event.activity,
            Some(CommandActivity {
                verb: "Read".to_string(),
                target: "wiley.py, science.py".to_string(),
            })
        );
    }

    #[test]
    fn notification_command_execution_preserves_search_and_skill_actions() {
        let params = Some(json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "item": {
                "type": "commandExecution",
                "id": "cmd-1",
                "command": "rg -n paused src/channel/telegram.rs",
                "cwd": "/tmp/project",
                "processId": null,
                "source": "agent",
                "status": "completed",
                "commandActions": [
                    {
                        "type": "search",
                        "command": "rg -n paused src/channel/telegram.rs",
                        "query": "paused|telegram_paused|AcquireLease",
                        "path": "src/channel/telegram.rs"
                    },
                    {
                        "type": "read",
                        "command": "sed -n '1,80p' /Users/yupeit/dev/skills/skills/rust-systems-style/SKILL.md",
                        "name": "SKILL.md",
                        "path": "/Users/yupeit/dev/skills/skills/rust-systems-style/SKILL.md"
                    }
                ],
                "aggregatedOutput": null,
                "exitCode": 0,
                "durationMs": 12
            },
            "completedAtMs": 10
        }));

        let event = notification_command_execution(&params, "thread-1", Some("turn-1")).unwrap();

        assert_eq!(
            event.activity,
            Some(CommandActivity {
                verb: "Explore".to_string(),
                target: "Search paused|telegram_paused|AcquireLease in telegram.rs\nRead SKILL.md (rust-systems-style skill)".to_string(),
            })
        );
    }

    #[test]
    fn notification_command_execution_reads_file_change_items() {
        let params = Some(json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "item": {
                "type": "fileChange",
                "id": "patch-1",
                "changes": [
                    {
                        "path": "src/channel/telegram.rs",
                        "kind": "update",
                        "diff": "@@\n-old\n+new\n+another\n"
                    }
                ],
                "cwd": "/tmp/project",
                "status": "completed"
            },
            "completedAtMs": 10
        }));

        let event = notification_command_execution(&params, "thread-1", Some("turn-1")).unwrap();

        assert_eq!(
            event,
            CommandExecution {
                item_id: "patch-1".to_string(),
                command: "apply patch".to_string(),
                cwd: "/tmp/project".to_string(),
                activity: Some(CommandActivity {
                    verb: "Edited".to_string(),
                    target: "src/channel/telegram.rs (+2 -1)".to_string(),
                }),
                status: CommandExecutionStatus::Completed,
                exit_code: None,
                duration_ms: None,
                aggregated_output: None,
            }
        );
    }

    #[test]
    fn notification_command_execution_reads_plan_items() {
        let params = Some(json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "item": {
                "type": "plan",
                "id": "plan-1",
                "text": "□ Inspect UI\n□ Ship release"
            },
            "completedAtMs": 10
        }));

        let event = notification_command_execution(&params, "thread-1", Some("turn-1")).unwrap();

        assert_eq!(
            event.activity,
            Some(CommandActivity {
                verb: "Plan".to_string(),
                target: "□ Inspect UI\n□ Ship release".to_string(),
            })
        );
    }

    #[test]
    fn notification_command_output_delta_filters_by_turn() {
        let params = Some(json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "cmd-1",
            "delta": "hello\n"
        }));

        assert_eq!(
            notification_command_output_delta(&params, "thread-1", Some("turn-1")),
            Some(("cmd-1".to_string(), "hello\n".to_string()))
        );
        assert_eq!(
            notification_command_output_delta(&params, "thread-1", Some("other-turn")),
            None
        );
    }

    #[test]
    fn notification_text_delta_reads_reasoning_delta() {
        let params = Some(json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "reasoning-1",
            "delta": "Checking location."
        }));

        assert_eq!(
            notification_text_delta(&params, "thread-1", Some("turn-1")),
            Some("Checking location.".to_string())
        );
    }

    #[test]
    fn notification_text_delta_reads_base64_delta() {
        let params = Some(json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "reasoning-1",
            "deltaBase64": "5L2g5aW9"
        }));

        assert_eq!(
            notification_text_delta(&params, "thread-1", Some("turn-1")),
            Some("你好".to_string())
        );
    }

    #[test]
    fn started_turn_id_filters_by_thread() {
        let params = Some(json!({
            "threadId": "thread-1",
            "turn": {
                "id": "turn-1"
            }
        }));

        assert_eq!(started_turn_id(&params, "thread-1"), Some("turn-1"));
        assert_eq!(started_turn_id(&params, "other-thread"), None);
    }

    #[test]
    fn parse_server_event_filters_started_by_expected_turn() {
        let message = json!({
            "method": "turn/started",
            "params": {
                "threadId": "thread-1",
                "turn": {"id": "turn-2"}
            }
        });

        assert_eq!(
            parse_server_event(&message, "thread-1", Some("turn-1")),
            None
        );
        assert_eq!(
            parse_server_event(&message, "thread-1", Some("turn-2")),
            Some(ParsedServerEvent::Stream(AppStreamEvent::TurnStarted))
        );
    }

    #[test]
    fn parse_server_event_reads_plan_delta() {
        let message = json!({
            "method": "item/plan/delta",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "delta": "Plan update"
            }
        });

        assert_eq!(
            parse_server_event(&message, "thread-1", Some("turn-1")),
            Some(ParsedServerEvent::Stream(AppStreamEvent::ReasoningDelta(
                "Plan update".to_string()
            )))
        );
    }

    fn listen_ws_url() -> (TcpListener, String) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, format!("ws://127.0.0.1:{port}"))
    }

    fn accept_client_websocket(listener: TcpListener) -> Result<TcpStream> {
        let (mut stream, _addr) = listener.accept().context("accept app-server client")?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let request = read_http_head(&mut stream)?;
        let key = request
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| value.trim())
            })
            .context("missing websocket key")?;
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {}\r\n\
             \r\n",
            websocket_accept(key)
        );
        stream.write_all(response.as_bytes())?;
        Ok(stream)
    }

    fn read_http_head(stream: &mut TcpStream) -> Result<String> {
        let mut bytes = Vec::new();
        loop {
            let mut byte = [0_u8; 1];
            stream.read_exact(&mut byte)?;
            bytes.push(byte[0]);
            if bytes.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(bytes).context("decode websocket request")
    }

    fn websocket_accept(key: &str) -> String {
        let mut hasher = sha1::Sha1::new();
        hasher.update(key.as_bytes());
        hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
    }

    fn write_server_text(stream: &mut TcpStream, text: &str) -> Result<()> {
        let payload = text.as_bytes();
        let mut frame = vec![0x81];
        match payload.len() {
            len if len < 126 => frame.push(len as u8),
            len if len <= u16::MAX as usize => {
                frame.push(126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            }
            len => {
                frame.push(127);
                frame.extend_from_slice(&(len as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(payload);
        stream.write_all(&frame)?;
        Ok(())
    }

    fn read_client_text(stream: &mut TcpStream) -> Result<String> {
        read_client_text_optional(stream)?.context("timed out waiting for client websocket text")
    }

    fn read_client_text_optional(stream: &mut TcpStream) -> Result<Option<String>> {
        let mut header = [0_u8; 2];
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::UnexpectedEof | ErrorKind::TimedOut | ErrorKind::WouldBlock
                ) =>
            {
                return Ok(None);
            }
            Err(err) => return Err(err).context("read websocket frame header"),
        }
        anyhow::ensure!(header[0] & 0x0f == 0x1, "expected websocket text frame");
        anyhow::ensure!(header[1] & 0x80 != 0, "expected masked client frame");
        let mut len = u64::from(header[1] & 0x7f);
        if len == 126 {
            let mut extended = [0_u8; 2];
            stream.read_exact(&mut extended)?;
            len = u64::from(u16::from_be_bytes(extended));
        } else if len == 127 {
            let mut extended = [0_u8; 8];
            stream.read_exact(&mut extended)?;
            len = u64::from_be_bytes(extended);
        }
        let mut mask = [0_u8; 4];
        stream.read_exact(&mut mask)?;
        let mut payload = vec![0_u8; len as usize];
        stream.read_exact(&mut payload)?;
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % mask.len()];
        }
        String::from_utf8(payload)
            .map(Some)
            .context("decode websocket text")
    }
}
