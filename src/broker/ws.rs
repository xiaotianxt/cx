//! Minimal WebSocket transport used by the broker.
//!
//! The broker only accepts loopback clients and only speaks text frames for the
//! Codex app-server JSON-RPC envelope. This module keeps framing mechanics out
//! of routing and worker scheduling.

use std::io::ErrorKind;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use sha1::Digest;
use sha1::Sha1;

use crate::app_server::LoopbackWsUrl;

pub(crate) const MAX_HANDSHAKE_BYTES: usize = 16 * 1024;
const MAX_FRAME_PAYLOAD: u64 = 16 * 1024 * 1024;
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerRole {
    Client,
    Server,
}

pub(crate) struct BrokerWebSocket {
    reader: WebSocketReader,
    writer: WebSocketWriter,
}

pub(crate) struct WebSocketReader {
    stream: TcpStream,
}

pub(crate) struct WebSocketWriter {
    stream: TcpStream,
    role: PeerRole,
    mask_seed: u64,
}

pub(crate) enum WebSocketMessage {
    Text(String),
    Ping(Vec<u8>),
    Closed,
}

impl BrokerWebSocket {
    pub(crate) fn connect(url: &str, timeout: Duration) -> Result<Self> {
        let url = LoopbackWsUrl::parse(url)?;
        let mut stream = TcpStream::connect((url.host(), url.port()))
            .with_context(|| format!("connect broker worker {url:?}"))?;
        stream
            .set_read_timeout(Some(timeout))
            .context("set worker websocket read timeout")?;
        stream
            .set_write_timeout(Some(timeout))
            .context("set worker websocket write timeout")?;

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
            .context("write worker websocket handshake")?;
        validate_client_handshake(&mut stream, &key)?;
        stream
            .set_read_timeout(None)
            .context("set worker websocket blocking read")?;
        Self::from_stream(stream, PeerRole::Client)
    }

    pub(crate) fn finish_accept(stream: TcpStream) -> Result<Self> {
        stream
            .set_read_timeout(None)
            .context("set broker websocket blocking read")?;
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .context("set broker websocket write timeout")?;
        Self::from_stream(stream, PeerRole::Server)
    }

    pub(crate) fn accept_response(request: &[u8]) -> Result<String> {
        let key = header_value(request, "Sec-WebSocket-Key")
            .context("websocket request missing Sec-WebSocket-Key")?;
        let accept = websocket_accept(key);
        Ok(format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\
             \r\n"
        ))
    }

    pub(crate) fn split(self) -> (WebSocketReader, WebSocketWriter) {
        (self.reader, self.writer)
    }

    pub(crate) fn send_text(&mut self, text: &str) -> Result<()> {
        self.writer.send_text(text)
    }

    pub(crate) fn send_pong(&mut self, payload: &[u8]) -> Result<()> {
        self.writer.send_pong(payload)
    }

    pub(crate) fn read_text_blocking(&mut self) -> Result<Option<String>> {
        loop {
            match self.reader.read_text_blocking()? {
                WebSocketMessage::Text(text) => return Ok(Some(text)),
                WebSocketMessage::Ping(payload) => self.send_pong(&payload)?,
                WebSocketMessage::Closed => return Ok(None),
            }
        }
    }

    pub(crate) fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        self.reader.set_read_timeout(timeout)
    }

    fn from_stream(stream: TcpStream, role: PeerRole) -> Result<Self> {
        let writer_stream = stream.try_clone().context("clone websocket stream")?;
        Ok(Self {
            reader: WebSocketReader { stream },
            writer: WebSocketWriter {
                stream: writer_stream,
                role,
                mask_seed: random_u64(),
            },
        })
    }
}

impl WebSocketReader {
    pub(crate) fn read_text_blocking(&mut self) -> Result<WebSocketMessage> {
        loop {
            let Some(frame) = self.read_frame_blocking()? else {
                return Ok(WebSocketMessage::Closed);
            };
            match frame.opcode {
                0x1 => {
                    if !frame.fin {
                        anyhow::bail!("fragmented websocket text messages are unsupported");
                    }
                    let text = String::from_utf8(frame.payload).context("decode websocket text")?;
                    return Ok(WebSocketMessage::Text(text));
                }
                0x8 => return Ok(WebSocketMessage::Closed),
                0x9 => return Ok(WebSocketMessage::Ping(frame.payload)),
                0xA => {}
                opcode => anyhow::bail!("unsupported websocket opcode: {opcode}"),
            }
        }
    }

