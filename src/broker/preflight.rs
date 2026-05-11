use std::collections::BTreeMap;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use mio::event::Event;
use mio::net::TcpListener as MioTcpListener;
use mio::net::TcpStream as MioTcpStream;
use mio::Events;
use mio::Interest;
use mio::Poll;
use mio::Token;

use super::health_response_bytes;
use super::start_client_connection;
use super::ws;
use super::BrokerShared;
use super::BrokerWebSocket;

const LISTENER: Token = Token(0);
const FIRST_CONNECTION_TOKEN: usize = 1;
const EVENT_CAPACITY: usize = 128;
const MAX_PREFLIGHT_CONNECTIONS: usize = 1024;
const CLIENT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_PREFLIGHT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn run_preflight_reactor(state: Arc<BrokerShared>, listener: TcpListener) -> Result<()> {
    let mut listener = MioTcpListener::from_std(listener);
    let mut poll = Poll::new().context("create broker preflight poller")?;
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)
        .context("register broker listener")?;

    let mut events = Events::with_capacity(EVENT_CAPACITY);
    let mut connections = BTreeMap::<Token, PreflightConnection>::new();
    let mut next_token = FIRST_CONNECTION_TOKEN;

    loop {
        expire_connections(poll.registry(), &mut connections);
        if state.shutdown.load(Ordering::Acquire) {
            break;
        }

        if let Err(err) = poll.poll(&mut events, next_poll_timeout(&connections)) {
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("poll broker preflight connections");
        }
        if state.shutdown.load(Ordering::Acquire) {
            break;
        }

        for event in events.iter() {
            if event.token() == LISTENER {
                accept_connections(
                    &state,
                    poll.registry(),
                    &mut listener,
                    &mut connections,
                    &mut next_token,
                )?;
            } else {
                handle_connection_event(&state, poll.registry(), &mut connections, event);
            }
        }
    }

    for (_token, mut connection) in connections {
        let _ = poll.registry().deregister(&mut connection.stream);
        let _ = connection.stream.shutdown(Shutdown::Both);
    }
    Ok(())
}

#[derive(Debug)]
struct PreflightConnection {
    stream: MioTcpStream,
    request: Vec<u8>,
    response: Option<PreflightResponse>,
    deadline: Instant,
}

#[derive(Debug)]
struct PreflightResponse {
    bytes: Vec<u8>,
    offset: usize,
    upgrade: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreflightProgress {
    Pending,
    Close,
    Upgrade,
}

impl PreflightConnection {
    fn new(stream: MioTcpStream) -> Self {
        Self {
            stream,
            request: Vec::new(),
            response: None,
            deadline: Instant::now() + CLIENT_HANDSHAKE_TIMEOUT,
        }
    }

    fn interest(&self) -> Interest {
        if self.response.is_some() {
            Interest::WRITABLE
        } else {
            Interest::READABLE
        }
    }

    fn process(&mut self, state: &BrokerShared, event: &Event) -> Result<PreflightProgress> {
        if self.response.is_none() && event.is_readable() {
            match self.read_request(state)? {
                PreflightProgress::Pending => {}
                complete => return Ok(complete),
            }
        }
        if self.response.is_some() && event.is_writable() {
            return self.write_response();
        }
        if self.response.is_some() {
            self.write_response()
        } else {
            Ok(PreflightProgress::Pending)
        }
    }

    fn read_request(&mut self, state: &BrokerShared) -> Result<PreflightProgress> {
        let mut buffer = [0_u8; 1024];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => return Ok(PreflightProgress::Close),
                Ok(count) => {
                    self.request.extend_from_slice(&buffer[..count]);
                    if self.request.len() > ws::MAX_HANDSHAKE_BYTES {
                        anyhow::bail!(
                            "broker client http head exceeded {} bytes",
                            ws::MAX_HANDSHAKE_BYTES
                        );
                    }
                    if self.request.ends_with(b"\r\n\r\n") {
                        self.prepare_response(state)?;
                        return self.write_response();
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(PreflightProgress::Pending);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err).context("read broker client request"),
            }
        }
    }

    fn prepare_response(&mut self, state: &BrokerShared) -> Result<()> {
        let request_line = ws::request_line(&self.request);
        let (bytes, upgrade) = if request_line.starts_with("GET /healthz ")
            || request_line.starts_with("GET /livez ")
        {
            (health_response_bytes(state.live()).to_vec(), false)
        } else if request_line.starts_with("GET /readyz ") {
            (
                health_response_bytes(state.observation_ready()).to_vec(),
                false,
            )
        } else if request_line.starts_with("GET /capacityz ") {
            (
                health_response_bytes(state.new_turn_capacity_ready()).to_vec(),
                false,
            )
        } else {
            (
                BrokerWebSocket::accept_response(&self.request)?.into_bytes(),
                true,
            )
        };
        self.response = Some(PreflightResponse {
            bytes,
            offset: 0,
            upgrade,
        });
        self.deadline = Instant::now() + CLIENT_PREFLIGHT_WRITE_TIMEOUT;
        Ok(())
    }

