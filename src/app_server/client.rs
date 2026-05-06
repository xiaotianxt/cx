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
}
