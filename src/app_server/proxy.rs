//! Low-level single-upstream app-server proxy.
//!
//! This remains for foreground `cx serve start`. The background service uses
//! the broker module so it can route by thread and worker instead of forwarding
//! every connection to one upstream.

use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use super::LoopbackWsUrl;

const MAX_HANDSHAKE_BYTES: usize = 16 * 1024;
const MAX_FRAME_PAYLOAD: u64 = 16 * 1024 * 1024;
#[derive(Debug, Clone)]
pub(crate) struct AppServerProxy {
    upstream: ProxyUpstream,
    event_log: PathBuf,
}

#[derive(Debug, Clone)]
enum ProxyUpstream {
    Fixed(String),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProxyEventRecord {
    timestamp_unix_ms: u128,
    connection_id: u64,
    direction: &'static str,
    method: Option<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    event_type: Option<String>,
    rate_limit_reached: bool,
}

impl AppServerProxy {
    pub(crate) fn new(upstream_url: String, event_log: PathBuf) -> Self {
        Self {
            upstream: ProxyUpstream::Fixed(upstream_url),
            event_log,
        }
    }

    pub(crate) fn spawn_with_listener(
        self,
        listener: TcpListener,
    ) -> Result<thread::JoinHandle<()>> {
        listener
            .set_nonblocking(false)
            .context("configure app-server proxy listener")?;
        let event_log = open_event_log(&self.event_log)?;
        let event_log = Arc::new(Mutex::new(event_log));
        let next_connection_id = Arc::new(AtomicU64::new(1));

        Ok(thread::spawn(move || {
            for accepted in listener.incoming() {
                let Ok(client) = accepted else {
                    continue;
                };
                let upstream = self.upstream.clone();
                let event_log = Arc::clone(&event_log);
                let connection_id = next_connection_id.fetch_add(1, Ordering::Relaxed);
                thread::spawn(move || {
                    if let Err(err) =
                        handle_connection(client, &upstream, &event_log, connection_id)
                    {
                        eprintln!("app-server proxy connection failed: {err:#}");
                    }
                });
            }
        }))
    }
}

fn handle_connection(
    mut client: TcpStream,
    upstream: &ProxyUpstream,
    event_log: &Arc<Mutex<fs::File>>,
    connection_id: u64,
) -> Result<()> {
    let request = read_http_head(&mut client).context("read app-server proxy request")?;
    let request_text = String::from_utf8_lossy(&request);
    let request_line = request_text.lines().next().unwrap_or_default();
    if request_line.starts_with("GET /readyz ") || request_line.starts_with("GET /healthz ") {
        let upstream_url = upstream.current()?;
        if !upstream_ready(&upstream_url) {
            client
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 12\r\nConnection: close\r\n\r\nunavailable",
                )
                .context("write app-server proxy unhealthy response")?;
            return Ok(());
        }
        client
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .context("write app-server proxy health response")?;
        return Ok(());
    }

    let upstream_url = upstream.current()?;
    let mut upstream = TcpStream::connect((upstream_url.host(), upstream_url.port()))
        .with_context(|| format!("connect upstream app-server {upstream_url:?}"))?;
    upstream
        .set_read_timeout(Some(Duration::from_secs(600)))
        .context("set upstream read timeout")?;
    upstream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .context("set upstream write timeout")?;
    client
        .set_read_timeout(Some(Duration::from_secs(600)))
        .context("set client read timeout")?;
    client
        .set_write_timeout(Some(Duration::from_secs(30)))
        .context("set client write timeout")?;

    let upstream_request = websocket_upstream_request(&request);
    upstream
        .write_all(&upstream_request)
        .context("forward websocket handshake to upstream")?;
    let response = read_http_head(&mut upstream).context("read upstream websocket handshake")?;
    client
        .write_all(&response)
        .context("forward websocket handshake to client")?;

    let client_log = Arc::clone(event_log);
    let client_reader = client.try_clone().context("clone client reader")?;
    let upstream_writer = upstream.try_clone().context("clone upstream writer")?;
    let client_to_upstream = thread::spawn(move || {
        if let Err(err) =
            proxy_client_to_upstream(client_reader, upstream_writer, &client_log, connection_id)
        {
            eprintln!("app-server proxy client stream failed: {err:#}");
        }
    });