    fn write_response(&mut self) -> Result<PreflightProgress> {
        let Some(response) = self.response.as_mut() else {
            return Ok(PreflightProgress::Pending);
        };
        while response.offset < response.bytes.len() {
            match self.stream.write(&response.bytes[response.offset..]) {
                Ok(0) => return Ok(PreflightProgress::Close),
                Ok(count) => response.offset += count,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(PreflightProgress::Pending);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err).context("write broker preflight response"),
            }
        }
        if response.upgrade {
            Ok(PreflightProgress::Upgrade)
        } else {
            Ok(PreflightProgress::Close)
        }
    }
}

fn accept_connections(
    state: &BrokerShared,
    registry: &mio::Registry,
    listener: &mut MioTcpListener,
    connections: &mut BTreeMap<Token, PreflightConnection>,
    next_token: &mut usize,
) -> Result<()> {
    loop {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                if state.shutdown.load(Ordering::Acquire) {
                    let _ = stream.shutdown(Shutdown::Both);
                    return Ok(());
                }
                if connections.len() >= MAX_PREFLIGHT_CONNECTIONS {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                let Some(token) = allocate_token(connections, next_token) else {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                };
                registry
                    .register(&mut stream, token, Interest::READABLE)
                    .context("register broker preflight connection")?;
                connections.insert(token, PreflightConnection::new(stream));
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err).context("accept broker client"),
        }
    }
}

fn handle_connection_event(
    state: &Arc<BrokerShared>,
    registry: &mio::Registry,
    connections: &mut BTreeMap<Token, PreflightConnection>,
    event: &Event,
) {
    let token = event.token();
    let Some(mut connection) = connections.remove(&token) else {
        return;
    };

    let progress = connection.process(state, event);
    match progress {
        Ok(PreflightProgress::Pending) => {
            let interest = connection.interest();
            if let Err(err) = registry.reregister(&mut connection.stream, token, interest) {
                eprintln!("broker preflight reregister failed: {err}");
                let _ = connection.stream.shutdown(Shutdown::Both);
                return;
            }
            connections.insert(token, connection);
        }
        Ok(PreflightProgress::Close) => {
            let _ = registry.deregister(&mut connection.stream);
            let _ = connection.stream.shutdown(Shutdown::Both);
        }
        Ok(PreflightProgress::Upgrade) => {
            let _ = registry.deregister(&mut connection.stream);
            match finish_upgrade(connection.stream) {
                Ok(socket) => start_client_connection(Arc::clone(state), socket),
                Err(err) => eprintln!("broker websocket finish accept failed: {err:#}"),
            }
        }
        Err(err) => {
            let _ = registry.deregister(&mut connection.stream);
            let _ = connection.stream.shutdown(Shutdown::Both);
            if !state.shutdown.load(Ordering::Acquire) {
                eprintln!("broker preflight connection failed: {err:#}");
            }
        }
    }
}

fn finish_upgrade(stream: MioTcpStream) -> Result<BrokerWebSocket> {
    let stream = TcpStream::from(stream);
    stream
        .set_nonblocking(false)
        .context("set broker websocket blocking after handshake")?;
    BrokerWebSocket::finish_accept(stream)
}

fn expire_connections(
    registry: &mio::Registry,
    connections: &mut BTreeMap<Token, PreflightConnection>,
) {
    let now = Instant::now();
    let expired = connections
        .iter()
        .filter_map(|(token, connection)| (connection.deadline <= now).then_some(*token))
        .collect::<Vec<_>>();
    for token in expired {
        if let Some(mut connection) = connections.remove(&token) {
            let _ = registry.deregister(&mut connection.stream);
            let _ = connection.stream.shutdown(Shutdown::Both);
        }
    }
}

fn next_poll_timeout(connections: &BTreeMap<Token, PreflightConnection>) -> Option<Duration> {
    let next_deadline = connections
        .values()
        .map(|connection| connection.deadline)
        .min()?;
    Some(next_deadline.saturating_duration_since(Instant::now()))
}

fn allocate_token(
    connections: &BTreeMap<Token, PreflightConnection>,
    next_token: &mut usize,
) -> Option<Token> {
    for _ in 0..usize::MAX {
        let token = Token(*next_token);
        *next_token = next_token
            .checked_add(1)
            .filter(|next| *next != LISTENER.0)
            .unwrap_or(FIRST_CONNECTION_TOKEN);
        if !connections.contains_key(&token) {
            return Some(token);
        }
    }
    None
}
