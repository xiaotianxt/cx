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
const DEFAULT_MAX_PREFLIGHT_CONNECTIONS: usize = 1024;
const PREFLIGHT_FD_RESERVE: usize = 64;
const PREFLIGHT_FD_SHARE: usize = 4;
const MAX_ACCEPTS_PER_TICK: usize = 64;
const CLIENT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const CLIENT_PREFLIGHT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const ACCEPT_RESOURCE_BACKOFF: Duration = Duration::from_millis(250);

pub(super) fn run_preflight_reactor(state: Arc<BrokerShared>, listener: TcpListener) -> Result<()> {
    run_preflight_reactor_with_limits(state, listener, PreflightLimits::from_process())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PreflightLimits {
    total: usize,
    incomplete: usize,
}

impl PreflightLimits {
    fn from_process() -> Self {
        let total = preflight_connection_limit();
        Self {
            total,
            incomplete: incomplete_handshake_limit(total),
        }
    }

    #[cfg(test)]
    pub(super) fn new_for_test(total: usize, incomplete: usize) -> Self {
        Self { total, incomplete }
    }
}

pub(super) fn run_preflight_reactor_with_limits(
    state: Arc<BrokerShared>,
    listener: TcpListener,
    limits: PreflightLimits,
) -> Result<()> {
    let mut listener = MioTcpListener::from_std(listener);
    let mut poll = Poll::new().context("create broker preflight poller")?;
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)
        .context("register broker listener")?;

    let mut events = Events::with_capacity(EVENT_CAPACITY);
    let mut connections = BTreeMap::<Token, PreflightConnection>::new();
    let mut next_token = FIRST_CONNECTION_TOKEN;
    let mut listener_registered = true;
    let mut accept_resume_at = None;

    loop {
        expire_connections(poll.registry(), &mut connections);
        resume_accept_if_due(
            poll.registry(),
            &mut listener,
            &mut listener_registered,
            &mut accept_resume_at,
        )?;
        if state.shutdown.load(Ordering::Acquire) {
            break;
        }

        if let Err(err) = poll.poll(
            &mut events,
            next_poll_timeout(&connections, accept_resume_at),
        ) {
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("poll broker preflight connections");
        }
        if state.shutdown.load(Ordering::Acquire) {
            break;
        }

        for event in events.iter() {
            if event.token() == LISTENER && listener_registered {
                match accept_connections(
                    &state,
                    poll.registry(),
                    &mut listener,
                    &mut connections,
                    &mut next_token,
                    limits,
                )? {
                    AcceptOutcome::Ready => {}
                    AcceptOutcome::ResourceExhausted => {
                        pause_accept(
                            poll.registry(),
                            &mut listener,
                            &mut listener_registered,
                            &mut accept_resume_at,
                        )?;
                    }
                }
            } else {
                handle_connection_event(&state, poll.registry(), &mut connections, event);
            }
        }
    }

    if listener_registered {
        let _ = poll.registry().deregister(&mut listener);
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
    initial_websocket_bytes: Vec<u8>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptOutcome {
    Ready,
    ResourceExhausted,
}

impl PreflightConnection {
    fn new(stream: MioTcpStream) -> Self {
        Self {
            stream,
            request: Vec::new(),
            initial_websocket_bytes: Vec::new(),
            response: None,
            deadline: Instant::now() + CLIENT_HANDSHAKE_TIMEOUT,
        }
    }

    fn incomplete(&self) -> bool {
        self.response.is_none()
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
                    if let Some(head_end) = http_head_end(&self.request) {
                        self.initial_websocket_bytes = self.request.split_off(head_end);
                        self.prepare_response(state)?;
                        return self.write_response();
                    }
                    if self.request.len() > ws::MAX_HANDSHAKE_BYTES {
                        anyhow::bail!(
                            "broker client http head exceeded {} bytes",
                            ws::MAX_HANDSHAKE_BYTES
                        );
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
    state: &Arc<BrokerShared>,
    registry: &mio::Registry,
    listener: &mut MioTcpListener,
    connections: &mut BTreeMap<Token, PreflightConnection>,
    next_token: &mut usize,
    limits: PreflightLimits,
) -> Result<AcceptOutcome> {
    let mut accepted = 0;
    loop {
        if accepted >= MAX_ACCEPTS_PER_TICK {
            return Ok(AcceptOutcome::Ready);
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                accepted += 1;
                if state.shutdown.load(Ordering::Acquire) {
                    let _ = stream.shutdown(Shutdown::Both);
                    return Ok(AcceptOutcome::Ready);
                }
                if connections.len() >= limits.total {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                let Some(token) = allocate_token(connections, next_token) else {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                };
                let over_incomplete_limit =
                    incomplete_connection_count(connections) >= limits.incomplete;
                let mut connection = PreflightConnection::new(stream);
                match connection.read_request(state) {
                    Ok(PreflightProgress::Pending)
                        if over_incomplete_limit && connection.incomplete() =>
                    {
                        let _ = connection.stream.shutdown(Shutdown::Both);
                    }
                    Ok(PreflightProgress::Pending) => {
                        let interest = connection.interest();
                        registry
                            .register(&mut connection.stream, token, interest)
                            .context("register broker preflight connection")?;
                        connections.insert(token, connection);
                    }
                    Ok(PreflightProgress::Close) => {
                        let _ = connection.stream.shutdown(Shutdown::Both);
                    }
                    Ok(PreflightProgress::Upgrade) => {
                        match finish_upgrade(connection.stream, connection.initial_websocket_bytes)
                        {
                            Ok(socket) => start_client_connection(Arc::clone(state), socket),
                            Err(err) => {
                                eprintln!("broker websocket finish accept failed: {err:#}");
                            }
                        }
                    }
                    Err(err) => {
                        let _ = connection.stream.shutdown(Shutdown::Both);
                        if !state.shutdown.load(Ordering::Acquire) {
                            eprintln!("broker preflight connection failed: {err:#}");
                        }
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(AcceptOutcome::Ready),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) if is_fd_exhaustion(&err) => {
                eprintln!("broker accept paused after file descriptor exhaustion: {err}");
                return Ok(AcceptOutcome::ResourceExhausted);
            }
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
            match finish_upgrade(connection.stream, connection.initial_websocket_bytes) {
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

fn finish_upgrade(
    stream: MioTcpStream,
    initial_websocket_bytes: Vec<u8>,
) -> Result<BrokerWebSocket> {
    let stream = TcpStream::from(stream);
    stream
        .set_nonblocking(false)
        .context("set broker websocket blocking after handshake")?;
    BrokerWebSocket::finish_accept_with_buffer(stream, initial_websocket_bytes)
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

fn resume_accept_if_due(
    registry: &mio::Registry,
    listener: &mut MioTcpListener,
    listener_registered: &mut bool,
    accept_resume_at: &mut Option<Instant>,
) -> Result<()> {
    if *listener_registered {
        return Ok(());
    }
    let Some(deadline) = accept_resume_at else {
        return Ok(());
    };
    if *deadline > Instant::now() {
        return Ok(());
    }
    registry
        .register(listener, LISTENER, Interest::READABLE)
        .context("resume broker listener after fd exhaustion")?;
    *listener_registered = true;
    *accept_resume_at = None;
    Ok(())
}

fn pause_accept(
    registry: &mio::Registry,
    listener: &mut MioTcpListener,
    listener_registered: &mut bool,
    accept_resume_at: &mut Option<Instant>,
) -> Result<()> {
    registry
        .deregister(listener)
        .context("pause broker listener after fd exhaustion")?;
    *listener_registered = false;
    *accept_resume_at = Some(Instant::now() + ACCEPT_RESOURCE_BACKOFF);
    Ok(())
}

fn next_poll_timeout(
    connections: &BTreeMap<Token, PreflightConnection>,
    accept_resume_at: Option<Instant>,
) -> Option<Duration> {
    let next_connection_deadline = connections
        .values()
        .map(|connection| connection.deadline)
        .min();
    let next_deadline = match (next_connection_deadline, accept_resume_at) {
        (Some(connection), Some(accept)) => Some(connection.min(accept)),
        (Some(connection), None) => Some(connection),
        (None, Some(accept)) => Some(accept),
        (None, None) => None,
    }?;
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

fn incomplete_connection_count(connections: &BTreeMap<Token, PreflightConnection>) -> usize {
    connections
        .values()
        .filter(|connection| connection.incomplete())
        .count()
}

fn http_head_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(b"\r\n\r\n".len())
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + b"\r\n\r\n".len())
}

fn preflight_connection_limit() -> usize {
    preflight_limit_for_fd_soft_limit(process_fd_soft_limit())
}

fn preflight_limit_for_fd_soft_limit(soft_limit: Option<usize>) -> usize {
    let Some(soft_limit) = soft_limit else {
        return DEFAULT_MAX_PREFLIGHT_CONNECTIONS;
    };
    let budget = soft_limit.saturating_sub(PREFLIGHT_FD_RESERVE) / PREFLIGHT_FD_SHARE;
    budget.clamp(1, DEFAULT_MAX_PREFLIGHT_CONNECTIONS)
}

fn incomplete_handshake_limit(total: usize) -> usize {
    if total <= 1 {
        return 1;
    }
    total.saturating_sub((total / 4).clamp(1, 16)).max(1)
}

#[cfg(unix)]
fn process_fd_soft_limit() -> Option<usize> {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: getrlimit writes a plain rlimit value into the provided pointer
    // and does not retain it.
    let result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    // SAFETY: getrlimit succeeded, so the rlimit struct has been initialized.
    let limit = unsafe { limit.assume_init() };
    if limit.rlim_cur == libc::RLIM_INFINITY {
        return None;
    }
    usize::try_from(limit.rlim_cur).ok()
}

#[cfg(not(unix))]
fn process_fd_soft_limit() -> Option<usize> {
    None
}

#[cfg(unix)]
fn is_fd_exhaustion(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

#[cfg(not(unix))]
fn is_fd_exhaustion(_err: &io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_limit_uses_fraction_of_fd_budget() {
        assert_eq!(
            preflight_limit_for_fd_soft_limit(None),
            DEFAULT_MAX_PREFLIGHT_CONNECTIONS
        );
        assert_eq!(preflight_limit_for_fd_soft_limit(Some(256)), 48);
        assert_eq!(
            preflight_limit_for_fd_soft_limit(Some(100_000)),
            DEFAULT_MAX_PREFLIGHT_CONNECTIONS
        );
        assert_eq!(preflight_limit_for_fd_soft_limit(Some(32)), 1);
    }

    #[test]
    fn incomplete_handshake_limit_reserves_accept_capacity() {
        assert_eq!(incomplete_handshake_limit(1), 1);
        assert_eq!(incomplete_handshake_limit(8), 6);
        assert_eq!(incomplete_handshake_limit(48), 36);
        assert_eq!(incomplete_handshake_limit(1024), 1008);
    }

    #[test]
    fn http_head_end_finds_delimiter_before_extra_bytes() {
        let bytes = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n\x81\x00";

        assert_eq!(http_head_end(bytes), Some(bytes.len() - 2));
        assert_eq!(http_head_end(b"GET / HTTP/1.1\r\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn fd_exhaustion_errors_are_recoverable_accept_errors() {
        assert!(is_fd_exhaustion(&io::Error::from_raw_os_error(
            libc::EMFILE
        )));
        assert!(is_fd_exhaustion(&io::Error::from_raw_os_error(
            libc::ENFILE
        )));
        assert!(!is_fd_exhaustion(&io::Error::from_raw_os_error(
            libc::ECONNABORTED
        )));
    }
}