    let result = proxy_upstream_to_client(upstream, client, event_log, connection_id);
    let _ = client_to_upstream.join();
    result
}

fn upstream_ready(upstream_url: &LoopbackWsUrl) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], upstream_url.port())),
        Duration::from_millis(500),
    ) else {
        return false;
    };
    let request = format!(
        "GET /readyz HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
        upstream_url.host(),
        upstream_url.port()
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        match stream.read_exact(&mut byte) {
            Ok(()) => {
                bytes.push(byte[0]);
                if bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
                if bytes.len() > MAX_HANDSHAKE_BYTES {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
    std::str::from_utf8(&bytes)
        .ok()
        .and_then(|response| response.lines().next())
        .is_some_and(|status| {
            status.starts_with("HTTP/1.1 200 ") || status.starts_with("HTTP/1.0 200 ")
        })
}

impl ProxyUpstream {
    fn current(&self) -> Result<LoopbackWsUrl> {
        match self {
            Self::Fixed(url) => LoopbackWsUrl::parse(url),
        }
    }
}

fn proxy_client_to_upstream(
    mut client: TcpStream,
    mut upstream: TcpStream,
    event_log: &Arc<Mutex<fs::File>>,
    connection_id: u64,
) -> Result<()> {
    let result =
        proxy_client_to_upstream_inner(&mut client, &mut upstream, event_log, connection_id);
    shutdown_pair(&client, &upstream);
    result
}

fn proxy_client_to_upstream_inner(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    event_log: &Arc<Mutex<fs::File>>,
    connection_id: u64,
) -> Result<()> {
    loop {
        let Some(frame) = read_frame(client).context("read client websocket frame")? else {
            return Ok(());
        };
        if frame.opcode() == 0x1 && frame.fin() {
            if let Ok(text) = String::from_utf8(frame.payload.clone()) {
                record_ws_text(event_log, connection_id, "client_to_server", &text);
            }
        }
        write_frame_raw(upstream, &frame).context("write upstream websocket frame")?;
        if frame.opcode() == 0x8 {
            return Ok(());
        }
    }
}

fn proxy_upstream_to_client(
    mut upstream: TcpStream,
    mut client: TcpStream,
    event_log: &Arc<Mutex<fs::File>>,
    connection_id: u64,
) -> Result<()> {
    let result =
        proxy_upstream_to_client_inner(&mut upstream, &mut client, event_log, connection_id);
    shutdown_pair(&upstream, &client);
    result
}

fn proxy_upstream_to_client_inner(
    upstream: &mut TcpStream,
    client: &mut TcpStream,
    event_log: &Arc<Mutex<fs::File>>,
    connection_id: u64,
) -> Result<()> {
    loop {
        let Some(frame) = read_frame(upstream).context("read upstream websocket frame")? else {
            return Ok(());
        };
        if frame.opcode() == 0x1 && frame.fin() {
            if let Ok(text) = String::from_utf8(frame.payload.clone()) {
                record_ws_text(event_log, connection_id, "server_to_client", &text);
            }
        }
        write_frame_raw(client, &frame).context("write client websocket frame")?;
        if frame.opcode() == 0x8 {
            return Ok(());
        }
    }
}

fn shutdown_pair(left: &TcpStream, right: &TcpStream) {
    let _ = left.shutdown(Shutdown::Both);
    let _ = right.shutdown(Shutdown::Both);
}

fn websocket_upstream_request(request: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(request) else {
        return request.to_vec();
    };
    let mut lines = text.split("\r\n");
    let Some(request_line) = lines.next() else {
        return request.to_vec();
    };
    let mut normalized = String::new();
    normalized.push_str(request_line);
    normalized.push_str("\r\n");
    for line in lines {
        if line.is_empty() {
            normalized.push_str("\r\n");
            break;
        }
        let Some((name, _value)) = line.split_once(':') else {
            normalized.push_str(line);
            normalized.push_str("\r\n");
            continue;
        };
        if name.eq_ignore_ascii_case("sec-websocket-extensions") {
            continue;
        }
        normalized.push_str(line);
        normalized.push_str("\r\n");
    }
    normalized.into_bytes()
}

fn read_http_head(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).context("read HTTP byte")?;
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            return Ok(bytes);
        }
        if bytes.len() > MAX_HANDSHAKE_BYTES {
            anyhow::bail!("HTTP head exceeded {MAX_HANDSHAKE_BYTES} bytes");
        }
    }
}