    pub(crate) fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        self.stream
            .set_read_timeout(timeout)
            .context("set websocket read timeout")
    }

    pub(crate) fn shutdown(&self, how: std::net::Shutdown) -> Result<()> {
        self.stream
            .shutdown(how)
            .context("shutdown websocket reader")
    }

    fn read_frame_blocking(&mut self) -> Result<Option<Frame>> {
        let mut header = [0_u8; 2];
        match self.stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::UnexpectedEof
                        | ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(None);
            }
            Err(err) => return Err(err).context("read websocket frame header"),
        }
        let fin = header[0] & 0x80 != 0;
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let mut len = u64::from(header[1] & 0x7f);
        if len == 126 {
            let mut extended = [0_u8; 2];
            self.stream
                .read_exact(&mut extended)
                .context("read websocket frame length")?;
            len = u64::from(u16::from_be_bytes(extended));
        } else if len == 127 {
            let mut extended = [0_u8; 8];
            self.stream
                .read_exact(&mut extended)
                .context("read websocket frame length")?;
            len = u64::from_be_bytes(extended);
        }
        if len > MAX_FRAME_PAYLOAD {
            anyhow::bail!("websocket frame too large: {len} bytes");
        }

        let mut mask = [0_u8; 4];
        if masked {
            self.stream
                .read_exact(&mut mask)
                .context("read websocket frame mask")?;
        }

        let mut payload = vec![0_u8; len as usize];
        self.stream
            .read_exact(&mut payload)
            .context("read websocket frame payload")?;
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
}

impl WebSocketWriter {
    pub(crate) fn send_text(&mut self, text: &str) -> Result<()> {
        self.send_frame(0x1, text.as_bytes())
    }

    pub(crate) fn send_pong(&mut self, payload: &[u8]) -> Result<()> {
        self.send_frame(0xA, payload)
    }

    pub(crate) fn shutdown(&self, how: std::net::Shutdown) -> Result<()> {
        self.stream
            .shutdown(how)
            .context("shutdown websocket writer")
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let mask_bit = if self.role == PeerRole::Client {
            0x80
        } else {
            0
        };
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

        if self.role == PeerRole::Client {
            let mask = self.next_mask();
            frame.extend_from_slice(&mask);
            for (index, byte) in payload.iter().enumerate() {
                frame.push(byte ^ mask[index % mask.len()]);
            }
        } else {
            frame.extend_from_slice(payload);
        }
        self.stream
            .write_all(&frame)
            .context("write websocket frame")
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

pub(crate) fn read_http_head(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        stream
            .read_exact(&mut byte)
            .context("read websocket http head")?;
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            return Ok(bytes);
        }
        if bytes.len() > MAX_HANDSHAKE_BYTES {
            anyhow::bail!("websocket http head exceeded {MAX_HANDSHAKE_BYTES} bytes");
        }
    }
}

pub(crate) fn request_line(request: &[u8]) -> String {
    String::from_utf8_lossy(request)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

fn validate_client_handshake(stream: &mut TcpStream, key: &str) -> Result<()> {
    let response = read_http_head(stream).context("read worker websocket handshake")?;
    let response_text = String::from_utf8_lossy(&response);
    let status = response_text.lines().next().unwrap_or_default();
    if !status.starts_with("HTTP/1.1 101 ") && !status.starts_with("HTTP/1.0 101 ") {
        anyhow::bail!("worker websocket handshake failed: {status}");
    }
    let expected_accept = websocket_accept(key);
    let actual_accept = header_value(&response, "Sec-WebSocket-Accept")
        .context("worker websocket response missing Sec-WebSocket-Accept")?;
    if actual_accept.trim() != expected_accept {
        anyhow::bail!("worker websocket accept key mismatch");
    }
    Ok(())
}

fn websocket_key() -> String {
    let mut seed = random_u64();
    let mut bytes = [0_u8; 16];
    for chunk in bytes.chunks_mut(8) {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        chunk.copy_from_slice(&seed.to_be_bytes()[..chunk.len()]);
    }
    STANDARD.encode(bytes)
}

fn websocket_accept(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WEBSOCKET_GUID.as_bytes());
    STANDARD.encode(hasher.finalize())
}

fn header_value<'a>(head: &'a [u8], name: &str) -> Option<&'a str> {
    let text = std::str::from_utf8(head).ok()?;
    for line in text.lines() {
        let Some((header, value)) = line.split_once(':') else {
            continue;
        };
        if header.eq_ignore_ascii_case(name) {
            return Some(value.trim());
        }
    }
    None
}

fn random_u64() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ u64::from(std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_value_skips_status_line() {
        let head = b"HTTP/1.1 101 Switching Protocols\r\n\
            Upgrade: websocket\r\n\
            Sec-WebSocket-Accept: accept-key\r\n\
            \r\n";

        assert_eq!(
            header_value(head, "Sec-WebSocket-Accept"),
            Some("accept-key")
        );
        assert_eq!(header_value(head, "Missing"), None);
    }
}
