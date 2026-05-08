use std::fs;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use crate::cli::ManagedResumeArgs;
use crate::cli::ManagedRotateArgs;
use crate::cli::ManagedStatusArgs;
use crate::paths::ManagerPaths;

pub(crate) const CONTROL_SCHEMA_VERSION: u32 = 1;
pub(crate) const MANAGED_STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManagedStateFile {
    pub(crate) schema_version: u32,
    pub(crate) pid: u32,
    pub(crate) child_pid: Option<u32>,
    pub(crate) slot: String,
    pub(crate) target: Option<String>,
    pub(crate) cwd: String,
    pub(crate) session_id: Option<String>,
    pub(crate) started_at_unix: u64,
    pub(crate) child_started_at_unix: Option<u64>,
    pub(crate) socket: String,
    pub(crate) command: Vec<String>,
    pub(crate) last_event: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlRequest {
    pub(crate) schema_version: u32,
    pub(crate) command: ControlCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum ControlCommand {
    Status,
    Rotate {
        slot: Option<String>,
        continue_after: bool,
    },
    Resume {
        session_id: String,
        slot: Option<String>,
        continue_after: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlResponse {
    pub(crate) schema_version: u32,
    pub(crate) ok: bool,
    pub(crate) command: String,
    pub(crate) message: String,
    pub(crate) supervisor: Option<ManagedStateFile>,
}

#[derive(Debug)]
pub(crate) struct ControlEnvelope {
    pub(crate) request: ControlRequest,
    pub(crate) reply: mpsc::Sender<ControlResponse>,
}

impl ControlRequest {
    pub(crate) fn new(command: ControlCommand) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            command,
        }
    }
}

impl ControlCommand {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Rotate { .. } => "rotate",
            Self::Resume { .. } => "resume",
        }
    }
}

impl ControlResponse {
    pub(crate) fn ok(
        command: &ControlCommand,
        message: impl Into<String>,
        supervisor: ManagedStateFile,
    ) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            ok: true,
            command: command.as_str().to_string(),
            message: message.into(),
            supervisor: Some(supervisor),
        }
    }

    pub(crate) fn error(
        command: impl Into<String>,
        message: impl Into<String>,
        supervisor: Option<ManagedStateFile>,
    ) -> Self {
        Self {
            schema_version: CONTROL_SCHEMA_VERSION,
            ok: false,
            command: command.into(),
            message: message.into(),
            supervisor,
        }
    }
}

pub(crate) fn status(args: ManagedStatusArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.manager_dir)?;
    let states = active_supervisors(&paths, args.all)?;
    print_status(&states, args.json)?;
    Ok(())
}

pub(crate) fn rotate(args: ManagedRotateArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.manager_dir)?;
    let state = select_current_supervisor(&paths)?;
    let response = send_request_to_state(
        &state,
        ControlCommand::Rotate {
            slot: args.slot,
            continue_after: args.continue_after,
        },
        args.timeout,
    )?;
    print_response(&response, args.json)?;
    if !response.ok {
        anyhow::bail!("{}", response.message);
    }
    Ok(())
}

pub(crate) fn resume(args: ManagedResumeArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.manager_dir)?;
    let state = select_current_supervisor(&paths)?;
    let response = send_request_to_state(
        &state,
        ControlCommand::Resume {
            session_id: args.session_id,
            slot: args.slot,
            continue_after: args.continue_after,
        },
        args.timeout,
    )?;
    print_response(&response, args.json)?;
    if !response.ok {
        anyhow::bail!("{}", response.message);
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn start_listener(
    paths: &ManagerPaths,
    pid: u32,
    tx: mpsc::Sender<ControlEnvelope>,
) -> Result<std::path::PathBuf> {
    use std::os::unix::net::UnixListener;

    fs::create_dir_all(paths.serve_dir())
        .with_context(|| format!("create {}", paths.serve_dir().display()))?;
    fs::create_dir_all(paths.managed_supervisors_dir())
        .with_context(|| format!("create {}", paths.managed_supervisors_dir().display()))?;
    set_private_dir_permissions(&paths.serve_dir())?;
    set_private_dir_permissions(&paths.managed_supervisors_dir())?;

    let socket = paths.managed_control_socket(pid);
    remove_file_if_exists(&socket)?;
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    set_private_file_permissions(&socket)?;

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                break;
            };
            if let Err(err) = handle_stream(stream, &tx) {
                eprintln!("cx managed control: {err:#}");
            }
        }
    });

    Ok(socket)
}

