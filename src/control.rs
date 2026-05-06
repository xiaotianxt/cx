use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use crate::cli::ServeDaemonArgs;
use crate::cli::ServeEventArgs;
use crate::cli::ServeEventCommand;
use crate::cli::ServeLeaseArgs;
use crate::cli::ServeLeaseCommand;
use crate::cli::ServePingArgs;
use crate::cli::ServeSessionArgs;
use crate::cli::ServeSessionCommand;
use crate::cli::ServeShutdownArgs;
use crate::paths::ManagerPaths;
use crate::session;
use crate::session::AcquireLeaseRequest;
use crate::session::ChannelId;
use crate::session::CreateSessionRequest;
use crate::session::EventId;
use crate::session::JournalEvent;
use crate::session::LeaseToken;
use crate::session::ReleaseLeaseRequest;
use crate::session::SessionId;
use crate::session::SessionRecord;

const CONTROL_SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlRequest {
    schema_version: u64,
    #[serde(flatten)]
    command: ControlCommand,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "kebab-case")]
enum ControlCommand {
    Ping,
    Shutdown,
    SessionCreate {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
        channel_id: ChannelId,
    },
    SessionList,
    SessionShow {
        session_id: SessionId,
    },
    LeaseAcquire {
        session_id: SessionId,
        channel_id: ChannelId,
        steal: bool,
    },
    LeaseRelease {
        session_id: SessionId,
        lease_token: LeaseToken,
    },
    EventList {
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlResponse {
    schema_version: u64,
    ok: bool,
    command: String,
    message: String,
    daemon: ControlDaemonInfo,
    shutting_down: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<SessionRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sessions: Option<Vec<SessionRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_id: Option<EventId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    events: Option<Vec<JournalEvent>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlDaemonInfo {
    pid: u32,
    socket: String,
    started_at_unix: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlDaemonStartReport {
    schema_version: u64,
    state: String,
    daemon: ControlDaemonInfo,
}

impl ControlRequest {
    fn new(command: ControlCommand) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            command,
        }
    }
}

impl ControlCommand {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Shutdown => "shutdown",
            Self::SessionCreate { .. } => "session-create",
            Self::SessionList => "session-list",
            Self::SessionShow { .. } => "session-show",
            Self::LeaseAcquire { .. } => "lease-acquire",
            Self::LeaseRelease { .. } => "lease-release",
            Self::EventList { .. } => "event-list",
        }
    }
}

impl ControlResponse {
    fn ok(command: ControlCommand, message: impl Into<String>, daemon: &ControlDaemonInfo) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            ok: true,
            command: command.as_str().to_string(),
            message: message.into(),
            daemon: daemon.clone(),
            shutting_down: command == ControlCommand::Shutdown,
            session: None,
            sessions: None,
            event_id: None,
            events: None,
        }
    }

    fn error(
        command: impl Into<String>,
        message: impl Into<String>,
        daemon: &ControlDaemonInfo,
    ) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            ok: false,
            command: command.into(),
            message: message.into(),
            daemon: daemon.clone(),
            shutting_down: false,
            session: None,
            sessions: None,
            event_id: None,
            events: None,
        }
    }

    fn session_created(
        daemon: &ControlDaemonInfo,
        session: SessionRecord,
        event: JournalEvent,
    ) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            ok: true,
            command: ControlCommand::SessionCreate {
                session_id: Some(session.session_id.clone()),
                channel_id: session.primary_channel_id.clone(),
            }
            .as_str()
            .to_string(),
            message: String::from("session created"),
            daemon: daemon.clone(),
            shutting_down: false,
            session: Some(session),
            sessions: None,
            event_id: Some(event.event_id),
            events: None,
        }
    }

    fn session_list(daemon: &ControlDaemonInfo, sessions: Vec<SessionRecord>) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            ok: true,
            command: ControlCommand::SessionList.as_str().to_string(),
            message: String::from("sessions listed"),
            daemon: daemon.clone(),
            shutting_down: false,
            session: None,
            sessions: Some(sessions),
            event_id: None,
            events: None,
        }
    }

    fn session_show(daemon: &ControlDaemonInfo, session: SessionRecord) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            ok: true,
            command: ControlCommand::SessionShow {
                session_id: session.session_id.clone(),
            }
            .as_str()
            .to_string(),
            message: String::from("session found"),
            daemon: daemon.clone(),
            shutting_down: false,
            session: Some(session),
            sessions: None,
            event_id: None,
            events: None,
        }
    }

    fn lease_changed(
        daemon: &ControlDaemonInfo,
        command: &'static str,
        message: &'static str,
        session: SessionRecord,
        event: JournalEvent,
    ) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            ok: true,
            command: command.to_string(),
            message: message.to_string(),
            daemon: daemon.clone(),
            shutting_down: false,
            session: Some(session),
            sessions: None,
            event_id: Some(event.event_id),
            events: None,
        }
    }

    fn event_list(daemon: &ControlDaemonInfo, events: Vec<JournalEvent>) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            ok: true,
            command: ControlCommand::EventList { session_id: None }
                .as_str()
                .to_string(),
            message: String::from("events listed"),
            daemon: daemon.clone(),
            shutting_down: false,
            session: None,
            sessions: None,
            event_id: None,
            events: Some(events),
        }
    }
}

