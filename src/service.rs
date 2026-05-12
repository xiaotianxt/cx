//! Background service supervisor.
//!
//! The service owns the stable broker endpoint and channel adapter children.
//! Raw Codex app-server workers are managed behind the broker, not exposed as
//! the service control surface.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use crate::broker::AppServerBroker;
use crate::cli::ServiceInstallArgs;
use crate::cli::ServiceLogsArgs;
use crate::cli::ServiceRunArgs;
use crate::cli::ServiceSpecArgs;
use crate::cli::ServiceStartArgs;
use crate::cli::ServiceStatusArgs;
use crate::cli::ServiceStopArgs;
use crate::cli::ServiceTokenArgs;
use crate::cli::ServiceTokenCommand;
use crate::cli::ServiceTokenName;
use crate::cli::ServiceUninstallArgs;
use crate::paths::ManagerPaths;
use crate::run;
use crate::serve;
use crate::worker_pool::WorkerPoolConfig;

const SERVICE_STATE_SCHEMA_VERSION: u64 = 1;
const SERVICE_TOKEN_SCHEMA_VERSION: u64 = 1;
const LAUNCHD_LABEL: &str = "dev.xiaotian.cx.service";
const CHILD_RESTART_BASE_DELAY: Duration = Duration::from_secs(2);
const CHILD_RESTART_MAX_DELAY: Duration = Duration::from_secs(60);
const CHILD_RESTART_STABLE_RUNTIME: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceStateFile {
    schema_version: u64,
    pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_group_id: Option<u32>,
    started_at_unix: u64,
    log_file: String,
    children: Vec<ServiceChildState>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceChildState {
    name: String,
    pid: u32,
    started_at_unix: u64,
    restarts: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_exit: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceTokenFile {
    schema_version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    telegram: Option<StoredToken>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredToken {
    token: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceStartReport {
    schema_version: u64,
    state: String,
    state_file: String,
    log_file: String,
    service: Option<ServiceStateFile>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceStatusReport {
    schema_version: u64,
    state: String,
    state_file: String,
    log_file: String,
    service: Option<ServiceStateFile>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceStopReport {
    schema_version: u64,
    state: String,
    stopped: bool,
    cleaned: bool,
    forced: bool,
    state_file: String,
    service: Option<ServiceStateFile>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceTokenStatusReport {
    schema_version: u64,
    configured: bool,
    token_file: String,
}

struct ManagedChild {
    name: &'static str,
    child: Option<Child>,
    pid: u32,
    started_at_unix: u64,
    started_at: Option<Instant>,
    restarts: u64,
    last_exit: Option<String>,
    restart_at: Option<Instant>,
    restart_policy: RestartPolicy,
}

#[derive(Debug, Clone, Copy, Default)]
struct RestartPolicy {
    consecutive_failures: u32,
}

impl RestartPolicy {
    fn record_exit(&mut self, runtime: Duration) -> Duration {
        if runtime >= CHILD_RESTART_STABLE_RUNTIME {
            self.consecutive_failures = 0;
        }
        let delay = restart_delay_for_failure(self.consecutive_failures);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        delay
    }
}

pub fn start(args: ServiceStartArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.spec.manager_dir.clone())?;
    fs::create_dir_all(paths.service_dir())
        .with_context(|| format!("create {}", paths.service_dir().display()))?;
    validate_spec(&args.spec)?;
    let run_spec = service_runtime_spec(&args.spec)?;

    let state_file = paths.service_state_file();
    if let Some(state) = read_state_if_exists(&state_file)? {
        if service_state_is_current(&state) {
            return print_start(
                ServiceStartReport {
                    schema_version: SERVICE_STATE_SCHEMA_VERSION,
                    state: String::from("already-running"),
                    state_file: state_file.display().to_string(),
                    log_file: paths.service_log_file().display().to_string(),
                    service: Some(state),
                },
                args.json,
            );
        }
        let _ = fs::remove_file(&state_file);
    }

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.service_log_file())
        .with_context(|| format!("open {}", paths.service_log_file().display()))?;
    let exe = std::env::current_exe().context("resolve current cx executable")?;
    let mut command = Command::new(exe);
    command.arg("service").arg("run");
    append_spec_args(&mut command, &run_spec);
    detach_background_command(&mut command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().context("clone service log")?))
        .stderr(Stdio::from(log));
    let mut child = command.spawn().context("spawn cx service supervisor")?;

    let state = wait_for_service_state(&state_file, &mut child, Duration::from_secs(5))?;
    print_start(
        ServiceStartReport {
            schema_version: SERVICE_STATE_SCHEMA_VERSION,
            state: if state.is_some() {
                String::from("started")
            } else {
                String::from("starting")
            },
            state_file: state_file.display().to_string(),
            log_file: paths.service_log_file().display().to_string(),
            service: state,
        },
        args.json,
    )
}

pub fn run(args: ServiceRunArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.spec.manager_dir.clone())?;
    fs::create_dir_all(paths.service_dir())
        .with_context(|| format!("create {}", paths.service_dir().display()))?;
    validate_spec(&args.spec)?;

    log_line(&paths, "cx service supervisor starting")?;
    let supervisor_process_group_id = configure_supervisor_process_group(&paths)?;
    let supervisor_started_at = unix_now()?;
    let app_server = AppServerBroker::start(
        paths.clone(),
        &args.spec.listen,
        WorkerPoolConfig {
            codex_bin: args.spec.codex_bin.clone(),
            slot: args.spec.slot.clone(),
            target: args.spec.target.clone(),
        },
    )?;
    serve::register_app_server(
        &paths,
        serve::ServeRegistration {
            kind: serve::ServeEndpointKind::Broker,
            pid: std::process::id(),
            slot: String::from("broker"),
            codex_home: None,
            target: args.spec.target.clone(),
            listen_url: app_server.listen_url().to_string(),
            readyz_url: app_server.readyz_url().to_string(),
        },
    )?;
    let mut telegram = if args.spec.no_telegram {
        None
    } else {
        Some(start_telegram_child(&args.spec, &paths)?)
    };

    write_service_state(
        &paths,
        service_state(
            &paths,
            supervisor_started_at,
            supervisor_process_group_id,
            &app_server,
            telegram.as_ref(),
        )?,
    )?;
    loop {
        if let Err(err) = app_server.poll_workers() {
            log_line(&paths, &format!("broker worker poll failed: {err:#}"))?;
        }

        let telegram_restart = if let Some(child) = telegram.as_mut() {
            if let Some(process) = child.child.as_mut() {
                if let Some(status) = process.try_wait().context("poll telegram child")? {
                    let runtime = child.started_at.map_or(Duration::ZERO, |started| {
                        Instant::now().saturating_duration_since(started)
                    });
                    let delay = child.restart_policy.record_exit(runtime);
                    let restart_at = Instant::now() + delay;
                    child.last_exit = Some(format!("{status}; restarting in {}s", delay.as_secs()));
                    child.child = None;
                    child.pid = 0;
                    child.started_at = None;
                    child.restarts += 1;
                    child.restart_at = Some(restart_at);
                    log_line(
                        &paths,
                        &format!(
                            "telegram child exited with {status}; restarting in {}s",
                            delay.as_secs()
                        ),
                    )?;
                }
            }
            if child
                .restart_at
                .is_some_and(|restart_at| Instant::now() >= restart_at)
            {
                Some((
                    child.restarts,
                    child.last_exit.clone(),
                    child.restart_policy,
                ))
            } else {
                None
            }
        } else {
            None
        };
        if let Some((restarts, last_exit, restart_policy)) = telegram_restart {
            match start_telegram_child(&args.spec, &paths) {
                Ok(mut restarted) => {
                    restarted.restarts = restarts;
                    restarted.last_exit = last_exit;
                    restarted.restart_policy = restart_policy;
                    telegram = Some(restarted);
                }
                Err(err) => {
                    if let Some(child) = telegram.as_mut() {
                        let delay = child.restart_policy.record_exit(Duration::ZERO);
                        child.restart_at = Some(Instant::now() + delay);
                        child.last_exit = Some(format!(
                            "restart failed: {err:#}; retrying in {}s",
                            delay.as_secs()
                        ));
                        log_line(
                            &paths,
                            &format!(
                                "telegram child restart failed: {err:#}; retrying in {}s",
                                delay.as_secs()
                            ),
                        )?;
                    }
                }
            }
        }

        write_service_state(
            &paths,
            service_state(
                &paths,
                supervisor_started_at,
                supervisor_process_group_id,
                &app_server,
                telegram.as_ref(),
            )?,
        )?;
        thread::sleep(Duration::from_secs(1));
    }
}

pub fn stop(args: ServiceStopArgs) -> Result<()> {
    if args.wait_timeout <= 0.0 {
        anyhow::bail!("--wait-timeout must be positive");
    }

    let paths = ManagerPaths::new(args.manager_dir)?;
    let state_file = paths.service_state_file();
    let Some(state) = read_state_if_exists(&state_file)? else {
        return print_stop(
            ServiceStopReport {
                schema_version: SERVICE_STATE_SCHEMA_VERSION,
                state: String::from("missing"),
                stopped: false,
                cleaned: false,
                forced: false,
                state_file: state_file.display().to_string(),
                service: None,
            },
            args.json,
        );
    };

    signal_service(&state, Signal::Terminate);

    let mut stopped = wait_for_service_stopped(&state, args.wait_timeout);
    let mut forced = false;
    if !stopped && args.force {
        signal_service(&state, Signal::Kill);
        forced = true;
        stopped = wait_for_service_stopped(&state, args.wait_timeout);
    }

    let cleaned = if stopped && state_file.exists() {
        fs::remove_file(&state_file).is_ok()
    } else {
        false
    };
    print_stop(
        ServiceStopReport {
            schema_version: SERVICE_STATE_SCHEMA_VERSION,
            state: if stopped { "stopped" } else { "timeout" }.to_string(),
            stopped,
            cleaned,
            forced,
            state_file: state_file.display().to_string(),
            service: Some(state),
        },
        args.json,
    )?;
    if !stopped {
        anyhow::bail!("cx service did not stop within --wait-timeout");
    }
    Ok(())
}

pub fn status(args: ServiceStatusArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.manager_dir)?;
    let state_file = paths.service_state_file();
    let state = read_state_if_exists(&state_file)?;
    let status = match state.as_ref() {
        Some(state) if service_state_is_current(state) => "running",
        Some(_) => "stale",
        None => "missing",
    };
    print_status(
        ServiceStatusReport {
            schema_version: SERVICE_STATE_SCHEMA_VERSION,
            state: status.to_string(),
            state_file: state_file.display().to_string(),
            log_file: paths.service_log_file().display().to_string(),
            service: state,
        },
        args.json,
    )
}

pub fn logs(args: ServiceLogsArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.manager_dir)?;
    let lines = read_last_lines(&paths.service_log_file(), args.lines)?;
    for line in lines {
        println!("{line}");
    }
    Ok(())
}

pub fn token(args: ServiceTokenArgs) -> Result<()> {
    match args.command {
        ServiceTokenCommand::Set(args) => {
            let paths = ManagerPaths::new(args.manager_dir)?;
            let token = read_secret_from_stdin()?;
            let mut token_file = ServiceTokenFile {
                schema_version: SERVICE_TOKEN_SCHEMA_VERSION,
                telegram: None,
            };
            match args.token {
                ServiceTokenName::Telegram => {
                    token_file.telegram = Some(StoredToken { token });
                }
            }
            write_token_file(&paths, &token_file)?;
            println!(
                "configured service token: {}",
                paths.service_token_file().display()
            );
            Ok(())
        }
        ServiceTokenCommand::Status(args) => {
            let paths = ManagerPaths::new(args.manager_dir)?;
            let token = read_token_file_if_exists(&paths.service_token_file())?;
            let report = ServiceTokenStatusReport {
                schema_version: SERVICE_TOKEN_SCHEMA_VERSION,
                configured: token.as_ref().is_some_and(|token| token.telegram.is_some()),
                token_file: paths.service_token_file().display().to_string(),
            };
            print_token_status(report, args.json)
        }
        ServiceTokenCommand::Delete(args) => {
            let paths = ManagerPaths::new(args.manager_dir)?;
            let Some(mut token_file) = read_token_file_if_exists(&paths.service_token_file())?
            else {
                println!(
                    "service token not configured: {}",
                    paths.service_token_file().display()
                );
                return Ok(());
            };
            match args.token {
                ServiceTokenName::Telegram => {
                    token_file.telegram = None;
                }
            }
            write_token_file(&paths, &token_file)?;
            println!(
                "removed service token: {}",
                paths.service_token_file().display()
            );
            Ok(())
        }
    }
}

pub fn install(args: ServiceInstallArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.spec.manager_dir.clone())?;
    fs::create_dir_all(paths.service_dir())
        .with_context(|| format!("create {}", paths.service_dir().display()))?;
    validate_spec(&args.spec)?;
    let run_spec = service_runtime_spec(&args.spec)?;
    let plist = paths.service_launchd_plist_file()?;
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let exe = std::env::current_exe().context("resolve current cx executable")?;
    let mut argv = vec![
        exe.display().to_string(),
        String::from("service"),
        String::from("run"),
    ];
    append_spec_argv(&mut argv, &run_spec);
    let content = launchd_plist(&argv, &paths.service_log_file(), &paths.service_log_file());
    fs::write(&plist, content).with_context(|| format!("write {}", plist.display()))?;
    println!("installed launchd service: {}", plist.display());
    if args.start {
        launchctl_bootstrap(&plist)?;
        launchctl_kickstart()?;
        println!("started launchd service: {LAUNCHD_LABEL}");
    }
    Ok(())
}

