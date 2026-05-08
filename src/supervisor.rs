pub(crate) mod control;
pub(crate) mod rate_limit;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::io::IsTerminal;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use portable_pty::native_pty_system;
use portable_pty::ChildKiller;
use portable_pty::CommandBuilder;
use portable_pty::MasterPty;
use portable_pty::PtySize;

use crate::paths::ManagerPaths;
use crate::run;
use crate::run::CodexCommandSpec;
use crate::selector;
use crate::target::TargetSpec;
use crate::usage::SlotStatus;
use crate::usage::UsageChecker;

const OUTPUT_RING_LIMIT: usize = 8192;
const LOW_CONFIDENCE_COOLDOWN: Duration = Duration::from_secs(10 * 60);
const DEFAULT_CONFIRMED_COOLDOWN: Duration = Duration::from_secs(15 * 60);
const CONTINUE_PROMPT: &str = "继续";

#[derive(Debug, Clone)]
pub(crate) struct SupervisorLaunch {
    pub(crate) paths: ManagerPaths,
    pub(crate) real_codex: PathBuf,
    pub(crate) target: Option<TargetSpec>,
    pub(crate) candidates: Vec<String>,
    pub(crate) selected_slot: String,
    pub(crate) original_codex_args: Vec<OsString>,
    pub(crate) initial_spec: CodexCommandSpec,
    pub(crate) session_id: Option<String>,
    pub(crate) quiet: bool,
    pub(crate) debug: bool,
}

pub(crate) fn run(launch: SupervisorLaunch) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = launch;
        anyhow::bail!("cx --managed is only supported on Unix platforms");
    }

    #[cfg(unix)]
    run_unix(launch)
}

#[cfg(unix)]
fn run_unix(launch: SupervisorLaunch) -> Result<()> {
    let pid = std::process::id();
    let (tx, rx) = mpsc::channel();
    let (control_tx, control_rx) = mpsc::channel();
    let socket = control::start_listener(&launch.paths, pid, control_tx)?;
    bridge_control_events(control_rx, tx.clone());
    start_stdin_thread(tx.clone());
    start_signal_thread(tx.clone())?;

    let _raw_mode = RawModeGuard::enable();
    let initial_spec = launch.initial_spec.clone();
    let initial_session_known = launch.session_id.is_some();
    let mut supervisor = Supervisor::new(launch, socket, tx, rx)?;
    let result = supervisor
        .write_state()
        .and_then(|_| supervisor.start_child(initial_spec, initial_session_known))
        .and_then(|_| supervisor.event_loop());
    let _ = control::remove_state(&supervisor.paths, supervisor.state.pid);

    match result {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(err) => Err(err),
    }
}

