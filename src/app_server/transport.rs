use std::fs::File;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sha1::Digest;
use sha1::Sha1;

const MAX_FRAME_PAYLOAD: u64 = 16 * 1024 * 1024;
const MAX_HANDSHAKE_BYTES: usize = 16 * 1024;
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoopbackWsUrl {
    host: String,
    port: u16,
}

pub(super) struct WebSocket {
    stream: TcpStream,
    mask_seed: u64,
}

impl LoopbackWsUrl {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let Some(rest) = raw.strip_prefix("ws://") else {
            anyhow::bail!("app-server URL must use ws://");
        };
        if rest.contains('/') {
            anyhow::bail!("app-server URL must not include a path");
        }
        let Some((host, port)) = rest.rsplit_once(':') else {
            anyhow::bail!("app-server URL requires host:port");
        };
        if !matches!(host, "127.0.0.1" | "localhost") {
            anyhow::bail!("app-server URL must use loopback host 127.0.0.1 or localhost");
        }
        let port = port
            .parse::<u16>()
            .with_context(|| format!("invalid app-server port: {port}"))?;
        Ok(Self {
            host: host.to_string(),
            port,
        })
    }

    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }
}

impl WebSocket {
    pub(super) fn connect(url: &LoopbackWsUrl, timeout: Duration) -> Result<Self> {
        let mut stream = connect_loopback(url, timeout)?;
        stream
            .set_read_timeout(Some(timeout))
            .context("set app-server read timeout")?;
        stream
            .set_write_timeout(Some(timeout))
            .context("set app-server write timeout")?;

        let key = websocket_key();
        let request = format!(
            "GET / HTTP/1.1\r\n\
             Host: {}:{}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n",
            url.host(),
            url.port(),
        );
        stream
            .write_all(request.as_bytes())
            .context("write app-server websocket handshake")?;
        validate_handshake(&mut stream, &key)?;

        Ok(Self {
            stream,
            mask_seed: random_u64(),
        })
    }

    pub(super) fn send_text(&mut self, text: &str) -> Result<()> {
        self.send_frame(0x1, text.as_bytes())
    }

    pub(super) fn shutdown(&mut self) -> Result<()> {
        self.stream
            .shutdown(Shutdown::Both)
            .context("shutdown app-server websocket")
    }

    pub(super) fn read_text(&mut self) -> Result<String> {
        loop {
            let frame = self.read_frame()?.context("app-server websocket closed")?;
            match frame.opcode {
                0x1 => {
                    if !frame.fin {
                        anyhow::bail!("fragmented app-server websocket messages are unsupported");
                    }
                    return String::from_utf8(frame.payload)
                        .context("decode app-server text frame");
                }
                0x8 => anyhow::bail!("app-server websocket closed"),
                0x9 => self.send_frame(0xA, &frame.payload)?,
                0xA => {}
                opcode => anyhow::bail!("unsupported app-server websocket opcode: {opcode}"),
            }
        }
    }

    pub(super) fn read_text_optional(&mut self) -> Result<Option<String>> {
        loop {
            let Some(frame) = self.read_frame()? else {
                return Ok(None);
            };
            match frame.opcode {
                0x1 => {
                    if !frame.fin {
                        anyhow::bail!("fragmented app-server websocket messages are unsupported");
                    }
                    return String::from_utf8(frame.payload)
                        .map(Some)
                        .context("decode app-server text frame");
                }
                0x8 => return Ok(None),
                0x9 => self.send_frame(0xA, &frame.payload)?,
                0xA => {}
                opcode => anyhow::bail!("unsupported app-server websocket opcode: {opcode}"),
            }
        }
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let mask_bit = 0x80;
        match payload.len() {
            len if len < 126 => frame.push(mask_bit | len as u8),
            len if len <= u16::MAX as usize => {
                frame.push(mask_bit | 126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            }
            len => {
                frame.push(mask_bit | 127);
                frame.extend_from_slice(&(len as u64).to_be_bytes());
            }
        }

        let mask = self.next_mask();
        frame.extend_from_slice(&mask);
        for (index, byte) in payload.iter().enumerate() {
            frame.push(byte ^ mask[index % mask.len()]);
        }
        self.stream
            .write_all(&frame)
            .context("write app-server websocket frame")
    }

    fn read_frame(&mut self) -> Result<Option<Frame>> {
        let mut header = [0_u8; 2];
        match self.stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::UnexpectedEof | ErrorKind::TimedOut | ErrorKind::WouldBlock
                ) =>
            {
                return Ok(None);
            }
            Err(err) => return Err(err).context("read app-server websocket frame header"),
        }
        let fin = header[0] & 0x80 != 0;
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let mut len = u64::from(header[1] & 0x7f);
        if len == 126 {
            let mut extended = [0_u8; 2];
            self.stream
                .read_exact(&mut extended)
                .context("read app-server websocket frame length")?;
            len = u64::from(u16::from_be_bytes(extended));
        } else if len == 127 {
            let mut extended = [0_u8; 8];
            self.stream
                .read_exact(&mut extended)
                .context("read app-server websocket frame length")?;
            len = u64::from_be_bytes(extended);
        }
        if len > MAX_FRAME_PAYLOAD {
            anyhow::bail!("app-server websocket frame too large: {len} bytes");
        }