pub fn uninstall(args: ServiceUninstallArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.manager_dir)?;
    let plist = paths.service_launchd_plist_file()?;
    let _ = launchctl_bootout();
    if plist.exists() {
        fs::remove_file(&plist).with_context(|| format!("remove {}", plist.display()))?;
        println!("removed launchd service: {}", plist.display());
    } else {
        println!("launchd service not installed: {}", plist.display());
    }
    Ok(())
}

fn validate_spec(spec: &ServiceSpecArgs) -> Result<()> {
    if spec.no_telegram {
        return Ok(());
    }
    Ok(())
}

fn service_runtime_spec(spec: &ServiceSpecArgs) -> Result<ServiceSpecArgs> {
    let mut run_spec = spec.clone();
    let codex_bin = run::resolve_codex_bin(spec.codex_bin.as_deref())?;
    run_spec.codex_bin = Some(resolve_launchable_command_path(&codex_bin)?);
    Ok(run_spec)
}

fn resolve_launchable_command_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if let Some(found) = run::find_executable_on_path(path) {
        return Ok(found);
    }
    let candidate = std::env::current_dir()
        .context("resolve current directory for codex path")?
        .join(path);
    if candidate.is_file() {
        return Ok(candidate);
    }
    anyhow::bail!(
        "unable to resolve codex executable {}; pass --codex-bin /absolute/path",
        path.display()
    );
}

