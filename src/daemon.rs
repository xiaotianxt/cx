#[cfg(unix)]
mod imp {
    use std::io::Read;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use anyhow::bail;
    use anyhow::Context;
    use anyhow::Result;
    use base64::Engine;
    use serde::Deserialize;
    use serde_json::Value;

    const SOCKET_DIR: &str = "app-server-control";
    const SOCKET_FILE: &str = "app-server-control.sock";
    const READ_TIMEOUT: Duration = Duration::from_secs(5);

    #[derive(Debug, Deserialize)]
    struct JsonRpcResponse {
        #[serde(default)]
        id: Option<Value>,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<JsonRpcErrorBody>,
    }

    #[derive(Debug, Deserialize)]
    struct JsonRpcErrorBody {
        message: String,
    }

    #[derive(Debug, Clone)]
    pub struct DaemonStatus {
        pub running: bool,
        pub socket_path: PathBuf,
        #[allow(dead_code)]
        pub app_server_version: Option<String>,
    }

    pub fn socket_path(codex_home: &Path) -> PathBuf {
        codex_home.join(SOCKET_DIR).join(SOCKET_FILE)
    }

    pub fn is_running(codex_home: &Path) -> bool {
        let path = socket_path(codex_home);
        if !path.exists() {
            return false;
        }
        probe(codex_home).is_ok()
    }

    pub fn status(codex_home: &Path) -> DaemonStatus {
        let socket_path = socket_path(codex_home);
        let running = is_running(codex_home);
        DaemonStatus {
            running,
            socket_path,
            app_server_version: None,
        }
    }

    fn probe(codex_home: &Path) -> Result<()> {
        let mut client = DaemonClient::connect(codex_home)?;
        let _account =
            client.request("account/read", serde_json::json!({"refreshToken": false}))?;
        Ok(())
    }

    pub struct DaemonClient {
        stream: UnixStream,
    }

    impl DaemonClient {
        pub fn connect(codex_home: &Path) -> Result<Self> {
            Self::connect_with_timeout(codex_home, READ_TIMEOUT)
        }

        pub fn connect_with_timeout(codex_home: &Path, timeout: Duration) -> Result<Self> {
            let path = socket_path(codex_home);
            let mut stream = UnixStream::connect(&path)
                .with_context(|| format!("connect to daemon at {}", path.display()))?;
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;

            ws_handshake(&mut stream)?;

            let init = serde_json::json!({
                "id": "cx-init",
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "cx",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {}
                }
            });
            let resp = Self::ws_request(&mut stream, &init)?;
            if let Some(e) = resp.error {
                bail!("daemon initialize failed: {}", e.message);
            }

            let initialized = serde_json::json!({
                "method": "initialized",
                "params": {}
            });
            ws_send_text(&mut stream, &serde_json::to_string(&initialized)?)?;

            Ok(Self { stream })
        }

        pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
            let req = serde_json::json!({
                "id": "cx-req",
                "method": method,
                "params": params,
            });
            let resp = Self::ws_request(&mut self.stream, &req)?;
            if let Some(e) = resp.error {
                bail!("daemon error ({}): {}", method, e.message);
            }
            resp.result.context("daemon returned empty result")
        }

        fn ws_request(stream: &mut UnixStream, request: &Value) -> Result<JsonRpcResponse> {
            ws_send_text(stream, &serde_json::to_string(request)?)?;
            loop {
                let text = ws_recv_text(stream)?;
                let msg: JsonRpcResponse = serde_json::from_str(&text)
                    .with_context(|| format!("parse daemon response: {text}"))?;
                if msg.id.is_some() {
                    return Ok(msg);
                }
            }
        }
    }

    fn ws_handshake(stream: &mut UnixStream) -> Result<()> {
        let key = ws_key();
        let request = format!(
            "GET / HTTP/1.1\r\n\
             Host: localhost\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );
        stream.write_all(request.as_bytes())?;

        let mut response = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            stream.read_exact(&mut buf)?;
            response.push(buf[0]);
            let len = response.len();
            if len >= 4
                && response[len - 4] == b'\r'
                && response[len - 3] == b'\n'
                && response[len - 2] == b'\r'
                && response[len - 1] == b'\n'
            {
                break;
            }
        }

        let status_line = String::from_utf8_lossy(&response)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        if !status_line.contains("101") {
            bail!("daemon handshake rejected: {status_line}");
        }
        Ok(())
    }

    fn ws_key() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let bytes: [u8; 16] = [
            nanos as u8,
            (nanos >> 8) as u8,
            (nanos >> 16) as u8,
            (nanos >> 24) as u8,
            (nanos >> 32) as u8,
            (nanos >> 40) as u8,
            (nanos >> 48) as u8,
            (nanos >> 56) as u8,
            pid as u8,
            (pid >> 8) as u8,
            (pid >> 16) as u8,
            (pid >> 24) as u8,
            0xca,
            0xfe,
            0xba,
            0xbe,
        ];
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn ws_send_text(stream: &mut UnixStream, text: &str) -> Result<()> {
        let payload = text.as_bytes();
        let mask = [0x12u8, 0x34, 0x56, 0x78];
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x81);
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= 0xFFFF {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            frame.push(b ^ mask[i % 4]);
        }
        stream.write_all(&frame)?;
        stream.flush()?;
        Ok(())
    }

    fn ws_recv_text(stream: &mut UnixStream) -> Result<String> {
        let mut fragments: Vec<u8> = Vec::new();
        let mut first_opcode: Option<u8> = None;
        loop {
            let mut header = [0u8; 2];
            stream.read_exact(&mut header)?;
            let fin = (header[0] & 0x80) != 0;
            let opcode = header[0] & 0x0F;
            let masked = (header[1] & 0x80) != 0;
            let mut len = (header[1] & 0x7F) as u64;

            if len == 126 {
                let mut buf = [0u8; 2];
                stream.read_exact(&mut buf)?;
                len = u16::from_be_bytes(buf) as u64;
            } else if len == 127 {
                let mut buf = [0u8; 8];
                stream.read_exact(&mut buf)?;
                len = u64::from_be_bytes(buf);
            }

            if len > 16 * 1024 * 1024 {
                bail!("daemon frame too large: {len} bytes");
            }

            let mut mask = [0u8; 4];
            if masked {
                stream.read_exact(&mut mask)?;
            }

            let mut payload = vec![0u8; len as usize];
            stream.read_exact(&mut payload)?;
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }

            match opcode {
                0x1 => {
                    if first_opcode.is_some() {
                        bail!("unexpected text frame during fragmentation");
                    }
                    if fin {
                        return String::from_utf8(payload)
                            .context("invalid UTF-8 in daemon response");
                    }
                    first_opcode = Some(0x1);
                    fragments.extend_from_slice(&payload);
                }
                0x0 => {
                    if first_opcode.is_none() {
                        bail!("unexpected continuation frame without start");
                    }
                    fragments.extend_from_slice(&payload);
                    if fin {
                        return String::from_utf8(fragments)
                            .context("invalid UTF-8 in daemon response");
                    }
                }
                0x8 => bail!("daemon closed connection"),
                0x9 => {
                    let mut pong = vec![0x8Au8];
                    let plen = payload.len();
                    if plen < 126 {
                        pong.push(plen as u8);
                    } else if plen <= 0xFFFF {
                        pong.push(126);
                        pong.extend_from_slice(&(plen as u16).to_be_bytes());
                    } else {
                        pong.push(127);
                        pong.extend_from_slice(&(plen as u64).to_be_bytes());
                    }
                    pong.extend_from_slice(&payload);
                    stream.write_all(&pong)?;
                    stream.flush()?;
                }
                _ => bail!("unexpected daemon frame opcode: {opcode}"),
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::path::Path;
    use std::path::PathBuf;

    #[derive(Debug, Clone)]
    pub struct DaemonStatus {
        pub running: bool,
        pub socket_path: PathBuf,
        pub app_server_version: Option<String>,
    }

    pub fn socket_path(_codex_home: &Path) -> PathBuf {
        PathBuf::new()
    }

    pub fn is_running(_codex_home: &Path) -> bool {
        false
    }

    pub fn status(_codex_home: &Path) -> DaemonStatus {
        DaemonStatus {
            running: false,
            socket_path: PathBuf::new(),
            app_server_version: None,
        }
    }
}

pub use imp::*;

#[cfg(unix)]
use imp::DaemonClient;

pub fn try_query_rate_limits(
    codex_home: &std::path::Path,
    timeout: std::time::Duration,
) -> Option<serde_json::Value> {
    #[cfg(unix)]
    {
        let mut client = DaemonClient::connect_with_timeout(codex_home, timeout).ok()?;
        client
            .request("account/rateLimits/read", serde_json::json!({}))
            .ok()
    }
    #[cfg(not(unix))]
    {
        let _ = codex_home;
        let _ = timeout;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_uses_nested_dir_structure() {
        let home = std::path::Path::new("/home/user/.codex");
        let path = socket_path(home);
        assert!(path.ends_with("app-server-control/app-server-control.sock"));
        assert!(path.starts_with("/home/user/.codex"));
    }

    #[test]
    fn status_reports_not_running_when_daemon_absent() {
        let tmp = std::env::temp_dir().join(format!("cx-daemon-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let status = status(&tmp);
        assert!(!status.running);
        assert_eq!(status.socket_path, socket_path(&tmp));
        assert!(status.app_server_version.is_none());
    }
}