#[cfg(unix)]
pub fn daemon(args: ServeDaemonArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.manager_dir)?;
    let socket = paths.serve_control_socket();
    let listener = bind_listener(&paths)?;
    let daemon = ControlDaemonInfo {
        pid: std::process::id(),
        socket: socket.display().to_string(),
        started_at_unix: unix_now()?,
    };
    print_daemon_start(&daemon, args.json)?;

    let mut should_shutdown = false;
    while !should_shutdown {
        let (stream, _) = listener
            .accept()
            .with_context(|| format!("accept {}", socket.display()))?;
        match handle_stream(stream, &daemon, &paths) {
            Ok(next_should_shutdown) => should_shutdown = next_should_shutdown,
            Err(err) => eprintln!("cx serve daemon: {err:#}"),
        }
    }

    remove_socket_if_current(&socket)?;
    Ok(())
}

#[cfg(not(unix))]
pub fn daemon(_args: ServeDaemonArgs) -> Result<()> {
    anyhow::bail!("cx serve daemon is only supported on Unix platforms");
}

pub fn ping(args: ServePingArgs) -> Result<()> {
    if args.timeout <= 0.0 {
        anyhow::bail!("--timeout must be positive");
    }

    let paths = ManagerPaths::new(args.manager_dir)?;
    let response = send_request(&paths, ControlCommand::Ping, args.timeout)?;
    print_response(&response, args.json)?;
    if !response.ok {
        anyhow::bail!("{}", response.message);
    }
    Ok(())
}

pub fn shutdown(args: ServeShutdownArgs) -> Result<()> {
    if args.timeout <= 0.0 {
        anyhow::bail!("--timeout must be positive");
    }

    let paths = ManagerPaths::new(args.manager_dir)?;
    let response = send_request(&paths, ControlCommand::Shutdown, args.timeout)?;
    print_response(&response, args.json)?;
    if !response.ok {
        anyhow::bail!("{}", response.message);
    }
    Ok(())
}

pub fn session(args: ServeSessionArgs) -> Result<()> {
    match args.command {
        ServeSessionCommand::Create(args) => {
            if args.timeout <= 0.0 {
                anyhow::bail!("--timeout must be positive");
            }
            let paths = ManagerPaths::new(args.manager_dir)?;
            let session_id = args
                .id
                .map(SessionId::parse)
                .transpose()
                .context("invalid --id")?;
            let channel_id = ChannelId::parse(args.channel).context("invalid --channel")?;
            let response = send_request(
                &paths,
                ControlCommand::SessionCreate {
                    session_id,
                    channel_id,
                },
                args.timeout,
            )?;
            print_response(&response, args.json)?;
            if !response.ok {
                anyhow::bail!("{}", response.message);
            }
        }
        ServeSessionCommand::List(args) => {
            if args.timeout <= 0.0 {
                anyhow::bail!("--timeout must be positive");
            }
            let paths = ManagerPaths::new(args.manager_dir)?;
            let response = send_request(&paths, ControlCommand::SessionList, args.timeout)?;
            print_response(&response, args.json)?;
            if !response.ok {
                anyhow::bail!("{}", response.message);
            }
        }
        ServeSessionCommand::Show(args) => {
            if args.timeout <= 0.0 {
                anyhow::bail!("--timeout must be positive");
            }
            let paths = ManagerPaths::new(args.manager_dir)?;
            let session_id = SessionId::parse(args.session_id).context("invalid session id")?;
            let response = send_request(
                &paths,
                ControlCommand::SessionShow { session_id },
                args.timeout,
            )?;
            print_response(&response, args.json)?;
            if !response.ok {
                anyhow::bail!("{}", response.message);
            }
        }
    }
    Ok(())
}