fn start_telegram_child(spec: &ServiceSpecArgs, paths: &ManagerPaths) -> Result<ManagedChild> {
    let mut command = cx_command()?;
    command.arg("channel").arg("telegram").arg("run");
    append_path_arg(&mut command, "--manager-dir", spec.manager_dir.as_ref());
    command
        .arg("--bot-token-env")
        .arg(&spec.telegram_bot_token_env)
        .arg("--app-server-timeout")
        .arg(spec.app_server_timeout.to_string());
    for chat in &spec.allow_chats {
        append_allow_chat_arg(&mut command, *chat);
    }
    if spec.acquire_lease {
        command.arg("--acquire-lease");
    }
    if spec.steal {
        command.arg("--steal");
    }
    if spec.log_updates {
        command.arg("--log-updates");
    }
    if let Some(token) = telegram_token(spec, paths)? {
        command.env(&spec.telegram_bot_token_env, token);
    }
    spawn_managed("telegram", command, paths)
}

fn spawn_managed(
    name: &'static str,
    mut command: Command,
    paths: &ManagerPaths,
) -> Result<ManagedChild> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.service_log_file())
        .with_context(|| format!("open {}", paths.service_log_file().display()))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().context("clone service log")?))
        .stderr(Stdio::from(log));
    let child = command
        .spawn()
        .with_context(|| format!("spawn {name} child"))?;
    log_line(paths, &format!("{name} child started pid={}", child.id()))?;
    Ok(ManagedChild {
        name,
        pid: child.id(),
        child: Some(child),
        started_at_unix: unix_now()?,
        started_at: Some(Instant::now()),
        restarts: 0,
        last_exit: None,
        restart_at: None,
        restart_policy: RestartPolicy::default(),
    })
}