        let mut mask = [0_u8; 4];
        if masked {
            self.stream
                .read_exact(&mut mask)
                .context("read app-server websocket frame mask")?;
        }

        let mut payload = vec![0_u8; len as usize];
        self.stream
            .read_exact(&mut payload)
            .context("read app-server websocket frame payload")?;
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % mask.len()];
            }
        }
        Ok(Some(Frame {
            fin,
            opcode,
            payload,
        }))
    }

    fn next_mask(&mut self) -> [u8; 4] {
        self.mask_seed = self
            .mask_seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.mask_seed as u32).to_be_bytes()
    }
}

#[derive(Debug)]
struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

fn connect_loopback(url: &LoopbackWsUrl, timeout: Duration) -> Result<TcpStream> {
    let addrs = (url.host(), url.port())
        .to_socket_addrs()
        .with_context(|| format!("resolve {}:{}", url.host(), url.port()))?;
    let mut last_err = None;
    for addr in addrs {
        if !addr.ip().is_loopback() {
            continue;
        }
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(err) => last_err = Some(err),
        }
    }
    match last_err {
        Some(err) => Err(err).with_context(|| format!("connect to app-server {url:?}")),
        None => anyhow::bail!("app-server URL did not resolve to loopback: {url:?}"),
    }
}

fn validate_handshake(stream: &mut TcpStream, key: &str) -> Result<()> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        stream
            .read_exact(&mut byte)
            .context("read app-server websocket handshake")?;
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
        if bytes.len() > MAX_HANDSHAKE_BYTES {
            anyhow::bail!("app-server websocket handshake exceeded {MAX_HANDSHAKE_BYTES} bytes");
        }
    }

    let response = String::from_utf8(bytes).context("decode app-server websocket handshake")?;
    let mut lines = response.split("\r\n");
    let status = lines.next().unwrap_or_default();
    if !status.starts_with("HTTP/1.1 101 ") {
        anyhow::bail!("app-server websocket upgrade failed: {status}");
    }

    let mut accept = None;
    let mut upgraded = false;
    let mut connection_upgrade = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "sec-websocket-accept" => accept = Some(value.to_string()),
            "upgrade" => upgraded = value.eq_ignore_ascii_case("websocket"),
            "connection" => {
                connection_upgrade = value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
            }
            _ => {}
        }
    }

    if !upgraded || !connection_upgrade {
        anyhow::bail!("app-server websocket handshake missing upgrade headers");
    }
    let expected = websocket_accept(key);
    if accept.as_deref() != Some(expected.as_str()) {
        anyhow::bail!("app-server websocket accept header did not match");
    }
    Ok(())
}

fn websocket_key() -> String {
    let mut bytes = [0_u8; 16];
    fill_randomish(&mut bytes);
    STANDARD.encode(bytes)
}

fn websocket_accept(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WEBSOCKET_GUID.as_bytes());
    STANDARD.encode(hasher.finalize())
}

fn nonce64() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default();
    nanos ^ u64::from(std::process::id())
}

fn random_u64() -> u64 {
    let mut bytes = [0_u8; 8];
    fill_randomish(&mut bytes);
    u64::from_be_bytes(bytes)
}

fn fill_randomish(bytes: &mut [u8]) {
    if fill_os_random(bytes).is_ok() {
        return;
    }
    let mut seed = nonce64();
    for chunk in bytes.chunks_mut(8) {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let seed_bytes = seed.to_be_bytes();
        chunk.copy_from_slice(&seed_bytes[..chunk.len()]);
    }
}

fn fill_os_random(bytes: &mut [u8]) -> Result<()> {
    #[cfg(unix)]
    {
        File::open("/dev/urandom")
            .context("open /dev/urandom")?
            .read_exact(bytes)
            .context("read /dev/urandom")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = bytes;
        anyhow::bail!("no OS random source configured");
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    #[test]
    fn loopback_url_rejects_non_loopback_host() {
        let err = LoopbackWsUrl::parse("ws://0.0.0.0:1234").unwrap_err();

        assert!(format!("{err:#}").contains("loopback"));
    }

    #[test]
    fn websocket_accept_matches_rfc_example() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn websocket_connect_preserves_first_frame_after_handshake() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let key = request
                .lines()
                .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
                .unwrap();
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {}\r\n\
                 \r\n",
                websocket_accept(key)
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream
                .write_all(&server_text_frame(br#"{"id":1,"result":{}}"#))
                .unwrap();
        });
        let url = LoopbackWsUrl::parse(&format!("ws://127.0.0.1:{port}")).unwrap();
        let mut websocket = WebSocket::connect(&url, Duration::from_secs(2)).unwrap();

        let text = websocket.read_text().unwrap();

        assert_eq!(text, r#"{"id":1,"result":{}}"#);
        server.join().unwrap();
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        loop {
            let mut byte = [0_u8; 1];
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            if bytes.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    fn server_text_frame(payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0x81];
        if payload.len() < 126 {
            frame.push(payload.len() as u8);
        } else {
            frame.push(126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(payload);
        frame
    }
}
