use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use serde::Serialize;
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

    fn request<P>(&mut self, method: &'static str, params: P) -> Result<Value>
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
                ServerMessage::Response(response) => {
                    anyhow::bail!(
                        "app-server returned response id {} while waiting for {}",
                        response.id,
                        id
                    );
                }
                ServerMessage::Notification { .. } => {}
            }
        }
    }
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
}
