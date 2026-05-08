use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
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
    listen_url: String,
    upstream: ProxyUpstream,
    event_log: PathBuf,
}

#[derive(Debug, Clone)]
enum ProxyUpstream {
    Fixed(String),
    Dynamic(Arc<RwLock<String>>),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProxyEventRecord<'a> {
    timestamp_unix_ms: u128,
    connection_id: u64,
    direction: &'static str,
    method: Option<&'a str>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    message: &'a Value,
}

impl AppServerProxy {
    pub(crate) fn new(listen_url: String, upstream_url: String, event_log: PathBuf) -> Self {
        Self {
            listen_url,
            upstream: ProxyUpstream::Fixed(upstream_url),
            event_log,
        }
    }

    pub(crate) fn new_dynamic(
        listen_url: String,
        upstream_url: Arc<RwLock<String>>,
        event_log: PathBuf,
    ) -> Self {
        Self {
            listen_url,
            upstream: ProxyUpstream::Dynamic(upstream_url),
            event_log,
        }
    }

    pub(crate) fn spawn(self) -> Result<thread::JoinHandle<()>> {
        let listen = LoopbackWsUrl::parse(&self.listen_url)?;
        if let Some(parent) = self.event_log.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let listener = TcpListener::bind((listen.host(), listen.port()))
            .with_context(|| format!("bind app-server proxy {}", self.listen_url))?;
        listener
            .set_nonblocking(false)
            .context("configure app-server proxy listener")?;
        let event_log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.event_log)
            .with_context(|| {
                format!(
                    "open app-server proxy event log {}",
                    self.event_log.display()
                )
            })?;
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
            Self::Dynamic(url) => {
                let url = url
                    .read()
                    .map_err(|_| anyhow::anyhow!("proxy upstream lock poisoned"))?;
                LoopbackWsUrl::parse(&url)
            }
        }
    }
}

fn proxy_client_to_upstream(
    mut client: TcpStream,
    mut upstream: TcpStream,
    event_log: &Arc<Mutex<fs::File>>,
    connection_id: u64,
) -> Result<()> {
    loop {
        let Some(frame) = read_frame(&mut client).context("read client websocket frame")? else {
            return Ok(());
        };
        if frame.opcode() == 0x1 && frame.fin() {
            if let Ok(text) = String::from_utf8(frame.payload.clone()) {
                record_ws_text(event_log, connection_id, "client_to_server", &text);
            }
        }
        write_frame_raw(&mut upstream, &frame).context("write upstream websocket frame")?;
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
    loop {
        let Some(frame) = read_frame(&mut upstream).context("read upstream websocket frame")?
        else {
            return Ok(());
        };
        if frame.opcode() == 0x1 && frame.fin() {
            if let Ok(text) = String::from_utf8(frame.payload.clone()) {
                record_ws_text(event_log, connection_id, "server_to_client", &text);
            }
        }
        write_frame_raw(&mut client, &frame).context("write client websocket frame")?;
        if frame.opcode() == 0x8 {
            return Ok(());
        }
    }
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
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => return Ok(None),
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
    let method = value.get("method").and_then(Value::as_str);
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
        message: &value,
    };
    let Ok(line) = serde_json::to_string(&record) else {
        return;
    };
    let Ok(mut file) = event_log.lock() else {
        return;
    };
    let _ = writeln!(file, "{line}");
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
}
