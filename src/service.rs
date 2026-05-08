use std::collections::BTreeSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::RwLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use crate::app_server::AppServerClient;
use crate::app_server::AppServerProxy;
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
use crate::rate_limit;
use crate::run;
use crate::selector;
use crate::serve;
use crate::session;
use crate::session::AppThreadBinding;
use crate::session::BindAppThreadRequest;
use crate::slot;
use crate::target;

const SERVICE_STATE_SCHEMA_VERSION: u64 = 1;
const SERVICE_TOKEN_SCHEMA_VERSION: u64 = 1;
const LAUNCHD_LABEL: &str = "dev.xiaotian.cx.service";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceStateFile {
    schema_version: u64,
    pid: u32,
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
    child: Child,
    started_at_unix: u64,
    restarts: u64,
    last_exit: Option<String>,
}

struct AppServerSupervisor {
    stable_listen_url: String,
    stable_readyz_url: String,
    upstream_url: Arc<RwLock<String>>,
    proxy_handle: thread::JoinHandle<()>,
    child: ManagedChild,
    current_slot: String,
    current_target: Option<String>,
    generation: u64,
    rotation_scan_offset: u64,
    rotation_turn_state: rate_limit::TurnSideEffectState,
    cooldown_slots: BTreeSet<String>,
    rotation_pending: bool,
}

struct AppServerGeneration {
    child: ManagedChild,
    slot: String,
    codex_home: PathBuf,
    target: Option<String>,
    upstream_listen_url: String,
}