pub fn lease(args: ServeLeaseArgs) -> Result<()> {
    match args.command {
        ServeLeaseCommand::Acquire(args) => {
            if args.timeout <= 0.0 {
                anyhow::bail!("--timeout must be positive");
            }
            let paths = ManagerPaths::new(args.manager_dir)?;
            let session_id = SessionId::parse(args.session).context("invalid --session")?;
            let channel_id = ChannelId::parse(args.channel).context("invalid --channel")?;
            let response = send_request(
                &paths,
                ControlCommand::LeaseAcquire {
                    session_id,
                    channel_id,
                    steal: args.steal,
                },
                args.timeout,
            )?;
            print_response(&response, args.json)?;
            if !response.ok {
                anyhow::bail!("{}", response.message);
            }
        }
        ServeLeaseCommand::Release(args) => {
            if args.timeout <= 0.0 {
                anyhow::bail!("--timeout must be positive");
            }
            let paths = ManagerPaths::new(args.manager_dir)?;
            let session_id = SessionId::parse(args.session).context("invalid --session")?;
            let lease_token = LeaseToken::parse(args.token).context("invalid --token")?;
            let response = send_request(
                &paths,
                ControlCommand::LeaseRelease {
                    session_id,
                    lease_token,
                },
                args.timeout,
            )?;
            print_response(&response, args.json)?;
            if !response.ok {
                anyhow::bail!("{}", response.message);
            }
        }
    }
    Ok(())
}

