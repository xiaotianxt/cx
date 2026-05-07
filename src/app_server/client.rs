use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppThreadSummary {
    pub upstream_thread_id: String,
    pub title: Option<String>,
    pub preview: String,
    pub cwd: String,
    pub source: String,
    pub status: String,
    pub active: bool,
    pub created_at_unix: i64,
    pub updated_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartedThread {
    pub upstream_thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartedTurn {
    pub turn_id: String,
    pub assistant_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterruptOutcome {
    pub interrupted_turn_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ObserveOutcome {
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApprovalRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

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
}

impl AppServerClient {
    pub(crate) fn connect(url: &str, timeout: Duration) -> Result<Self> {
        let url = LoopbackWsUrl::parse(url)?;
        let websocket = WebSocket::connect(&url, timeout)?;
        Ok(Self {
            websocket,
            next_request_id: 1,
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
                capabilities: serde_json::Map::new(),
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
                limit: 1,
                use_state_db_only: true,
            },
        )?;
        Ok(ThreadListInfo::from_response(&response))
    }

    pub(crate) fn thread_list(&mut self, limit: u64) -> Result<ThreadListPage> {
        let response = self.request(
            "thread/list",
            protocol::ThreadListParams {
                limit,
                use_state_db_only: true,
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
            upstream_thread_id: response.thread.id,
        })
    }

    pub(crate) fn thread_resume(&mut self, thread_id: &str, cwd: Option<&str>) -> Result<()> {
        let response = self.request(
            "thread/resume",
            protocol::ThreadResumeParams {
                thread_id: thread_id.to_string(),
                cwd: cwd.map(str::to_string),
            },
        )?;
        let response = serde_json::from_value::<protocol::ThreadResumeResponse>(response)
            .context("decode thread/resume response")?;
        let _thread_id = response.thread.id;
        Ok(())
    }

    pub(crate) fn thread_archive(&mut self, thread_id: &str) -> Result<()> {
        let _response = self.request(
            "thread/archive",
            protocol::ThreadArchiveParams {
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
        mut on_delta: F,
        mut on_approval: impl FnMut(ApprovalRequest) -> Result<Value>,
    ) -> Result<StartedTurn>
    where
        F: FnMut(&str) -> Result<()>,
    {
        let request_id = self.send_request(
            "turn/start",
            protocol::TurnStartParams {
                thread_id: thread_id.to_string(),
                input: vec![protocol::UserInput::Text {
                    text: prompt,
                    text_elements: Vec::new(),
                }],
            },
        )?;
        self.collect_turn_start(request_id, thread_id, &mut on_delta, &mut on_approval)
    }

    pub(crate) fn observe_thread_events<F>(
        &mut self,
        thread_id: &str,
        cwd: Option<&str>,
        mut on_delta: F,
    ) -> Result<ObserveOutcome>
    where
        F: FnMut(&str) -> Result<()>,
    {
        self.thread_resume(thread_id, cwd)?;

        let Some(turn_id) = self.active_turn_id(thread_id)? else {
            return Ok(ObserveOutcome { turn_id: None });
        };

        loop {
            let frame = self.websocket.read_text()?;
            let message =
                serde_json::from_str::<ServerMessage>(&frame).context("decode observe event")?;
            match message {
                ServerMessage::Notification { method, params }
                    if method == "item/agentMessage/delta" =>
                {
                    if let Some(delta) = notification_delta(&params, thread_id, Some(&turn_id)) {
                        on_delta(delta)?;
                    }
                }
                ServerMessage::Notification { method, params } if method == "turn/completed" => {
                    if let Some(completed_tid) = completed_turn_id(&params, thread_id) {
                        if completed_tid == turn_id {
                            return Ok(ObserveOutcome {
                                turn_id: Some(turn_id),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn active_turn_id(&mut self, thread_id: &str) -> Result<Option<String>> {
        let response = self.request(
            "thread/read",
            protocol::ThreadReadParams {
                thread_id: thread_id.to_string(),
                include_turns: true,
            },
        )?;
        Ok(latest_in_progress_turn_id(&response))
    }

    fn request<P>(&mut self, method: &'static str, params: P) -> Result<Value>
    where
        P: Serialize,
    {
        let id = self.send_request(method, params)?;
        self.read_response(method, id)
    }

    fn send_request<P>(&mut self, method: &'static str, params: P) -> Result<u64>
    where
        P: Serialize,
    {
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

    fn read_response(&mut self, method: &'static str, id: u64) -> Result<Value> {
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
                    self.send_error_response(
                        request.id,
                        -32601,
                        format!(
                            "unsupported app-server request during {method}: {}",
                            request.method
                        ),
                    )?;
                }
                ServerMessage::Notification { .. } => {}
            }
        }
    }

    fn collect_turn_start<F>(
        &mut self,
        request_id: u64,
        thread_id: &str,
        on_delta: &mut F,
        on_approval: &mut impl FnMut(ApprovalRequest) -> Result<Value>,
    ) -> Result<StartedTurn>
    where
        F: FnMut(&str) -> Result<()>,
    {
        let mut turn_id = None::<String>;
        let mut assistant_text = String::new();
        let mut completed = false;
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
                    if completed {
                        return Ok(StartedTurn {
                            turn_id: response_turn_id,
                            assistant_text,
                        });
                    }
                    turn_id = Some(response_turn_id);
                }
                ServerMessage::Notification { method, params } => {
                    if method == "item/agentMessage/delta" {
                        if let Some(delta) =
                            notification_delta(&params, thread_id, turn_id.as_deref())
                        {
                            assistant_text.push_str(delta);
                            on_delta(delta)?;
                        }
                    } else if method == "turn/completed" {
                        if let Some(completed_turn_id) = completed_turn_id(&params, thread_id) {
                            if turn_id.as_deref().is_none_or(|id| id == completed_turn_id) {
                                completed = true;
                                if let Some(turn_id) = turn_id {
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
                    if is_approval_request_method(&request.method) {
                        let approval = ApprovalRequest {
                            id: request.id.clone(),
                            method: request.method,
                            params: request.params.unwrap_or(Value::Null),
                        };
                        let result = match on_approval(approval) {
                            Ok(result) => result,
                            Err(err) => {
                                self.send_error_response(
                                    request.id,
                                    -32000,
                                    format!("approval failed: {err:#}"),
                                )?;
                                return Err(err);
                            }
                        };
                        self.send_success_response(request.id, result)?;
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

fn latest_in_progress_turn_id(response: &Value) -> Option<String> {
    response
        .get("thread")?
        .get("turns")?
        .as_array()?
        .iter()
        .rev()
        .find(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))?
        .get("id")?
        .as_str()
        .map(str::to_string)
}

fn notification_delta<'a>(
    params: &'a Option<Value>,
    thread_id: &str,
    turn_id: Option<&str>,
) -> Option<&'a str> {
    let params = params.as_ref()?;
    if params.get("threadId").and_then(Value::as_str)? != thread_id {
        return None;
    }
    if let Some(turn_id) = turn_id {
        if params.get("turnId").and_then(Value::as_str)? != turn_id {
            return None;
        }
    }
    params.get("delta").and_then(Value::as_str)
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

impl From<protocol::ThreadSummary> for AppThreadSummary {
    fn from(thread: protocol::ThreadSummary) -> Self {
        let status = status_type(&thread.status);
        Self {
            upstream_thread_id: thread.id,
            title: thread.name,
            preview: thread.preview,
            cwd: thread.cwd,
            source: source_kind(&thread.source),
            active: status == "active",
            status,
            created_at_unix: thread.created_at,
            updated_at_unix: thread.updated_at,
        }
    }
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
            name: Some(String::from("Fix build")),
            preview: String::from("please fix"),
            cwd: String::from("/tmp/project"),
            source: serde_json::json!({ "custom": "cx" }),
            status: serde_json::json!({ "type": "active", "activeFlags": [] }),
            created_at: 10,
            updated_at: 20,
        };

        let summary = AppThreadSummary::from(thread);

        assert_eq!(summary.upstream_thread_id, "thread-1");
        assert_eq!(summary.source, "custom");
        assert_eq!(summary.status, "active");
        assert!(summary.active);
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
}