pub fn start(args: ServiceStartArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.spec.manager_dir.clone())?;
    fs::create_dir_all(paths.service_dir())
        .with_context(|| format!("create {}", paths.service_dir().display()))?;
    validate_spec(&args.spec)?;
    let run_spec = service_runtime_spec(&args.spec)?;

    let state_file = paths.service_state_file();
    if let Some(state) = read_state_if_exists(&state_file)? {
        if process_exists(state.pid) {
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
    let child = command.spawn().context("spawn cx service supervisor")?;

    let state = wait_for_service_state(&state_file, child.id(), Duration::from_secs(5))?;
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
    let supervisor_started_at = unix_now()?;
    let mut app_server = AppServerSupervisor::start(&args.spec, &paths)?;
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
            &app_server.child,
            telegram.as_ref(),
        )?,
    )?;
    loop {
        if let Some(status) = app_server
            .child
            .child
            .try_wait()
            .context("poll app-server child")?
        {
            app_server.child.last_exit = Some(status.to_string());
            app_server.child.restarts += 1;
            log_line(
                &paths,
                &format!("app-server generation exited with {status}; restarting"),
            )?;
            app_server.restart(&args.spec, &paths)?;
        }

        if let Some(child) = telegram.as_mut() {
            if let Some(status) = child.child.try_wait().context("poll telegram child")? {
                child.last_exit = Some(status.to_string());
                child.restarts += 1;
                log_line(
                    &paths,
                    &format!("telegram child exited with {status}; restarting"),
                )?;
                thread::sleep(Duration::from_secs(2));
                telegram = Some(start_telegram_child(&args.spec, &paths)?);
            }
        }

        if let Err(err) = app_server.observe_rotation(&args.spec, &paths) {
            log_line(
                &paths,
                &format!("app-server rotation check failed: {err:#}"),
            )?;
        }

        write_service_state(
            &paths,
            service_state(
                &paths,
                supervisor_started_at,
                &app_server.child,
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

    for child in &state.children {
        let _ = send_signal(child.pid, Signal::Terminate);
    }
    let _ = send_signal(state.pid, Signal::Terminate);

    let mut stopped = wait_for_pids_stopped(&state, args.wait_timeout);
    let mut forced = false;
    if !stopped && args.force {
        for child in &state.children {
            let _ = send_signal(child.pid, Signal::Kill);
        }
        let _ = send_signal(state.pid, Signal::Kill);
        forced = true;
        stopped = wait_for_pids_stopped(&state, args.wait_timeout);
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
        Some(state) if process_exists(state.pid) => "running",
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

impl AppServerSupervisor {
    fn start(spec: &ServiceSpecArgs, paths: &ManagerPaths) -> Result<Self> {
        let stable = ServiceListenUrl::parse(&spec.listen)?.resolve()?;
        let stable_listen_url = stable.websocket_url();
        let stable_readyz_url = stable.readyz_url();
        let upstream_url = Arc::new(RwLock::new(String::new()));
        let generation = start_app_server_generation(spec, paths, None, 0)?;
        replace_proxy_upstream(&upstream_url, &generation.upstream_listen_url)?;
        let AppServerGeneration {
            child,
            slot,
            codex_home,
            target,
            upstream_listen_url: _,
        } = generation;

        let proxy = AppServerProxy::new_dynamic(
            stable_listen_url.clone(),
            Arc::clone(&upstream_url),
            paths.serve_dir().join("events").join("default.jsonl"),
        );
        let proxy_handle = proxy.spawn().context("spawn stable app-server proxy")?;
        serve::register_app_server(
            paths,
            serve::ServeRegistration {
                pid: child.child.id(),
                slot: slot.clone(),
                codex_home: Some(codex_home.display().to_string()),
                target: target.clone(),
                listen_url: stable_listen_url.clone(),
                readyz_url: stable_readyz_url.clone(),
            },
        )?;
        log_line(
            paths,
            &format!(
                "app-server supervisor ready listen={stable_listen_url} generation=0 pid={}",
                child.child.id()
            ),
        )?;

        Ok(Self {
            stable_listen_url,
            stable_readyz_url,
            upstream_url,
            proxy_handle,
            child,
            current_slot: slot,
            current_target: target,
            generation: 0,
            rotation_scan_offset: proxy_event_log_len(paths).unwrap_or(0),
            rotation_turn_state: rate_limit::TurnSideEffectState::default(),
            cooldown_slots: BTreeSet::new(),
            rotation_pending: false,
        })
    }

    fn restart(&mut self, spec: &ServiceSpecArgs, paths: &ManagerPaths) -> Result<()> {
        let _ = serve::unregister_app_server(paths, self.child.child.id());
        self.generation = self
            .generation
            .checked_add(1)
            .context("app-server generation overflow")?;
        let mut generation = start_app_server_generation(spec, paths, None, self.generation)?;
        if let Err(err) = restore_sessions_on_generation(paths, &generation, self.generation) {
            let _ = generation.child.child.kill();
            let _ = generation.child.child.wait();
            return Err(err);
        }
        replace_proxy_upstream(&self.upstream_url, &generation.upstream_listen_url)?;
        let AppServerGeneration {
            child,
            slot,
            codex_home,
            target,
            upstream_listen_url: _,
        } = generation;
        self.child = child;
        self.current_slot = slot;
        self.current_target = target;
        serve::register_app_server(
            paths,
            serve::ServeRegistration {
                pid: self.child.child.id(),
                slot: self.current_slot.clone(),
                codex_home: Some(codex_home.display().to_string()),
                target: self.current_target.clone(),
                listen_url: self.stable_listen_url.clone(),
                readyz_url: self.stable_readyz_url.clone(),
            },
        )?;
        let _ = self.proxy_handle.thread().id();
        log_line(
            paths,
            &format!(
                "app-server generation ready listen={} generation={} pid={}",
                self.stable_listen_url,
                self.generation,
                self.child.child.id()
            ),
        )
    }

    fn observe_rotation(&mut self, spec: &ServiceSpecArgs, paths: &ManagerPaths) -> Result<()> {
        if self.rotation_pending && !self.has_active_app_threads(paths) {
            log_line(
                paths,
                "app-server rotation pending from unsafe limit signal; rotating at idle boundary",
            )?;
            self.rotate_after_limit(spec, paths)?;
            return Ok(());
        }

        let Some(signal) = self.scan_rate_limit_signal(paths)? else {
            return Ok(());
        };
        self.cooldown_slots.insert(self.current_slot.clone());
        if signal.safe_to_continue {
            log_line(
                paths,
                "app-server rate-limit signal detected at safe boundary; rotating generation",
            )?;
            self.rotate_after_limit(spec, paths)?;
        } else {
            self.rotation_pending = true;
            log_line(
                paths,
                "app-server rate-limit signal detected during active work; rotation deferred until idle boundary",
            )?;
        }
        Ok(())
    }

    fn scan_rate_limit_signal(
        &mut self,
        paths: &ManagerPaths,
    ) -> Result<Option<rate_limit::StreamRateLimitSignal>> {
        let path = proxy_event_log_path(paths);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("open {}", path.display())),
        };
        let file_len = file
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?
            .len();
        if self.rotation_scan_offset > file_len {
            self.rotation_scan_offset = 0;
            self.rotation_turn_state = rate_limit::TurnSideEffectState::default();
        }
        file.seek(SeekFrom::Start(self.rotation_scan_offset))
            .with_context(|| format!("seek {}", path.display()))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut signal = None;
        loop {
            let bytes = reader
                .read_line(&mut line)
                .with_context(|| format!("read {}", path.display()))?;
            if bytes == 0 {
                break;
            }
            self.rotation_scan_offset = self
                .rotation_scan_offset
                .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
            if let Some(observed) =
                rate_limit::inspect_stream_fragment(&line, &mut self.rotation_turn_state)
            {
                signal = Some(observed);
            }
            line.clear();
        }
        Ok(signal)
    }

    fn has_active_app_threads(&self, paths: &ManagerPaths) -> bool {
        let Ok(upstream_url) = self
            .upstream_url
            .read()
            .map(|value| value.clone())
            .map_err(|_| anyhow::anyhow!("proxy upstream lock poisoned"))
        else {
            return false;
        };
        if upstream_url.is_empty() {
            return false;
        }
        let Ok(sessions) = session::list_sessions(paths) else {
            return false;
        };
        let app_threads = sessions
            .into_iter()
            .filter_map(|session| session.app_thread)
            .collect::<Vec<_>>();
        if app_threads.is_empty() {
            return false;
        }
        let Ok(mut client) = AppServerClient::connect(&upstream_url, Duration::from_secs(2)) else {
            return false;
        };
        if client
            .initialize("cx-service", env!("CARGO_PKG_VERSION"))
            .is_err()
        {
            return false;
        }
        app_threads.into_iter().any(|app_thread| {
            client
                .thread_read(&app_thread.thread_id, false)
                .is_ok_and(|read| read.summary.active || read.summary.active_turn_id.is_some())
        })
    }

    fn rotate_after_limit(&mut self, spec: &ServiceSpecArgs, paths: &ManagerPaths) -> Result<()> {
        let Some(slot) = self.next_rotation_slot(spec, paths)? else {
            log_line(
                paths,
                "app-server rotation skipped: no alternate usable slot is available",
            )?;
            self.rotation_pending = false;
            return Ok(());
        };
        self.rotate_to_slot(spec, paths, &slot)?;
        self.mark_rotation_log_consumed(paths);
        self.rotation_pending = false;
        Ok(())
    }

    fn mark_rotation_log_consumed(&mut self, paths: &ManagerPaths) {
        self.rotation_scan_offset = proxy_event_log_len(paths).unwrap_or(self.rotation_scan_offset);
        self.rotation_turn_state = rate_limit::TurnSideEffectState::default();
    }

    fn next_rotation_slot(
        &mut self,
        spec: &ServiceSpecArgs,
        paths: &ManagerPaths,
    ) -> Result<Option<String>> {
        if spec.slot.is_some() {
            return Ok(None);
        }
        let candidates = if let Some(target_name) = spec.target.as_deref() {
            target::load_target(paths, target_name)?.slots_or_rotation(paths)?
        } else {
            slot::load_rotation(paths)?
        };
        if candidates.len() <= 1 {
            return Ok(None);
        }
        let results = selector::query_slots(paths, &candidates, run::usage_timeout())?;
        let mut excluded = self.cooldown_slots.clone();
        excluded.insert(self.current_slot.clone());
        if let Some(selected) = selector::choose_result_excluding(&results, &excluded) {
            return Ok(Some(selected.slot.clone()));
        }

        self.cooldown_slots.clear();
        let excluded = BTreeSet::from([self.current_slot.clone()]);
        Ok(
            selector::choose_result_excluding(&results, &excluded)
                .map(|result| result.slot.clone()),
        )
    }

    fn rotate_to_slot(
        &mut self,
        spec: &ServiceSpecArgs,
        paths: &ManagerPaths,
        next_slot: &str,
    ) -> Result<()> {
        let next_generation = self
            .generation
            .checked_add(1)
            .context("app-server generation overflow")?;
        let mut generation =
            start_app_server_generation(spec, paths, Some(next_slot), next_generation)?;
        if let Err(err) = restore_sessions_on_generation(paths, &generation, next_generation) {
            let _ = generation.child.child.kill();
            let _ = generation.child.child.wait();
            return Err(err);
        }
        replace_proxy_upstream(&self.upstream_url, &generation.upstream_listen_url)?;

        let AppServerGeneration {
            child,
            slot,
            codex_home,
            target,
            upstream_listen_url: _,
        } = generation;
        let old_slot = self.current_slot.clone();
        let mut old_child = std::mem::replace(&mut self.child, child);
        let old_pid = old_child.child.id();
        self.current_slot = slot;
        self.current_target = target;
        self.generation = next_generation;
        serve::register_app_server(
            paths,
            serve::ServeRegistration {
                pid: self.child.child.id(),
                slot: self.current_slot.clone(),
                codex_home: Some(codex_home.display().to_string()),
                target: self.current_target.clone(),
                listen_url: self.stable_listen_url.clone(),
                readyz_url: self.stable_readyz_url.clone(),
            },
        )?;
        let _ = serve::unregister_app_server(paths, old_pid);
        let _ = send_signal(old_pid, Signal::Terminate);
        let _ = old_child.child.wait();
        log_line(
            paths,
            &format!(
                "app-server generation rotated old_slot={old_slot} new_slot={} generation={} pid={}",
                self.current_slot,
                self.generation,
                self.child.child.id()
            ),
        )
    }
}

fn start_app_server_generation(
    spec: &ServiceSpecArgs,
    paths: &ManagerPaths,
    slot_override: Option<&str>,
    generation: u64,
) -> Result<AppServerGeneration> {
    let runtime = run::select_runtime(
        paths,
        slot_override.or(spec.slot.as_deref()),
        spec.target.as_deref(),
        false,
    )?;
    let real_codex = run::resolve_codex_bin(spec.codex_bin.as_deref())?;
    let upstream = ServiceListenUrl {
        host: String::from("127.0.0.1"),
        port: 0,
    }
    .resolve()?;
    let upstream_listen_url = upstream.websocket_url();

    let spec_command = run::build_slot_command_spec(
        paths,
        real_codex,
        &runtime.slot,
        runtime.target.as_ref(),
        vec![
            "app-server".into(),
            "--listen".into(),
            upstream_listen_url.clone().into(),
        ],
    )?;
    let slot = spec_command.slot().to_string();
    let codex_home = spec_command.codex_home.clone();
    let target = spec_command.target_name().map(str::to_string);
    let mut command = spec_command.into_command();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = command
        .spawn()
        .context("spawn codex app-server generation")?;
    if let Err(err) =
        wait_for_app_server_ready(&upstream.readyz_url(), Duration::from_secs(15), &mut child)
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
    log_line(
        paths,
        &format!(
            "app-server generation={generation} slot={slot} target={} upstream={upstream_listen_url} pid={}",
            target.as_deref().unwrap_or("<none>"),
            child.id()
        ),
    )?;
    Ok(AppServerGeneration {
        child: ManagedChild {
            name: "app-server",
            child,
            started_at_unix: unix_now()?,
            restarts: generation,
            last_exit: None,
        },
        slot,
        codex_home,
        target,
        upstream_listen_url,
    })
}

fn replace_proxy_upstream(upstream_url: &Arc<RwLock<String>>, next_url: &str) -> Result<()> {
    let mut stored = upstream_url
        .write()
        .map_err(|_| anyhow::anyhow!("proxy upstream lock poisoned"))?;
    *stored = next_url.to_string();
    Ok(())
}

fn restore_sessions_on_generation(
    paths: &ManagerPaths,
    generation: &AppServerGeneration,
    generation_index: u64,
) -> Result<usize> {
    let sessions = session::list_sessions(paths)?;
    let bindings = sessions
        .into_iter()
        .filter_map(|session| {
            session
                .app_thread
                .clone()
                .map(|app_thread| (session, app_thread))
        })
        .collect::<Vec<_>>();
    if bindings.is_empty() {
        return Ok(0);
    }

    let mut client =
        AppServerClient::connect(&generation.upstream_listen_url, Duration::from_secs(5))
            .with_context(|| {
                format!(
                    "connect new app-server generation {}",
                    generation.upstream_listen_url
                )
            })?;
    client.initialize("cx-service", env!("CARGO_PKG_VERSION"))?;

    let mut restored = 0_usize;
    for (session, app_thread) in bindings {
        let Some(path) = app_thread.path.as_deref() else {
            continue;
        };
        let read = match client.thread_read(&app_thread.thread_id, false) {
            Ok(read) => read,
            Err(_) => client
                .thread_resume_with_path(
                    &app_thread.thread_id,
                    Some(path),
                    Some(&app_thread.cwd),
                    true,
                )
                .with_context(|| {
                    format!(
                        "resume app-server thread {} from path {} on slot {}",
                        app_thread.thread_id, path, generation.slot
                    )
                })?,
        };
        session::bind_app_thread(
            paths,
            BindAppThreadRequest {
                session_id: session.session_id,
                app_thread: AppThreadBinding {
                    thread_id: read.summary.upstream_thread_id.clone(),
                    cwd: if read.summary.cwd.is_empty() {
                        app_thread.cwd
                    } else {
                        read.summary.cwd.clone()
                    },
                    title: read.summary.title.clone().or(app_thread.title),
                    slot: Some(generation.slot.clone()),
                    generation: generation_index,
                    path: read.summary.path.clone().or_else(|| Some(path.to_string())),
                    updated_at_unix: read.summary.updated_at_unix.max(0) as u64,
                },
            },
        )?;
        restored += 1;
    }
    Ok(restored)
}

fn proxy_event_log_path(paths: &ManagerPaths) -> PathBuf {
    paths.serve_dir().join("events").join("default.jsonl")
}

fn proxy_event_log_len(paths: &ManagerPaths) -> Result<u64> {
    Ok(fs::metadata(proxy_event_log_path(paths))?.len())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceListenUrl {
    host: String,
    port: u16,
}

impl ServiceListenUrl {
    fn parse(raw: &str) -> Result<Self> {
        let Some(rest) = raw.strip_prefix("ws://") else {
            anyhow::bail!("service --listen only supports ws:// loopback URLs");
        };
        if rest.contains('/') {
            anyhow::bail!("service --listen must not include a path");
        }
        let Some((host, port)) = rest.rsplit_once(':') else {
            anyhow::bail!("service --listen requires host:port");
        };
        if !matches!(host, "127.0.0.1" | "localhost") {
            anyhow::bail!("service --listen must bind a loopback host");
        }
        let port = port
            .parse::<u16>()
            .with_context(|| format!("invalid service --listen port: {port}"))?;
        Ok(Self {
            host: host.to_string(),
            port,
        })
    }

    fn resolve(self) -> Result<Self> {
        if self.port != 0 {
            return Ok(self);
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).context("reserve loopback port")?;
        let port = listener.local_addr().context("read reserved port")?.port();
        drop(listener);
        Ok(Self { port, ..self })
    }

    fn websocket_url(&self) -> String {
        format!("ws://{}:{}", self.host, self.port)
    }

    fn readyz_url(&self) -> String {
        format!("http://{}:{}/readyz", self.host, self.port)
    }
}

fn wait_for_app_server_ready(readyz_url: &str, timeout: Duration, child: &mut Child) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .context("build app-server readyz client")?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().context("poll codex app-server")? {
            anyhow::bail!("codex app-server exited before ready with {status}");
        }
        if let Ok(response) = client.get(readyz_url).send() {
            if response.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for codex app-server ready endpoint {readyz_url}");
        }
        thread::sleep(Duration::from_millis(100));
    }
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
        child,
        started_at_unix: unix_now()?,
        restarts: 0,
        last_exit: None,
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
    serve: &ManagedChild,
    telegram: Option<&ManagedChild>,
) -> Result<ServiceStateFile> {
    let mut children = vec![child_state(serve)?];
    if let Some(telegram) = telegram {
        children.push(child_state(telegram)?);
    }
    Ok(ServiceStateFile {
        schema_version: SERVICE_STATE_SCHEMA_VERSION,
        pid: std::process::id(),
        started_at_unix: supervisor_started_at,
        log_file: paths.service_log_file().display().to_string(),
        children,
    })
}

fn child_state(child: &ManagedChild) -> Result<ServiceChildState> {
    Ok(ServiceChildState {
        name: child.name.to_string(),
        pid: child.child.id(),
        started_at_unix: child.started_at_unix,
        restarts: child.restarts,
        last_exit: child.last_exit.clone(),
    })
}

fn wait_for_service_state(
    path: &Path,
    supervisor_pid: u32,
    timeout: Duration,
) -> Result<Option<ServiceStateFile>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(state) = read_state_if_exists(path)? {
            if state.pid == supervisor_pid {
                return Ok(Some(state));
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(None)
}

fn wait_for_pids_stopped(state: &ServiceStateFile, timeout_secs: f32) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f32(timeout_secs);
    loop {
        let service_alive = process_exists(state.pid);
        let child_alive = state.children.iter().any(|child| process_exists(child.pid));
        if !service_alive && !child_alive {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
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
    fn kill_arg(self) -> &'static str {
        match self {
            Self::Terminate => "-TERM",
            Self::Kill => "-KILL",
        }
    }
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: Signal) -> Result<()> {
    let status = Command::new("kill")
        .arg(signal.kill_arg())
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("send signal to pid {pid}"))?;
    if !status.success() {
        anyhow::bail!("failed to send signal to pid {pid}");
    }
    Ok(())
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _signal: Signal) -> Result<()> {
    anyhow::bail!("cx service stop is only supported on Unix platforms");
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
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
    fn read_last_lines_returns_suffix() {
        let path = std::env::temp_dir().join(format!("cx-service-log-{}.txt", std::process::id()));
        fs::write(&path, "a\nb\nc\n").unwrap();

        let lines = read_last_lines(&path, 2).unwrap();

        assert_eq!(lines, vec![String::from("b"), String::from("c")]);
        let _ = fs::remove_file(path);
    }
}