pub fn event(args: ServeEventArgs) -> Result<()> {
    match args.command {
        ServeEventCommand::List(args) => {
            if args.timeout <= 0.0 {
                anyhow::bail!("--timeout must be positive");
            }
            let paths = ManagerPaths::new(args.manager_dir)?;
            let session_id = args
                .session
                .map(SessionId::parse)
                .transpose()
                .context("invalid --session")?;
            let response = send_request(
                &paths,
                ControlCommand::EventList { session_id },
                args.timeout,
            )?;
            print_response(&response, args.json)?;
            if !response.ok {
                anyhow::bail!("{}", response.message);
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn bind_listener(paths: &ManagerPaths) -> Result<std::os::unix::net::UnixListener> {
    use std::os::unix::net::UnixListener;
    use std::os::unix::net::UnixStream;

    fs::create_dir_all(paths.serve_dir())
        .with_context(|| format!("create {}", paths.serve_dir().display()))?;
    set_private_dir_permissions(&paths.serve_dir())?;

    let socket = paths.serve_control_socket();
    if socket.exists() {
        if UnixStream::connect(&socket).is_ok() {
            anyhow::bail!("cx serve daemon already running: {}", socket.display());
        }
        fs::remove_file(&socket).with_context(|| format!("remove stale {}", socket.display()))?;
    }

    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    set_private_file_permissions(&socket)?;
    Ok(listener)
}

#[cfg(unix)]
fn handle_stream(
    mut stream: std::os::unix::net::UnixStream,
    daemon: &ControlDaemonInfo,
    paths: &ManagerPaths,
) -> Result<bool> {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        reader
            .read_line(&mut line)
            .context("read control request")?;
    }
    if line.is_empty() {
        return Ok(false);
    }

    let (response, should_shutdown) = match serde_json::from_str::<ControlRequest>(&line) {
        Ok(request) => handle_request(request, daemon, paths),
        Err(err) => (
            ControlResponse::error("unknown", format!("invalid control request: {err}"), daemon),
            false,
        ),
    };
    if let Err(err) = write_response(&mut stream, &response) {
        eprintln!("cx serve daemon: {err:#}");
        return Ok(false);
    }
    Ok(should_shutdown)
}

fn write_response(mut writer: impl Write, response: &ControlResponse) -> Result<()> {
    serde_json::to_writer(&mut writer, response).context("write control response")?;
    writer
        .write_all(b"\n")
        .context("write control response newline")?;
    writer.flush().context("flush control response")?;
    Ok(())
}

fn handle_request(
    request: ControlRequest,
    daemon: &ControlDaemonInfo,
    paths: &ManagerPaths,
) -> (ControlResponse, bool) {
    if request.schema_version != CONTROL_SCHEMA_VERSION {
        return (
            ControlResponse::error(
                request.command.as_str(),
                format!(
                    "unsupported control schema version: {}",
                    request.schema_version
                ),
                daemon,
            ),
            false,
        );
    }

    match request.command {
        ControlCommand::Ping => (
            ControlResponse::ok(ControlCommand::Ping, "ready", daemon),
            false,
        ),
        ControlCommand::Shutdown => (
            ControlResponse::ok(ControlCommand::Shutdown, "shutting down", daemon),
            true,
        ),
        ControlCommand::SessionCreate {
            session_id,
            channel_id,
        } => match session::create_session(
            paths,
            CreateSessionRequest {
                session_id,
                channel_id,
            },
        ) {
            Ok(result) => (
                ControlResponse::session_created(daemon, result.session, result.event),
                false,
            ),
            Err(err) => (
                ControlResponse::error("session-create", format!("{err:#}"), daemon),
                false,
            ),
        },
        ControlCommand::SessionList => match session::list_sessions(paths) {
            Ok(sessions) => (ControlResponse::session_list(daemon, sessions), false),
            Err(err) => (
                ControlResponse::error("session-list", format!("{err:#}"), daemon),
                false,
            ),
        },
        ControlCommand::SessionShow { session_id } => {
            match session::show_session(paths, &session_id) {
                Ok(session) => (ControlResponse::session_show(daemon, session), false),
                Err(err) => (
                    ControlResponse::error("session-show", format!("{err:#}"), daemon),
                    false,
                ),
            }
        }
        ControlCommand::LeaseAcquire {
            session_id,
            channel_id,
            steal,
        } => match session::acquire_lease(
            paths,
            AcquireLeaseRequest {
                session_id,
                channel_id,
                steal,
            },
        ) {
            Ok(result) => (
                ControlResponse::lease_changed(
                    daemon,
                    "lease-acquire",
                    "lease acquired",
                    result.session,
                    result.event,
                ),
                false,
            ),
            Err(err) => (
                ControlResponse::error("lease-acquire", format!("{err:#}"), daemon),
                false,
            ),
        },
        ControlCommand::LeaseRelease {
            session_id,
            lease_token,
        } => match session::release_lease(
            paths,
            ReleaseLeaseRequest {
                session_id,
                lease_token,
            },
        ) {
            Ok(result) => (
                ControlResponse::lease_changed(
                    daemon,
                    "lease-release",
                    "lease released",
                    result.session,
                    result.event,
                ),
                false,
            ),
            Err(err) => (
                ControlResponse::error("lease-release", format!("{err:#}"), daemon),
                false,
            ),
        },
        ControlCommand::EventList { session_id } => {
            match session::list_events(paths, session_id.as_ref()) {
                Ok(events) => (ControlResponse::event_list(daemon, events), false),
                Err(err) => (
                    ControlResponse::error("event-list", format!("{err:#}"), daemon),
                    false,
                ),
            }
        }
    }
}

#[cfg(unix)]
fn send_request(
    paths: &ManagerPaths,
    command: ControlCommand,
    timeout_secs: f32,
) -> Result<ControlResponse> {
    use std::os::unix::net::UnixStream;

    let socket = paths.serve_control_socket();
    let mut stream =
        UnixStream::connect(&socket).with_context(|| format!("connect {}", socket.display()))?;
    let timeout = Duration::from_secs_f32(timeout_secs);
    stream
        .set_read_timeout(Some(timeout))
        .context("set control read timeout")?;
    stream
        .set_write_timeout(Some(timeout))
        .context("set control write timeout")?;

    let request = ControlRequest::new(command);
    serde_json::to_writer(&mut stream, &request).context("write control request")?;
    stream
        .write_all(b"\n")
        .context("write control request newline")?;
    stream.flush().context("flush control request")?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader
        .read_line(&mut line)
        .context("read control response")?;
    if line.is_empty() {
        anyhow::bail!("empty control response from {}", socket.display());
    }
    serde_json::from_str::<ControlResponse>(&line).context("parse control response")
}

#[cfg(not(unix))]
fn send_request(
    _paths: &ManagerPaths,
    _command: ControlCommand,
    _timeout_secs: f32,
) -> Result<ControlResponse> {
    anyhow::bail!("cx serve control socket is only supported on Unix platforms");
}

fn print_daemon_start(daemon: &ControlDaemonInfo, json: bool) -> Result<()> {
    if json {
        let report = ControlDaemonStartReport {
            schema_version: CONTROL_SCHEMA_VERSION,
            state: String::from("ready"),
            daemon: daemon.clone(),
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("cx serve daemon: ready");
    println!("pid: {}", daemon.pid);
    println!("socket: {}", daemon.socket);
    println!("started_at_unix: {}", daemon.started_at_unix);
    Ok(())
}

fn print_response(response: &ControlResponse, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(response)?);
        return Ok(());
    }
    println!("cx serve daemon: {}", response.message);
    println!("pid: {}", response.daemon.pid);
    println!("socket: {}", response.daemon.socket);
    if let Some(session) = &response.session {
        println!("session: {}", session.session_id);
        println!("primary_channel: {}", session.primary_channel_id);
        println!("current_channel: {}", session.current_channel_id);
        println!("status: {:?}", session.status);
        println!("lease_epoch: {}", session.lease_epoch);
        if let Some(lease) = &session.active_lease {
            println!("active_lease_channel: {}", lease.channel_id);
            println!("active_lease_epoch: {}", lease.epoch);
            println!("active_lease_token: {}", lease.lease_token);
        }
        println!("created_at_unix: {}", session.created_at_unix);
    }
    if let Some(event_id) = &response.event_id {
        println!("event: {event_id}");
    }
    if let Some(sessions) = &response.sessions {
        if sessions.is_empty() {
            println!("sessions: 0");
        } else {
            for session in sessions {
                println!(
                    "{}\t{}\t{:?}\t{}",
                    session.session_id,
                    session.current_channel_id,
                    session.status,
                    session.created_at_unix
                );
            }
        }
    }
    if let Some(events) = &response.events {
        if events.is_empty() {
            println!("events: 0");
        } else {
            for event in events {
                println!(
                    "{}\t{:?}\t{}\t{}\t{}",
                    event.event_id,
                    event.event_kind,
                    event.session_id,
                    event.channel_id,
                    event.occurred_at_unix
                );
            }
        }
    }
    Ok(())
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?
        .as_secs())
}

#[cfg(unix)]
fn remove_socket_if_current(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

fn set_private_dir_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", path.display()))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use super::*;

    fn temp_paths(name: &str) -> ManagerPaths {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = Path::new("/tmp").join(format!("cxctl-{name}-{}-{unique}", std::process::id()));
        ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
    }

    fn daemon_info() -> ControlDaemonInfo {
        ControlDaemonInfo {
            pid: 123,
            socket: String::from("/tmp/cx-control.sock"),
            started_at_unix: 1_800_000_000,
        }
    }

    #[test]
    fn request_serializes_kebab_case_command() {
        let request = ControlRequest::new(ControlCommand::Ping);

        let content = serde_json::to_string(&request).unwrap();

        assert!(content.contains("\"command\":\"ping\""));
        let parsed = serde_json::from_str::<ControlRequest>(&content).unwrap();
        assert_eq!(parsed.command, ControlCommand::Ping);
    }

    #[test]
    fn shutdown_request_marks_response() {
        let request = ControlRequest::new(ControlCommand::Shutdown);
        let paths = temp_paths("shutdown");

        let (response, should_shutdown) = handle_request(request, &daemon_info(), &paths);

        assert!(response.ok);
        assert_eq!(response.command, "shutdown");
        assert!(response.shutting_down);
        assert!(should_shutdown);
    }

    #[test]
    fn schema_mismatch_is_error_without_shutdown() {
        let mut request = ControlRequest::new(ControlCommand::Shutdown);
        request.schema_version = CONTROL_SCHEMA_VERSION + 1;
        let paths = temp_paths("schema");

        let (response, should_shutdown) = handle_request(request, &daemon_info(), &paths);

        assert!(!response.ok);
        assert!(!response.shutting_down);
        assert!(!should_shutdown);
    }

    #[cfg(unix)]
    #[test]
    fn bind_listener_replaces_stale_socket_file() {
        use std::os::unix::fs::FileTypeExt;
        use std::os::unix::fs::PermissionsExt;

        let paths = temp_paths("stale-socket");
        fs::create_dir_all(paths.serve_dir()).unwrap();
        fs::write(paths.serve_control_socket(), b"stale").unwrap();

        let listener = bind_listener(&paths).unwrap();

        let metadata = fs::metadata(paths.serve_control_socket()).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

        drop(listener);
        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[cfg(unix)]
    #[test]
    fn empty_client_connection_is_noop() {
        use std::os::unix::net::UnixStream;

        let (server, client) = UnixStream::pair().unwrap();
        drop(client);

        let paths = temp_paths("empty");

        let should_shutdown = handle_stream(server, &daemon_info(), &paths).unwrap();

        assert!(!should_shutdown);
    }

    #[test]
    fn session_create_request_writes_registry() {
        let paths = temp_paths("session-create");
        let request = ControlRequest::new(ControlCommand::SessionCreate {
            session_id: Some(SessionId::parse("sess_manual").unwrap()),
            channel_id: ChannelId::parse("terminal").unwrap(),
        });

        let (response, should_shutdown) = handle_request(request, &daemon_info(), &paths);

        assert!(response.ok);
        assert!(!should_shutdown);
        assert_eq!(
            response.session.unwrap().session_id,
            SessionId::parse("sess_manual").unwrap()
        );
        assert!(paths.serve_session_file("sess_manual").exists());
        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn lease_acquire_request_sets_active_lease() {
        let paths = temp_paths("lease-acquire");
        let create = ControlRequest::new(ControlCommand::SessionCreate {
            session_id: Some(SessionId::parse("sess_manual").unwrap()),
            channel_id: ChannelId::parse("terminal").unwrap(),
        });
        let _ = handle_request(create, &daemon_info(), &paths);
        let acquire = ControlRequest::new(ControlCommand::LeaseAcquire {
            session_id: SessionId::parse("sess_manual").unwrap(),
            channel_id: ChannelId::parse("telegram:12345").unwrap(),
            steal: false,
        });

        let (response, should_shutdown) = handle_request(acquire, &daemon_info(), &paths);

        assert!(response.ok);
        assert!(!should_shutdown);
        let session = response.session.unwrap();
        assert_eq!(session.lease_epoch, 1);
        assert!(session.active_lease.is_some());
        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn event_list_request_returns_journal_events() {
        let paths = temp_paths("event-list");
        let create = ControlRequest::new(ControlCommand::SessionCreate {
            session_id: Some(SessionId::parse("sess_manual").unwrap()),
            channel_id: ChannelId::parse("terminal").unwrap(),
        });
        let _ = handle_request(create, &daemon_info(), &paths);
        let list = ControlRequest::new(ControlCommand::EventList {
            session_id: Some(SessionId::parse("sess_manual").unwrap()),
        });

        let (response, should_shutdown) = handle_request(list, &daemon_info(), &paths);

        assert!(response.ok);
        assert!(!should_shutdown);
        assert_eq!(response.events.unwrap().len(), 1);
        let _ = fs::remove_dir_all(&paths.manager_dir);
    }
}