fn telegram_token(spec: &ServiceSpecArgs, paths: &ManagerPaths) -> Result<Option<String>> {
    if std::env::var_os(&spec.telegram_bot_token_env).is_some() {
        return Ok(None);
    }
    let token_file = read_token_file_if_exists(&paths.service_token_file())?.with_context(|| {
        format!(
            "{} is not set and no service token is configured; pipe the token into `cx service token set telegram` or use --no-telegram",
            spec.telegram_bot_token_env
        )
    })?;
    let token = token_file
        .telegram
        .with_context(|| "Telegram service token is not configured")?
        .token;
    Ok(Some(token))
}

fn service_state(
    paths: &ManagerPaths,
    supervisor_started_at: u64,
    supervisor_process_group_id: Option<u32>,
    broker: &AppServerBroker,
    telegram: Option<&ManagedChild>,
) -> Result<ServiceStateFile> {
    let mut children = broker_child_states(broker)?;
    if let Some(telegram) = telegram {
        children.push(child_state(telegram)?);
    }
    Ok(ServiceStateFile {
        schema_version: SERVICE_STATE_SCHEMA_VERSION,
        pid: std::process::id(),
        process_group_id: supervisor_process_group_id,
        started_at_unix: supervisor_started_at,
        log_file: paths.service_log_file().display().to_string(),
        children,
    })
}

fn broker_child_states(broker: &AppServerBroker) -> Result<Vec<ServiceChildState>> {
    Ok(broker
        .worker_records()?
        .into_iter()
        .map(|worker| ServiceChildState {
            name: format!("worker:{}", worker.slot),
            pid: worker.pid,
            started_at_unix: worker.started_at_unix,
            restarts: worker.generation,
            last_exit: (worker.status != crate::worker_pool::WorkerStatus::Ready)
                .then(|| format!("{:?}", worker.status)),
        })
        .collect())
}

fn child_state(child: &ManagedChild) -> Result<ServiceChildState> {
    Ok(ServiceChildState {
        name: child.name.to_string(),
        pid: child.pid,
        started_at_unix: child.started_at_unix,
        restarts: child.restarts,
        last_exit: child.last_exit.clone(),
    })
}

fn restart_delay_for_failure(consecutive_failures: u32) -> Duration {
    let shift = consecutive_failures.min(5);
    let multiplier = 1_u32 << shift;
    CHILD_RESTART_BASE_DELAY
        .saturating_mul(multiplier)
        .min(CHILD_RESTART_MAX_DELAY)
}

fn wait_for_service_state(
    path: &Path,
    supervisor: &mut Child,
    timeout: Duration,
) -> Result<Option<ServiceStateFile>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(state) = read_state_if_exists(path)? {
            if state.pid == supervisor.id() {
                return Ok(Some(state));
            }
        }
        if let Some(status) = supervisor
            .try_wait()
            .context("poll cx service supervisor during startup")?
        {
            anyhow::bail!("cx service supervisor exited before writing state: {status}");
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(None)
}