fn read_frame(stream: &mut TcpStream) -> Result<Option<Frame>> {
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
        Err(err) => return Err(err).context("read frame header"),
    }
    let mut raw = Vec::from(header);
    let masked = header[1] & 0x80 != 0;
    let mut len = u64::from(header[1] & 0x7f);
    if len == 126 {
        let mut extended = [0_u8; 2];
        stream
            .read_exact(&mut extended)
            .context("read frame length")?;
        raw.extend_from_slice(&extended);
        len = u64::from(u16::from_be_bytes(extended));
    } else if len == 127 {
        let mut extended = [0_u8; 8];
        stream
            .read_exact(&mut extended)
            .context("read frame length")?;
        raw.extend_from_slice(&extended);
        len = u64::from_be_bytes(extended);
    }
    if len > MAX_FRAME_PAYLOAD {
        anyhow::bail!("websocket frame too large: {len} bytes");
    }

    let mut mask = [0_u8; 4];
    if masked {
        stream.read_exact(&mut mask).context("read frame mask")?;
        raw.extend_from_slice(&mask);
    }
    let mut payload = vec![0_u8; len as usize];
    stream
        .read_exact(&mut payload)
        .context("read frame payload")?;
    raw.extend_from_slice(&payload);
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % mask.len()];
        }
    }
    Ok(Some(Frame {
        first_byte: header[0],
        payload,
        raw,
    }))
}

fn write_frame_raw(stream: &mut TcpStream, frame: &Frame) -> Result<()> {
    stream
        .write_all(&frame.raw)
        .context("write websocket frame")
}

#[derive(Debug)]
struct Frame {
    first_byte: u8,
    payload: Vec<u8>,
    raw: Vec<u8>,
}

impl Frame {
    fn fin(&self) -> bool {
        self.first_byte & 0x80 != 0
    }

    fn opcode(&self) -> u8 {
        self.first_byte & 0x0f
    }
}

fn record_ws_text(
    event_log: &Arc<Mutex<fs::File>>,
    connection_id: u64,
    direction: &'static str,
    text: &str,
) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    let params = value.get("params");
    let record = ProxyEventRecord {
        timestamp_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default(),
        connection_id,
        direction,
        method,
        thread_id: params.and_then(extract_thread_id),
        turn_id: params.and_then(extract_turn_id),
        event_type: extract_event_type(&value),
        rate_limit_reached: has_rate_limit_fragment(&value),
    };
    let Ok(line) = serde_json::to_string(&record) else {
        return;
    };
    let Ok(mut file) = event_log.lock() else {
        return;
    };
    let _ = writeln!(file, "{line}");
}

fn open_event_log(path: &Path) -> Result<fs::File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("open app-server proxy event log {}", path.display()))?;
    set_private_event_log_permissions(&file)?;
    Ok(file)
}

#[cfg(unix)]
fn set_private_event_log_permissions(file: &fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .context("set app-server proxy event log permissions")
}

#[cfg(not(unix))]
fn set_private_event_log_permissions(_file: &fs::File) -> Result<()> {
    Ok(())
}