#[derive(Debug)]
enum SupervisorEvent {
    Stdin(Vec<u8>),
    Output { generation: u64, bytes: Vec<u8> },
    ChildExit { generation: u64, code: i32 },
    Control(control::ControlEnvelope),
    Signal(SupervisorSignal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorSignal {
    Winch,
    Interrupt,
    Terminate,
}

struct ChildRuntime {
    pid: Option<u32>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

struct Supervisor {
    paths: ManagerPaths,
    real_codex: PathBuf,
    target: Option<TargetSpec>,
    candidates: Vec<String>,
    original_codex_args: Vec<OsString>,
    current_slot: String,
    session_id: Option<String>,
    quiet: bool,
    debug: bool,
    tx: mpsc::Sender<SupervisorEvent>,
    rx: mpsc::Receiver<SupervisorEvent>,
    state: control::ManagedStateFile,
    child: Option<ChildRuntime>,
    generation: u64,
    cooldowns: BTreeMap<String, Instant>,
    output_ring: VecDeque<u8>,
    output_signal_seen: bool,
}

impl Supervisor {
    fn new(
        launch: SupervisorLaunch,
        socket: PathBuf,
        tx: mpsc::Sender<SupervisorEvent>,
        rx: mpsc::Receiver<SupervisorEvent>,
    ) -> Result<Self> {
        let cwd = current_cwd_string()?;
        let started_at_unix = unix_now()?;
        let command = command_snapshot(&launch.initial_spec);
        let state = control::ManagedStateFile {
            schema_version: control::MANAGED_STATE_SCHEMA_VERSION,
            pid: std::process::id(),
            child_pid: None,
            slot: launch.selected_slot.clone(),
            target: launch.initial_spec.target_name.clone(),
            cwd,
            session_id: launch.session_id.clone(),
            started_at_unix,
            child_started_at_unix: None,
            socket: socket.display().to_string(),
            command,
            last_event: Some(String::from("starting")),
        };

        Ok(Self {
            paths: launch.paths,
            real_codex: launch.real_codex,
            target: launch.target,
            candidates: launch.candidates,
            original_codex_args: launch.original_codex_args,
            current_slot: launch.selected_slot,
            session_id: launch.session_id,
            quiet: launch.quiet,
            debug: launch.debug,
            tx,
            rx,
            state,
            child: None,
            generation: 0,
            cooldowns: BTreeMap::new(),
            output_ring: VecDeque::new(),
            output_signal_seen: false,
        })
    }

    fn event_loop(&mut self) -> Result<i32> {
        let mut last_tick = Instant::now();
        loop {
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => {
                    if let Some(exit_code) = self.handle_event(event)? {
                        return Ok(exit_code);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(1),
            }

            if last_tick.elapsed() >= Duration::from_secs(1) {
                last_tick = Instant::now();
                self.tick()?;
            }
        }
    }

    fn handle_event(&mut self, event: SupervisorEvent) -> Result<Option<i32>> {
        match event {
            SupervisorEvent::Stdin(bytes) => {
                self.forward_stdin(&bytes);
            }
            SupervisorEvent::Output { generation, bytes } => {
                if generation == self.generation {
                    self.handle_output(&bytes)?;
                }
            }
            SupervisorEvent::ChildExit { generation, code } => {
                if generation == self.generation {
                    return self.handle_child_exit(code);
                }
            }
            SupervisorEvent::Control(envelope) => {
                self.handle_control(envelope);
            }
            SupervisorEvent::Signal(signal) => {
                return self.handle_signal(signal);
            }
        }
        Ok(None)
    }

    fn handle_signal(&mut self, signal: SupervisorSignal) -> Result<Option<i32>> {
        match signal {
            SupervisorSignal::Winch => {
                self.resize_child();
                self.forward_signal(libc::SIGWINCH);
                Ok(None)
            }
            SupervisorSignal::Interrupt => {
                self.forward_signal(libc::SIGINT);
                Ok(None)
            }
            SupervisorSignal::Terminate => {
                self.notice("received SIGTERM; stopping child");
                self.stop_current_child();
                Ok(Some(143))
            }
        }
    }

    fn handle_child_exit(&mut self, code: i32) -> Result<Option<i32>> {
        self.child = None;
        self.state.child_pid = None;
        self.state.child_started_at_unix = None;
        self.state.last_event = Some(format!("child exited with {code}"));
        let _ = self.write_state();

        if self.output_signal_seen && self.handle_rate_limit_detected(false)? {
            return Ok(None);
        }

        Ok(Some(code))
    }

    fn handle_control(&mut self, envelope: control::ControlEnvelope) {
        let response = if envelope.request.schema_version != control::CONTROL_SCHEMA_VERSION {
            control::ControlResponse::error(
                envelope.request.command.as_str(),
                format!(
                    "unsupported managed control schema version: {}",
                    envelope.request.schema_version
                ),
                Some(self.state.clone()),
            )
        } else {
            match envelope.request.command {
                control::ControlCommand::Status => control::ControlResponse::ok(
                    &control::ControlCommand::Status,
                    "running",
                    self.state.clone(),
                ),
                control::ControlCommand::Rotate {
                    slot,
                    continue_after,
                } => match self.manual_rotate(slot, continue_after) {
                    Ok(()) => control::ControlResponse::ok(
                        &control::ControlCommand::Rotate {
                            slot: None,
                            continue_after,
                        },
                        "rotated",
                        self.state.clone(),
                    ),
                    Err(err) => control::ControlResponse::error(
                        "rotate",
                        format!("{err:#}"),
                        Some(self.state.clone()),
                    ),
                },
                control::ControlCommand::Resume {
                    session_id,
                    slot,
                    continue_after,
                } => match self.manual_resume(session_id, slot, continue_after) {
                    Ok(()) => control::ControlResponse::ok(
                        &control::ControlCommand::Resume {
                            session_id: String::new(),
                            slot: None,
                            continue_after,
                        },
                        "resumed",
                        self.state.clone(),
                    ),
                    Err(err) => control::ControlResponse::error(
                        "resume",
                        format!("{err:#}"),
                        Some(self.state.clone()),
                    ),
                },
            }
        };
        let _ = envelope.reply.send(response);
    }

    fn tick(&mut self) -> Result<()> {
        Ok(())
    }

    fn handle_output(&mut self, bytes: &[u8]) -> Result<()> {
        self.push_output_ring(bytes);
        if self.output_signal_seen {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&self.output_ring.iter().copied().collect::<Vec<_>>())
            .to_string();
        if rate_limit::classify_output(&text).is_some() {
            self.output_signal_seen = true;
            self.handle_rate_limit_detected(false)?;
        }
        Ok(())
    }

    fn handle_rate_limit_detected(&mut self, safe_to_continue: bool) -> Result<bool> {
        if self.session_id.is_none() {
            self.state.last_event = Some(String::from(
                "rate-limit signal ignored: session id unknown",
            ));
            let _ = self.write_state();
            self.notice("rate-limit signal seen, but session id is not known yet");
            return Ok(false);
        }

        let checker = UsageChecker::new(run::usage_timeout())?;
        let result = checker.query_slot(&self.paths, &self.current_slot, 0);
        let decision = RotationDecision::from_result(&result);
        match decision {
            RotationDecision::Confirmed { cooldown } => {
                self.cooldowns
                    .insert(self.current_slot.clone(), Instant::now() + cooldown);
                self.notice(&format!(
                    "rate limit confirmed on {}; rotating slot",
                    self.current_slot
                ));
                self.rotate_to_next_slot(safe_to_continue, "rate-limit confirmed")?;
                Ok(true)
            }
            RotationDecision::LowConfidence { cooldown } => {
                self.cooldowns
                    .insert(self.current_slot.clone(), Instant::now() + cooldown);
                self.notice(&format!(
                    "low-confidence provider rate limit on {}; rotating with short cooldown",
                    self.current_slot
                ));
                self.rotate_to_next_slot(safe_to_continue, "provider rate-limit low-confidence")?;
                Ok(true)
            }
            RotationDecision::NotConfirmed => {
                self.state.last_event = Some(format!(
                    "rate-limit signal not confirmed for {}: {}",
                    self.current_slot, result.summary
                ));
                let _ = self.write_state();
                self.notice("rate-limit signal was not confirmed by usage check");
                Ok(false)
            }
        }
    }

    fn manual_rotate(&mut self, slot: Option<String>, continue_after: bool) -> Result<()> {
        if self.session_id.is_none() {
            anyhow::bail!("session id is unknown; refusing rotate/resume");
        }
        let slot = self.select_slot(slot)?;
        self.rotate_to_slot(slot, continue_after, "manual rotate")
    }

    fn manual_resume(
        &mut self,
        session_id: String,
        slot: Option<String>,
        continue_after: bool,
    ) -> Result<()> {
        self.session_id = Some(session_id);
        let slot = match slot {
            Some(slot) => self.select_slot(Some(slot))?,
            None => self.current_slot.clone(),
        };
        self.rotate_to_slot(slot, continue_after, "manual resume")
    }

    fn rotate_to_next_slot(&mut self, continue_after: bool, reason: &str) -> Result<()> {
        let next_slot = self.select_slot(None)?;
        self.rotate_to_slot(next_slot, continue_after, reason)
    }

    fn rotate_to_slot(&mut self, slot: String, continue_after: bool, reason: &str) -> Result<()> {
        let session_id = self
            .session_id
            .clone()
            .context("session id is unknown; refusing rotate/resume")?;
        let codex_args =
            managed_resume_codex_args(&self.original_codex_args, &session_id, continue_after);
        let spec = run::build_slot_command_spec(
            &self.paths,
            self.real_codex.clone(),
            &slot,
            self.target.as_ref(),
            codex_args,
        )?;

        self.stop_current_child();
        self.current_slot = slot;
        self.state.slot = self.current_slot.clone();
        self.state.session_id = self.session_id.clone();
        self.state.last_event = Some(reason.to_string());
        self.start_child(spec, true)
    }

    fn select_slot(&mut self, explicit_slot: Option<String>) -> Result<String> {
        self.purge_expired_cooldowns();
        if let Some(slot) = explicit_slot {
            let slot_home = self.paths.slot_home(&slot);
            if !slot_home.is_dir() {
                anyhow::bail!("missing slot home for {slot}: {}", slot_home.display());
            }
            return Ok(slot);
        }

        let candidates = if self.candidates.is_empty() {
            crate::slot::load_rotation(&self.paths)?
        } else {
            self.candidates.clone()
        };
        if candidates.is_empty() {
            anyhow::bail!("no configured slots to rotate to");
        }
        let results = selector::query_slots(&self.paths, &candidates, run::usage_timeout())?;
        let mut excluded = self.cooldowns.keys().cloned().collect::<BTreeSet<String>>();
        excluded.insert(self.current_slot.clone());
        let selected = selector::choose_result_excluding(&results, &excluded);
        if self.debug || std::env::var_os("CX_SLOT_DEBUG").is_some() {
            crate::output::print_report(
                &results,
                selected.map(|result| result.slot.as_str()),
                false,
            )?;
        }
        selected
            .map(|result| result.slot.clone())
            .context("no usable Codex slot found outside current/cooldown slots")
    }

    fn purge_expired_cooldowns(&mut self) {
        let now = Instant::now();
        self.cooldowns.retain(|_, until| *until > now);
    }

    fn start_child(&mut self, spec: CodexCommandSpec, _known_session: bool) -> Result<()> {
        let generation = self.generation + 1;
        self.generation = generation;
        self.output_signal_seen = false;
        self.output_ring.clear();
        self.current_slot = spec.slot.clone();
        self.state.slot = spec.slot.clone();
        self.state.session_id = self.session_id.clone();

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(current_pty_size())?;
        let mut command = command_builder_from_spec(&spec)?;
        command.cwd(std::env::current_dir().context("resolve current directory")?);
        let mut child = pair.slave.spawn_command(command)?;
        let pid = child.process_id();
        let killer = child.clone_killer();

        let tx = self.tx.clone();
        let mut reader = pair.master.try_clone_reader()?;
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let bytes = buf[..n].to_vec();
                        let _ = io::stdout().write_all(&bytes);
                        let _ = io::stdout().flush();
                        let _ = tx.send(SupervisorEvent::Output { generation, bytes });
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        let tx = self.tx.clone();
        thread::spawn(move || {
            let code = match child.wait() {
                Ok(status) => status.exit_code() as i32,
                Err(_) => 1,
            };
            let _ = tx.send(SupervisorEvent::ChildExit { generation, code });
        });

        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        self.child = Some(ChildRuntime {
            pid,
            master: pair.master,
            writer,
            killer,
        });
        self.state.child_pid = pid;
        self.state.child_started_at_unix = Some(unix_now()?);
        self.state.last_event = Some(format!("child started on {}", self.current_slot));
        self.write_state()?;

        Ok(())
    }

    fn stop_current_child(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if let Some(pid) = child.pid {
            let _ = send_process_group_signal(pid, libc::SIGINT);
            thread::sleep(Duration::from_millis(250));
            let _ = send_process_group_signal(pid, libc::SIGTERM);
            thread::sleep(Duration::from_millis(500));
            let _ = send_process_group_signal(pid, libc::SIGKILL);
        }
        let _ = child.killer.kill();
        self.state.child_pid = None;
        self.state.child_started_at_unix = None;
        let _ = self.write_state();
    }

    fn forward_stdin(&mut self, bytes: &[u8]) {
        let Some(child) = &self.child else {
            return;
        };
        let Ok(mut writer) = child.writer.lock() else {
            return;
        };
        let _ = writer.write_all(bytes);
        let _ = writer.flush();
    }

    fn resize_child(&mut self) {
        let Some(child) = &mut self.child else {
            return;
        };
        let _ = child.master.resize(current_pty_size());
    }

    fn forward_signal(&self, signal: i32) {
        let Some(child) = &self.child else {
            return;
        };
        if let Some(pid) = child.pid {
            let _ = send_process_group_signal(pid, signal);
        }
    }

    fn push_output_ring(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if self.output_ring.len() == OUTPUT_RING_LIMIT {
                self.output_ring.pop_front();
            }
            self.output_ring.push_back(*byte);
        }
    }

    fn write_state(&self) -> Result<()> {
        control::write_state(&self.paths, &self.state)
    }

    fn notice(&self, message: &str) {
        if self.quiet {
            return;
        }
        let _ = writeln!(io::stderr(), "\r\ncx managed: {message}\r");
    }
}

#[derive(Debug, Clone, Copy)]
enum RotationDecision {
    Confirmed { cooldown: Duration },
    LowConfidence { cooldown: Duration },
    NotConfirmed,
}

impl RotationDecision {
    fn from_result(result: &crate::usage::SlotResult) -> Self {
        if result.status == SlotStatus::Exhausted {
            return Self::Confirmed {
                cooldown: cooldown_from_result(result),
            };
        }

        if matches!(
            result.status,
            SlotStatus::ApiKey | SlotStatus::ExternalProvider
        ) {
            return Self::LowConfidence {
                cooldown: LOW_CONFIDENCE_COOLDOWN,
            };
        }

        Self::NotConfirmed
    }
}

struct RawModeGuard {
    enabled: bool,
}

impl RawModeGuard {
    fn enable() -> Self {
        if !io::stdin().is_terminal() {
            return Self { enabled: false };
        }
        let enabled = crossterm::terminal::enable_raw_mode().is_ok();
        Self { enabled }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.enabled {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

fn command_builder_from_spec(spec: &CodexCommandSpec) -> Result<CommandBuilder> {
    let program = spec.program.to_string_lossy().to_string();
    if program.is_empty() {
        anyhow::bail!("missing codex program");
    }
    let mut command = CommandBuilder::new(program);
    command.env("CODEX_HOME", spec.codex_home.display().to_string());
    for (key, value) in &spec.envs {
        command.env(key, value);
    }
    for arg in &spec.args {
        command.arg(arg);
    }
    Ok(command)
}

pub(crate) fn managed_resume_codex_args(
    original_args: &[OsString],
    session_id: &str,
    continue_after: bool,
) -> Vec<OsString> {
    let mut args = codex_root_option_prefix(original_args);
    args.push(OsString::from("resume"));
    args.push(OsString::from(session_id));
    if continue_after {
        args.push(OsString::from(CONTINUE_PROMPT));
    }
    args
}

fn codex_root_option_prefix(original_args: &[OsString]) -> Vec<OsString> {
    let mut prefix = Vec::new();
    let mut index = 0;
    while index < original_args.len() {
        let arg = &original_args[index];
        let arg_text = arg.to_string_lossy();
        if arg_text == "--" {
            break;
        }
        if !arg_text.starts_with('-') {
            break;
        }
        match run::codex_option_kind(arg_text.as_ref()) {
            run::CodexOptionKind::Flag => {
                prefix.push(arg.clone());
                index += 1;
            }
            run::CodexOptionKind::Value => {
                let Some(value) = original_args.get(index + 1) else {
                    break;
                };
                prefix.push(arg.clone());
                prefix.push(value.clone());
                index += 2;
            }
            run::CodexOptionKind::Unknown => {
                break;
            }
        }
    }
    prefix
}

fn command_snapshot(spec: &CodexCommandSpec) -> Vec<String> {
    let mut command = vec![spec.program.display().to_string()];
    command.extend(
        spec.args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string()),
    );
    command
}

fn current_pty_size() -> PtySize {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn cooldown_from_result(result: &crate::usage::SlotResult) -> Duration {
    let now = unix_now().unwrap_or_default() as i64;
    let reset_at = result
        .reset_at
        .or(result.five_hour_refresh_at)
        .or(result.weekly_refresh_at);
    if let Some(reset_at) = reset_at.filter(|reset_at| *reset_at > now) {
        return Duration::from_secs((reset_at - now) as u64);
    }
    DEFAULT_CONFIRMED_COOLDOWN
}

fn start_stdin_thread(tx: mpsc::Sender<SupervisorEvent>) {
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(SupervisorEvent::Stdin(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}

fn bridge_control_events(
    control_rx: mpsc::Receiver<control::ControlEnvelope>,
    tx: mpsc::Sender<SupervisorEvent>,
) {
    thread::spawn(move || {
        while let Ok(envelope) = control_rx.recv() {
            if tx.send(SupervisorEvent::Control(envelope)).is_err() {
                break;
            }
        }
    });
}

#[cfg(unix)]
fn start_signal_thread(tx: mpsc::Sender<SupervisorEvent>) -> Result<()> {
    use signal_hook::consts::signal::SIGINT;
    use signal_hook::consts::signal::SIGTERM;
    use signal_hook::consts::signal::SIGWINCH;
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGWINCH, SIGINT, SIGTERM])?;
    thread::spawn(move || {
        for signal in signals.forever() {
            let event = match signal {
                SIGWINCH => SupervisorEvent::Signal(SupervisorSignal::Winch),
                SIGINT => SupervisorEvent::Signal(SupervisorSignal::Interrupt),
                SIGTERM => SupervisorEvent::Signal(SupervisorSignal::Terminate),
                _ => continue,
            };
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    Ok(())
}

#[cfg(unix)]
fn send_process_group_signal(pid: u32, signal: i32) -> io::Result<()> {
    let process_group = -(pid as i32);
    let result = unsafe { libc::kill(process_group, signal) };
    if result == 0 {
        return Ok(());
    }
    let group_error = io::Error::last_os_error();
    if group_error.raw_os_error() == Some(libc::ESRCH) {
        let direct_result = unsafe { libc::kill(pid as i32, signal) };
        if direct_result == 0 {
            return Ok(());
        }
    }
    Err(group_error)
}

fn current_cwd_string() -> Result<String> {
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    Ok(cwd.display().to_string())
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_resume_args_preserve_root_flags_and_add_continue_prompt() {
        let args = managed_resume_codex_args(
            &[
                OsString::from("-m"),
                OsString::from("gpt-5.5"),
                OsString::from("--search"),
                OsString::from("write code"),
            ],
            "sid",
            true,
        );

        assert_eq!(
            args,
            vec![
                OsString::from("-m"),
                OsString::from("gpt-5.5"),
                OsString::from("--search"),
                OsString::from("resume"),
                OsString::from("sid"),
                OsString::from(CONTINUE_PROMPT),
            ]
        );
    }

    #[test]
    fn managed_resume_args_replace_existing_resume_invocation() {
        let args = managed_resume_codex_args(
            &[
                OsString::from("-c"),
                OsString::from("model=\"gpt-5.5\""),
                OsString::from("resume"),
                OsString::from("old"),
            ],
            "new",
            false,
        );

        assert_eq!(
            args,
            vec![
                OsString::from("-c"),
                OsString::from("model=\"gpt-5.5\""),
                OsString::from("resume"),
                OsString::from("new"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn pty_round_trips_fake_tui_and_exit_code() {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg("read line; printf 'got:%s\\n' \"$line\"; exit 7");
        let mut child = pair.slave.spawn_command(command).unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        writer.write_all(b"hello\n").unwrap();
        writer.flush().unwrap();

        let mut output = Vec::new();
        let mut buf = [0u8; 128];
        loop {
            let n = reader.read(&mut buf).unwrap();
            output.extend_from_slice(&buf[..n]);
            if String::from_utf8_lossy(&output).contains("got:hello") {
                break;
            }
        }
        let status = child.wait().unwrap();

        assert_eq!(status.exit_code(), 7);
    }
}