#[cfg(not(unix))]
pub(crate) fn start_listener(
    _paths: &ManagerPaths,
    _pid: u32,
    _tx: mpsc::Sender<ControlEnvelope>,
) -> Result<std::path::PathBuf> {
    anyhow::bail!("cx managed control socket is only supported on Unix platforms");
}

pub(crate) fn write_state(paths: &ManagerPaths, state: &ManagedStateFile) -> Result<()> {
    fs::create_dir_all(paths.serve_dir())
        .with_context(|| format!("create {}", paths.serve_dir().display()))?;
    fs::create_dir_all(paths.managed_supervisors_dir())
        .with_context(|| format!("create {}", paths.managed_supervisors_dir().display()))?;
    set_private_dir_permissions(&paths.serve_dir())?;
    set_private_dir_permissions(&paths.managed_supervisors_dir())?;

    let state_file = paths.managed_state_file(state.pid);
    let tmp_file = paths
        .managed_supervisors_dir()
        .join(format!("{}.json.tmp", state.pid));
    let content = serde_json::to_vec_pretty(state).context("serialize managed state")?;
    let mut file = private_open_for_write(&tmp_file)?;
    set_private_file_permissions(&tmp_file)?;
    file.write_all(&content)
        .with_context(|| format!("write {}", tmp_file.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("write {}", tmp_file.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", tmp_file.display()))?;
    fs::rename(&tmp_file, &state_file)
        .with_context(|| format!("rename {} to {}", tmp_file.display(), state_file.display()))?;
    Ok(())
}

pub(crate) fn remove_state(paths: &ManagerPaths, pid: u32) -> Result<()> {
    remove_file_if_exists(&paths.managed_state_file(pid))?;
    remove_file_if_exists(&paths.managed_control_socket(pid))?;
    Ok(())
}

#[cfg(unix)]
fn handle_stream(
    mut stream: std::os::unix::net::UnixStream,
    tx: &mpsc::Sender<ControlEnvelope>,
) -> Result<()> {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        reader
            .read_line(&mut line)
            .context("read managed control request")?;
    }
    if line.is_empty() {
        return Ok(());
    }

    let request = match serde_json::from_str::<ControlRequest>(&line) {
        Ok(request) => request,
        Err(err) => {
            let response = ControlResponse::error(
                "unknown",
                format!("invalid managed control request: {err}"),
                None,
            );
            return write_response(&mut stream, &response);
        }
    };
    let command = request.command.as_str().to_string();
    let (reply, response_rx) = mpsc::channel();
    tx.send(ControlEnvelope { request, reply })
        .context("send managed control request to supervisor")?;
    let response = response_rx
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| ControlResponse::error(command, "managed supervisor timed out", None));
    write_response(&mut stream, &response)
}

fn write_response(mut writer: impl Write, response: &ControlResponse) -> Result<()> {
    serde_json::to_writer(&mut writer, response).context("write managed control response")?;
    writer
        .write_all(b"\n")
        .context("write managed control response newline")?;
    writer.flush().context("flush managed control response")?;
    Ok(())
}

#[cfg(unix)]
fn send_request_to_state(
    state: &ManagedStateFile,
    command: ControlCommand,
    timeout_secs: f32,
) -> Result<ControlResponse> {
    use std::os::unix::net::UnixStream;

    let mut stream =
        UnixStream::connect(&state.socket).with_context(|| format!("connect {}", state.socket))?;
    let timeout = Duration::from_secs_f32(timeout_secs.max(0.1));
    stream
        .set_read_timeout(Some(timeout))
        .context("set managed control read timeout")?;
    stream
        .set_write_timeout(Some(timeout))
        .context("set managed control write timeout")?;

    let request = ControlRequest::new(command);
    serde_json::to_writer(&mut stream, &request).context("write managed control request")?;
    stream
        .write_all(b"\n")
        .context("write managed control request newline")?;
    stream.flush().context("flush managed control request")?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader
        .read_line(&mut line)
        .context("read managed control response")?;
    if line.is_empty() {
        anyhow::bail!("empty managed control response from {}", state.socket);
    }
    serde_json::from_str::<ControlResponse>(&line).context("parse managed control response")
}

#[cfg(not(unix))]
fn send_request_to_state(
    _state: &ManagedStateFile,
    _command: ControlCommand,
    _timeout_secs: f32,
) -> Result<ControlResponse> {
    anyhow::bail!("cx managed control socket is only supported on Unix platforms");
}

fn active_supervisors(paths: &ManagerPaths, all: bool) -> Result<Vec<ManagedStateFile>> {
    let mut states = Vec::new();
    let supervisors_dir = paths.managed_supervisors_dir();
    if !supervisors_dir.is_dir() {
        return Ok(states);
    }
    let cwd = current_cwd_string()?;
    for entry in fs::read_dir(&supervisors_dir)
        .with_context(|| format!("read {}", supervisors_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(state) = read_state_if_exists(&path)? else {
            continue;
        };
        if state.schema_version != MANAGED_STATE_SCHEMA_VERSION {
            continue;
        }
        if !process_exists(state.pid) {
            continue;
        }
        if all || state.cwd == cwd {
            states.push(state);
        }
    }
    states.sort_by_key(|state| state.started_at_unix);
    Ok(states)
}

fn select_current_supervisor(paths: &ManagerPaths) -> Result<ManagedStateFile> {
    active_supervisors(paths, false)?
        .into_iter()
        .last()
        .context("no running cx managed supervisor for current working directory")
}

fn read_state_if_exists(path: &Path) -> Result<Option<ManagedStateFile>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let state = serde_json::from_str::<ManagedStateFile>(&content)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(state))
}