fn extract_thread_id(value: &Value) -> Option<String> {
    value
        .get("thread_id")
        .or_else(|| value.get("threadId"))
        .or_else(|| value.pointer("/thread/id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn extract_turn_id(value: &Value) -> Option<String> {
    value
        .get("turn_id")
        .or_else(|| value.get("turnId"))
        .or_else(|| value.pointer("/turn/id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn extract_event_type(value: &Value) -> Option<String> {
    value
        .pointer("/params/payload/type")
        .or_else(|| value.pointer("/params/event/type"))
        .or_else(|| value.pointer("/params/type"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn has_rate_limit_fragment(value: &Value) -> bool {
    value.pointer("/params/payload/info/rate_limits").is_some()
        || value.pointer("/params/payload/info/rateLimits").is_some()
        || value.pointer("/params/info/rate_limits").is_some()
        || value.pointer("/params/info/rateLimits").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_upstream_request_strips_extensions() {
        let request = b"GET /ws HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nSec-WebSocket-Extensions: permessage-deflate\r\nUpgrade: websocket\r\n\r\n";

        let normalized = String::from_utf8(websocket_upstream_request(request)).unwrap();

        assert!(normalized.starts_with("GET /ws HTTP/1.1\r\n"));
        assert!(normalized.contains("Host: 127.0.0.1:1234\r\n"));
        assert!(normalized.contains("Upgrade: websocket\r\n"));
        assert!(!normalized
            .to_ascii_lowercase()
            .contains("sec-websocket-extensions"));
        assert!(normalized.ends_with("\r\n\r\n"));
    }

    #[test]
    fn upstream_ready_requires_upstream_readyz_success() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 512];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
        });
        let url = LoopbackWsUrl::parse(&format!("ws://127.0.0.1:{port}")).unwrap();

        assert!(upstream_ready(&url));
        handle.join().unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = LoopbackWsUrl::parse(&format!("ws://127.0.0.1:{port}")).unwrap();

        assert!(!upstream_ready(&url));
    }

    #[test]
    fn read_frame_returns_none_on_idle_timeout() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(150));
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(25)))
            .unwrap();

        assert!(read_frame(&mut stream).unwrap().is_none());

        handle.join().unwrap();
    }

    #[test]
    fn client_close_shuts_down_upstream_even_when_another_clone_exists() {
        let client_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client_addr = client_listener.local_addr().unwrap();
        let upstream_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();

        let client_peer = TcpStream::connect(client_addr).unwrap();
        let (client_proxy, _) = client_listener.accept().unwrap();
        let upstream_proxy = TcpStream::connect(upstream_addr).unwrap();
        let upstream_held_clone = upstream_proxy.try_clone().unwrap();
        let (mut upstream_peer, _) = upstream_listener.accept().unwrap();
        upstream_peer
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let event_log = temp_event_log("client-close-shutdown");

        let handle = std::thread::spawn(move || {
            proxy_client_to_upstream(client_proxy, upstream_proxy, &event_log, 1).unwrap();
        });
        drop(client_peer);

        handle.join().unwrap();
        let mut byte = [0_u8; 1];
        match upstream_peer.read(&mut byte) {
            Ok(0) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof
                ) => {}
            other => panic!("expected upstream peer to close, got {other:?}"),
        }
        drop(upstream_held_clone);
    }

    #[test]
    fn record_ws_text_logs_metadata_without_message_body() {
        let path = temp_event_log_path("redacted");
        let event_log = Arc::new(Mutex::new(open_event_log(&path).unwrap()));
        let text = serde_json::json!({
            "id": 7,
            "method": "turn/start",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "prompt": "private prompt contents"
            }
        })
        .to_string();

        record_ws_text(&event_log, 3, "client_to_server", &text);
        event_log.lock().unwrap().flush().unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let record = serde_json::from_str::<Value>(contents.trim()).unwrap();
        assert_eq!(
            record.get("method").and_then(Value::as_str),
            Some("turn/start")
        );
        assert_eq!(
            record.get("threadId").and_then(Value::as_str),
            Some("thread-1")
        );
        assert_eq!(record.get("turnId").and_then(Value::as_str), Some("turn-1"));
        assert!(record.get("message").is_none());
        assert!(!contents.contains("private prompt contents"));

        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn event_log_file_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_event_log_path("permissions");
        let file = open_event_log(&path).unwrap();
        drop(file);

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let _ = fs::remove_file(path);
    }

    fn temp_event_log(name: &str) -> Arc<Mutex<fs::File>> {
        let path = temp_event_log_path(name);
        let file = open_event_log(&path).unwrap();
        Arc::new(Mutex::new(file))
    }

    fn temp_event_log_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "cx-proxy-{name}-{}-{unique}.jsonl",
            std::process::id()
        ))
    }
}