fn wait_for_service_stopped(state: &ServiceStateFile, timeout_secs: f32) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f32(timeout_secs);
    loop {
        if service_stopped(state) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn service_stopped(state: &ServiceStateFile) -> bool {
    if let Some(process_group_id) = verified_service_process_group(state) {
        return !process_group_exists(process_group_id);
    }
    let service_alive = service_state_is_current(state);
    let child_alive = state.children.iter().any(|child| process_exists(child.pid));
    !service_alive && !child_alive
}

fn write_service_state(paths: &ManagerPaths, state: ServiceStateFile) -> Result<()> {
    fs::create_dir_all(paths.service_dir())
        .with_context(|| format!("create {}", paths.service_dir().display()))?;
    let tmp_path = paths.service_dir().join("default.json.tmp");
    let content = serde_json::to_vec_pretty(&state).context("serialize service state")?;
    fs::write(&tmp_path, content).with_context(|| format!("write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, paths.service_state_file())
        .with_context(|| format!("rename {}", paths.service_state_file().display()))?;
    Ok(())
}

fn read_state_if_exists(path: &Path) -> Result<Option<ServiceStateFile>> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let state = serde_json::from_str::<ServiceStateFile>(&content)
                .with_context(|| format!("parse {}", path.display()))?;
            if state.schema_version != SERVICE_STATE_SCHEMA_VERSION {
                anyhow::bail!(
                    "unsupported service state schema version: {}",
                    state.schema_version
                );
            }
            Ok(Some(state))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn write_token_file(paths: &ManagerPaths, token: &ServiceTokenFile) -> Result<()> {
    fs::create_dir_all(paths.service_dir())
        .with_context(|| format!("create {}", paths.service_dir().display()))?;
    set_private_dir_permissions(&paths.service_dir())?;
    let tmp_path = paths.service_dir().join("tokens.json.tmp");
    let content = serde_json::to_vec_pretty(token).context("serialize service token")?;
    {
        let mut file = private_open_for_write(&tmp_path)?;
        file.write_all(&content)
            .with_context(|| format!("write {}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, paths.service_token_file())
        .with_context(|| format!("rename {}", paths.service_token_file().display()))?;
    Ok(())
}

fn read_token_file_if_exists(path: &Path) -> Result<Option<ServiceTokenFile>> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let token = serde_json::from_str::<ServiceTokenFile>(&content)
                .with_context(|| format!("parse {}", path.display()))?;
            if token.schema_version != SERVICE_TOKEN_SCHEMA_VERSION {
                anyhow::bail!(
                    "unsupported service token schema version: {}",
                    token.schema_version
                );
            }
            Ok(Some(token))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn read_secret_from_stdin() -> Result<String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("read token from stdin")?;
    let token = input.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("token stdin is empty");
    }
    Ok(token)
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
            .with_context(|| format!("set permissions on {}", path.display()))?;
    }
    Ok(())
}

fn print_start(report: ServiceStartReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("cx service: {}", report.state);
    println!("state: {}", report.state_file);
    println!("log: {}", report.log_file);
    if let Some(service) = report.service {
        print_service_state(service);
    }
    Ok(())
}

fn print_token_status(report: ServiceTokenStatusReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!(
        "cx service token: {}",
        if report.configured {
            "configured"
        } else {
            "missing"
        }
    );
    println!("token_file: {}", report.token_file);
    Ok(())
}

fn print_status(report: ServiceStatusReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("cx service: {}", report.state);
    println!("state: {}", report.state_file);
    println!("log: {}", report.log_file);
    if let Some(service) = report.service {
        print_service_state(service);
    }
    Ok(())
}

fn print_stop(report: ServiceStopReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("cx service stop: {}", report.state);
    println!("state: {}", report.state_file);
    println!("stopped: {}", report.stopped);
    println!("cleaned: {}", report.cleaned);
    if report.forced {
        println!("forced: true");
    }
    Ok(())
}

fn print_service_state(service: ServiceStateFile) {
    println!("pid: {}", service.pid);
    if let Some(process_group_id) = service.process_group_id {
        println!("process_group_id: {process_group_id}");
    }
    println!("started_at_unix: {}", service.started_at_unix);
    for child in service.children {
        println!(
            "child: {}\tpid={}\trestarts={}",
            child.name, child.pid, child.restarts
        );
        if let Some(last_exit) = child.last_exit {
            println!("child_last_exit: {}\t{}", child.name, last_exit);
        }
    }
}

fn read_last_lines(path: &Path, line_count: usize) -> Result<Vec<String>> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .with_context(|| format!("read {}", path.display()))?;
    let mut lines = content
        .lines()
        .rev()
        .take(line_count)
        .map(str::to_string)
        .collect::<Vec<_>>();
    lines.reverse();
    Ok(lines)
}

fn log_line(paths: &ManagerPaths, message: &str) -> Result<()> {
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.service_log_file())
        .with_context(|| format!("open {}", paths.service_log_file().display()))?;
    writeln!(log, "[{}] {message}", unix_now()?).context("write service log")
}

fn configure_supervisor_process_group(paths: &ManagerPaths) -> Result<Option<u32>> {
    #[cfg(unix)]
    {
        let pid = std::process::id();
        if current_process_group_id() == Some(pid) {
            return Ok(Some(pid));
        }
        // SAFETY: setpgid is called for the current process before the service
        // supervisor starts children. No borrowed Rust state crosses the call.
        let set_result = unsafe { libc::setpgid(0, 0) };
        if set_result == 0 && current_process_group_id() == Some(pid) {
            return Ok(Some(pid));
        }
        let err = std::io::Error::last_os_error();
        log_line(
            paths,
            &format!(
                "service supervisor could not create isolated process group; stop will use recorded pid snapshot only: {err}"
            ),
        )?;
        Ok(None)
    }
    #[cfg(not(unix))]
    {
        let _ = paths;
        Ok(None)
    }
}

fn cx_command() -> Result<Command> {
    Ok(Command::new(
        std::env::current_exe().context("resolve current cx executable")?,
    ))
}

#[cfg(unix)]
fn detach_background_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn detach_background_command(_command: &mut Command) {}

fn append_spec_args(command: &mut Command, spec: &ServiceSpecArgs) {
    append_path_arg(command, "--manager-dir", spec.manager_dir.as_ref());
    append_path_arg(command, "--codex-bin", spec.codex_bin.as_ref());
    append_string_arg(command, "--slot", spec.slot.as_ref());
    append_string_arg(command, "--target", spec.target.as_ref());
    command.arg("--listen").arg(&spec.listen);
    if spec.no_telegram {
        command.arg("--no-telegram");
    }
    command
        .arg("--telegram-bot-token-env")
        .arg(&spec.telegram_bot_token_env)
        .arg("--app-server-timeout")
        .arg(spec.app_server_timeout.to_string());
    for chat in &spec.allow_chats {
        append_allow_chat_arg(command, *chat);
    }
    if spec.acquire_lease {
        command.arg("--acquire-lease");
    }
    if spec.steal {
        command.arg("--steal");
    }
    if spec.log_updates {
        command.arg("--log-updates");
    }
}

fn append_spec_argv(argv: &mut Vec<String>, spec: &ServiceSpecArgs) {
    append_path_argv(argv, "--manager-dir", spec.manager_dir.as_ref());
    append_path_argv(argv, "--codex-bin", spec.codex_bin.as_ref());
    append_string_argv(argv, "--slot", spec.slot.as_ref());
    append_string_argv(argv, "--target", spec.target.as_ref());
    argv.push(String::from("--listen"));
    argv.push(spec.listen.clone());
    if spec.no_telegram {
        argv.push(String::from("--no-telegram"));
    }
    argv.push(String::from("--telegram-bot-token-env"));
    argv.push(spec.telegram_bot_token_env.clone());
    argv.push(String::from("--app-server-timeout"));
    argv.push(spec.app_server_timeout.to_string());
    for chat in &spec.allow_chats {
        append_allow_chat_argv(argv, *chat);
    }
    if spec.acquire_lease {
        argv.push(String::from("--acquire-lease"));
    }
    if spec.steal {
        argv.push(String::from("--steal"));
    }
    if spec.log_updates {
        argv.push(String::from("--log-updates"));
    }
}

fn append_path_arg(command: &mut Command, name: &str, value: Option<&PathBuf>) {
    if let Some(value) = value {
        command.arg(name).arg(value);
    }
}

fn append_string_arg(command: &mut Command, name: &str, value: Option<&String>) {
    if let Some(value) = value {
        command.arg(name).arg(value);
    }
}

fn append_path_argv(argv: &mut Vec<String>, name: &str, value: Option<&PathBuf>) {
    if let Some(value) = value {
        argv.push(name.to_string());
        argv.push(value.display().to_string());
    }
}

fn append_string_argv(argv: &mut Vec<String>, name: &str, value: Option<&String>) {
    if let Some(value) = value {
        argv.push(name.to_string());
        argv.push(value.clone());
    }
}

fn append_allow_chat_arg(command: &mut Command, chat: i64) {
    command.arg(format!("--allow-chat={chat}"));
}

fn append_allow_chat_argv(argv: &mut Vec<String>, chat: i64) {
    argv.push(format!("--allow-chat={chat}"));
}

fn launchd_plist(argv: &[String], stdout: &Path, stderr: &Path) -> String {
    let args = argv
        .iter()
        .map(|arg| format!("    <string>{}</string>", xml_escape(arg)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{}</string>
  <key>ProgramArguments</key>
  <array>
{}
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{}</string>
  <key>StandardErrorPath</key>
  <string>{}</string>
</dict>
</plist>
"#,
        LAUNCHD_LABEL,
        args,
        xml_escape(&stdout.display().to_string()),
        xml_escape(&stderr.display().to_string())
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn launchctl_bootstrap(plist: &Path) -> Result<()> {
    let target = launchctl_target()?;
    let _ = Command::new("launchctl")
        .arg("bootout")
        .arg(&target)
        .arg(plist)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let status = Command::new("launchctl")
        .arg("bootstrap")
        .arg(&target)
        .arg(plist)
        .status()
        .with_context(|| format!("launchctl bootstrap {}", plist.display()))?;
    if !status.success() {
        anyhow::bail!("launchctl bootstrap failed for {}", plist.display());
    }
    Ok(())
}

fn launchctl_kickstart() -> Result<()> {
    let service = format!("{}/{}", launchctl_target()?, LAUNCHD_LABEL);
    let status = Command::new("launchctl")
        .arg("kickstart")
        .arg("-k")
        .arg(&service)
        .status()
        .with_context(|| format!("launchctl kickstart {service}"))?;
    if !status.success() {
        anyhow::bail!("launchctl kickstart failed for {service}");
    }
    Ok(())
}

fn launchctl_bootout() -> Result<()> {
    let service = format!("{}/{}", launchctl_target()?, LAUNCHD_LABEL);
    let status = Command::new("launchctl")
        .arg("bootout")
        .arg(&service)
        .status()
        .with_context(|| format!("launchctl bootout {service}"))?;
    if !status.success() {
        anyhow::bail!("launchctl bootout failed for {service}");
    }
    Ok(())
}

fn launchctl_target() -> Result<String> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .context("read current uid")?;
    if !output.status.success() {
        anyhow::bail!("id -u failed");
    }
    let uid = String::from_utf8(output.stdout).context("uid is not UTF-8")?;
    Ok(format!("gui/{}", uid.trim()))
}

#[derive(Debug, Clone, Copy)]
enum Signal {
    Terminate,
    Kill,
}

impl Signal {
    #[cfg(unix)]
    fn signo(self) -> libc::c_int {
        match self {
            Self::Terminate => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
        }
    }
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: Signal) -> Result<()> {
    let pid_i32 = i32::try_from(pid).with_context(|| format!("invalid pid {pid}"))?;
    if pid_i32 <= 0 {
        anyhow::bail!("invalid pid {pid}");
    }
    // SAFETY: kill is invoked with a validated positive pid and a fixed signal
    // number. It does not touch Rust memory.
    let result = unsafe { libc::kill(pid_i32, signal.signo()) };
    if result == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error()).with_context(|| format!("send signal to pid {pid}"))
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _signal: Signal) -> Result<()> {
    anyhow::bail!("cx service stop is only supported on Unix platforms");
}

fn signal_service(state: &ServiceStateFile, signal: Signal) {
    if let Some(process_group_id) = verified_service_process_group(state) {
        if current_process_group_id() != Some(process_group_id) {
            let _ = send_signal_to_process_group(process_group_id, signal);
        }
    }
    for child in &state.children {
        let _ = send_signal(child.pid, signal);
    }
    let _ = send_signal(state.pid, signal);
}

fn service_state_is_current(state: &ServiceStateFile) -> bool {
    if let Some(process_group_id) = verified_service_process_group(state) {
        return process_group_exists(process_group_id);
    }
    legacy_service_process_is_current(state.pid)
}

#[cfg(unix)]
fn legacy_service_process_is_current(pid: u32) -> bool {
    process_exists(pid) && process_looks_like_service_supervisor(pid)
}

#[cfg(not(unix))]
fn legacy_service_process_is_current(pid: u32) -> bool {
    process_exists(pid)
}

fn verified_service_process_group(state: &ServiceStateFile) -> Option<u32> {
    let process_group_id = state.process_group_id?;
    if pid_process_group_id(state.pid) != Some(process_group_id) {
        return None;
    }
    if !process_looks_like_service_supervisor(state.pid) {
        return None;
    }
    Some(process_group_id)
}

#[cfg(unix)]
fn send_signal_to_process_group(process_group_id: u32, signal: Signal) -> Result<()> {
    let pgid = i32::try_from(process_group_id)
        .with_context(|| format!("invalid process group id {process_group_id}"))?;
    if pgid <= 0 {
        anyhow::bail!("invalid process group id {process_group_id}");
    }
    // SAFETY: kill is invoked with a negative process group id and a fixed
    // signal number. It does not alias Rust memory.
    let result = unsafe { libc::kill(-pgid, signal.signo()) };
    if result == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
        .with_context(|| format!("send signal to process group {process_group_id}"))
}

#[cfg(not(unix))]
fn send_signal_to_process_group(_process_group_id: u32, _signal: Signal) -> Result<()> {
    anyhow::bail!("cx service stop is only supported on Unix platforms");
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let Ok(pid_i32) = i32::try_from(pid) else {
        return false;
    };
    if pid_i32 <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 probes process existence without delivering a
    // signal and does not touch Rust memory.
    let result = unsafe { libc::kill(pid_i32, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
fn pid_process_group_id(pid: u32) -> Option<u32> {
    let pid = i32::try_from(pid).ok()?;
    // SAFETY: getpgid reads process metadata for the supplied pid and does not
    // access Rust memory.
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid < 0 {
        return None;
    }
    u32::try_from(pgid).ok()
}

#[cfg(not(unix))]
fn pid_process_group_id(_pid: u32) -> Option<u32> {
    None
}

#[cfg(unix)]
fn process_looks_like_service_supervisor(pid: u32) -> bool {
    let Some(command) = process_command_line(pid) else {
        return false;
    };
    command.contains("service") && command.contains("run")
}

#[cfg(unix)]
fn process_command_line(pid: u32) -> Option<String> {
    for ps in ["/bin/ps", "/usr/bin/ps", "ps"] {
        if ps.contains('/') && !Path::new(ps).is_file() {
            continue;
        }
        let Ok(output) = Command::new(ps)
            .arg("-p")
            .arg(pid.to_string())
            .arg("-o")
            .arg("command=")
            .output()
        else {
            continue;
        };
        if output.status.success() {
            return Some(String::from_utf8_lossy(&output.stdout).into_owned());
        }
    }
    None
}

#[cfg(not(unix))]
fn process_looks_like_service_supervisor(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn process_group_exists(process_group_id: u32) -> bool {
    let Ok(pgid) = i32::try_from(process_group_id) else {
        return false;
    };
    if pgid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 probes process group existence without
    // delivering a signal and does not touch Rust memory.
    let result = unsafe { libc::kill(-pgid, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_group_exists(_process_group_id: u32) -> bool {
    false
}

#[cfg(unix)]
fn current_process_group_id() -> Option<u32> {
    // SAFETY: getpgrp takes no arguments and has no Rust memory effects.
    let pgid = unsafe { libc::getpgrp() };
    u32::try_from(pgid).ok()
}

#[cfg(not(unix))]
fn current_process_group_id() -> Option<u32> {
    None
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
    fn launchd_plist_escapes_program_arguments() {
        let plist = launchd_plist(
            &[
                String::from("/tmp/cx"),
                String::from("service"),
                String::from("run"),
                String::from("--target"),
                String::from("A&B"),
            ],
            Path::new("/tmp/cx.log"),
            Path::new("/tmp/cx.err"),
        );

        assert!(plist.contains("<string>A&amp;B</string>"));
        assert!(plist.contains("<string>dev.xiaotian.cx.service</string>"));
    }

    #[test]
    fn service_argv_passes_negative_allow_chat_with_equals() {
        let spec = ServiceSpecArgs {
            manager_dir: None,
            codex_bin: None,
            slot: None,
            target: None,
            listen: "ws://127.0.0.1:0".to_string(),
            no_telegram: false,
            telegram_bot_token_env: "TELEGRAM_BOT_TOKEN".to_string(),
            allow_chats: vec![-1003586916929],
            acquire_lease: false,
            steal: false,
            log_updates: false,
            app_server_timeout: 600.0,
        };
        let mut argv = Vec::new();

        append_spec_argv(&mut argv, &spec);

        assert!(argv.contains(&"--allow-chat=-1003586916929".to_string()));
        assert!(!argv.contains(&"--allow-chat".to_string()));
    }

    #[test]
    fn service_argv_includes_resolved_codex_bin_for_supervised_runs() {
        let spec = ServiceSpecArgs {
            manager_dir: None,
            codex_bin: Some(PathBuf::from("/opt/homebrew/bin/codex")),
            slot: None,
            target: None,
            listen: "ws://127.0.0.1:0".to_string(),
            no_telegram: false,
            telegram_bot_token_env: "TELEGRAM_BOT_TOKEN".to_string(),
            allow_chats: Vec::new(),
            acquire_lease: false,
            steal: false,
            log_updates: false,
            app_server_timeout: 600.0,
        };
        let run_spec = service_runtime_spec(&spec).unwrap();
        let mut argv = Vec::new();

        append_spec_argv(&mut argv, &run_spec);

        let codex_bin = argv
            .windows(2)
            .find_map(|window| (window[0] == "--codex-bin").then(|| window[1].clone()));
        assert_eq!(codex_bin, Some(String::from("/opt/homebrew/bin/codex")));
    }

    #[test]
    fn restart_policy_backs_off_to_cap() {
        let mut policy = RestartPolicy::default();

        let delays = (0..7)
            .map(|_| policy.record_exit(Duration::from_secs(1)).as_secs())
            .collect::<Vec<_>>();

        assert_eq!(delays, vec![2, 4, 8, 16, 32, 60, 60]);
    }

    #[test]
    fn restart_policy_resets_after_stable_runtime() {
        let mut policy = RestartPolicy::default();
        assert_eq!(policy.record_exit(Duration::from_secs(1)).as_secs(), 2);
        assert_eq!(policy.record_exit(Duration::from_secs(1)).as_secs(), 4);
        assert_eq!(policy.record_exit(Duration::from_secs(1)).as_secs(), 8);

        let delay = policy.record_exit(CHILD_RESTART_STABLE_RUNTIME);

        assert_eq!(delay.as_secs(), 2);
        assert_eq!(policy.record_exit(Duration::from_secs(1)).as_secs(), 4);
    }

    #[test]
    fn legacy_service_state_defaults_process_group_to_none() {
        let state = serde_json::from_str::<ServiceStateFile>(
            r#"{
                "schemaVersion": 1,
                "pid": 123,
                "startedAtUnix": 10,
                "logFile": "/tmp/cx.log",
                "children": []
            }"#,
        )
        .unwrap();

        assert_eq!(state.process_group_id, None);
    }

    #[cfg(unix)]
    #[test]
    fn process_exists_reports_current_process() {
        assert!(process_exists(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn process_exists_rejects_invalid_pid_zero() {
        assert!(!process_exists(0));
    }

    #[cfg(unix)]
    #[test]
    fn current_test_process_is_not_service_supervisor() {
        assert!(!process_looks_like_service_supervisor(std::process::id()));
    }

    #[test]
    fn read_last_lines_returns_suffix() {
        let path = std::env::temp_dir().join(format!("cx-service-log-{}.txt", std::process::id()));
        fs::write(&path, "a\nb\nc\n").unwrap();

        let lines = read_last_lines(&path, 2).unwrap();

        assert_eq!(lines, vec![String::from("b"), String::from("c")]);
        let _ = fs::remove_file(path);
    }
}