fn print_status(states: &[ManagedStateFile], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(states)?);
        return Ok(());
    }
    if states.is_empty() {
        println!("cx managed: not running");
        return Ok(());
    }
    println!("cx managed: {}", states.len());
    for state in states {
        print_state(state);
    }
    Ok(())
}

fn print_response(response: &ControlResponse, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(response)?);
        return Ok(());
    }
    println!("cx managed {}: {}", response.command, response.message);
    if let Some(state) = &response.supervisor {
        print_state(state);
    }
    Ok(())
}

fn print_state(state: &ManagedStateFile) {
    println!("pid: {}", state.pid);
    if let Some(child_pid) = state.child_pid {
        println!("child_pid: {child_pid}");
    }
    println!("slot: {}", state.slot);
    if let Some(target) = &state.target {
        println!("target: {target}");
    }
    if let Some(session_id) = &state.session_id {
        println!("session: {session_id}");
    }
    println!("cwd: {}", state.cwd);
    println!("socket: {}", state.socket);
    if let Some(event) = &state.last_event {
        println!("last_event: {event}");
    }
}

fn current_cwd_string() -> Result<String> {
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    Ok(cwd.display().to_string())
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
}

fn private_open_for_write(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("open {}", path.display()))
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

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_request_serializes_kebab_case_command() {
        let request = ControlRequest::new(ControlCommand::Rotate {
            slot: Some(String::from("bus2")),
            continue_after: true,
        });

        let json = serde_json::to_string(&request).unwrap();

        assert!(json.contains("\"type\":\"rotate\""));
        assert!(json.contains("\"continueAfter\":true"));
    }

    #[test]
    fn state_file_uses_camel_case_fields() {
        let state = ManagedStateFile {
            schema_version: MANAGED_STATE_SCHEMA_VERSION,
            pid: 1,
            child_pid: Some(2),
            slot: String::from("bus1"),
            target: Some(String::from("work")),
            cwd: String::from("/tmp"),
            session_id: Some(String::from("sid")),
            started_at_unix: 10,
            child_started_at_unix: Some(11),
            socket: String::from("/tmp/cx.sock"),
            command: vec![String::from("codex")],
            last_event: None,
        };

        let json = serde_json::to_string(&state).unwrap();

        assert!(json.contains("\"schemaVersion\":1"));
        assert!(json.contains("\"childPid\":2"));
        assert!(json.contains("\"sessionId\":\"sid\""));
    }
}
