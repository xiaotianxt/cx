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

use crate::cli::ServiceInstallArgs;
use crate::cli::ServiceLogsArgs;
use crate::cli::ServiceRunArgs;
use crate::cli::ServiceSpecArgs;
use crate::cli::ServiceStartArgs;
use crate::cli::ServiceStatusArgs;
use crate::cli::ServiceStopArgs;
use crate::cli::ServiceUninstallArgs;
use crate::paths::ManagerPaths;

const SERVICE_STATE_SCHEMA_VERSION: u64 = 1;
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

struct ManagedChild {
    name: &'static str,
    child: Child,
    started_at_unix: u64,
    restarts: u64,
    last_exit: Option<String>,
}

pub fn start(args: ServiceStartArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.spec.manager_dir.clone())?;
    fs::create_dir_all(paths.service_dir())
        .with_context(|| format!("create {}", paths.service_dir().display()))?;
    validate_spec(&args.spec)?;

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
    append_spec_args(&mut command, &args.spec);
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
    let mut serve = start_serve_child(&args.spec, &paths)?;
    wait_for_serve_state(&paths, Duration::from_secs(15))?;
    let mut telegram = if args.spec.no_telegram {
        None
    } else {
        Some(start_telegram_child(&args.spec, &paths)?)
    };

    write_service_state(
        &paths,
        service_state(&paths, supervisor_started_at, &serve, telegram.as_ref())?,
    )?;
    loop {
        if let Some(status) = serve.child.try_wait().context("poll serve child")? {
            serve.last_exit = Some(status.to_string());
            serve.restarts += 1;
            log_line(
                &paths,
                &format!("serve child exited with {status}; restarting"),
            )?;
            serve = start_serve_child(&args.spec, &paths)?;
            wait_for_serve_state(&paths, Duration::from_secs(15))?;
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

        write_service_state(
            &paths,
            service_state(&paths, supervisor_started_at, &serve, telegram.as_ref())?,
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

pub fn install(args: ServiceInstallArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.spec.manager_dir.clone())?;
    fs::create_dir_all(paths.service_dir())
        .with_context(|| format!("create {}", paths.service_dir().display()))?;
    validate_spec(&args.spec)?;
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
    append_spec_argv(&mut argv, &args.spec);
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
    if spec.telegram_token_op_ref.is_none()
        && std::env::var_os(&spec.telegram_bot_token_env).is_none()
    {
        anyhow::bail!(
            "{} is not set; pass --telegram-token-op-ref or use --no-telegram",
            spec.telegram_bot_token_env
        );
    }
    Ok(())
}

fn start_serve_child(spec: &ServiceSpecArgs, paths: &ManagerPaths) -> Result<ManagedChild> {
    let mut command = cx_command()?;
    command.arg("serve").arg("start");
    append_path_arg(&mut command, "--manager-dir", spec.manager_dir.as_ref());
    append_path_arg(&mut command, "--codex-bin", spec.codex_bin.as_ref());
    append_string_arg(&mut command, "--slot", spec.slot.as_ref());
    append_string_arg(&mut command, "--target", spec.target.as_ref());
    command.arg("--listen").arg(&spec.listen);
    spawn_managed("serve", command, paths)
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
        command.arg("--allow-chat").arg(chat.to_string());
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
    if let Some(token) = telegram_token(spec)? {
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

fn telegram_token(spec: &ServiceSpecArgs) -> Result<Option<String>> {
    if let Some(reference) = &spec.telegram_token_op_ref {
        let output = Command::new("op")
            .arg("read")
            .arg(reference)
            .output()
            .context("read Telegram token from 1Password")?;
        if !output.status.success() {
            anyhow::bail!("op read failed for Telegram token reference");
        }
        let token = String::from_utf8(output.stdout)
            .context("Telegram token from 1Password is not UTF-8")?
            .trim()
            .to_string();
        if token.is_empty() {
            anyhow::bail!("Telegram token from 1Password is empty");
        }
        return Ok(Some(token));
    }
    Ok(None)
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

fn wait_for_serve_state(paths: &ManagerPaths, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if paths.serve_state_file().exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!(
        "serve child did not become ready within {} seconds",
        timeout.as_secs()
    )
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
    append_string_arg(
        command,
        "--telegram-token-op-ref",
        spec.telegram_token_op_ref.as_ref(),
    );
    for chat in &spec.allow_chats {
        command.arg("--allow-chat").arg(chat.to_string());
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
    append_string_argv(
        argv,
        "--telegram-token-op-ref",
        spec.telegram_token_op_ref.as_ref(),
    );
    for chat in &spec.allow_chats {
        argv.push(String::from("--allow-chat"));
        argv.push(chat.to_string());
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
                String::from("--telegram-token-op-ref"),
                String::from("op://Private/A&B/credential"),
            ],
            Path::new("/tmp/cx.log"),
            Path::new("/tmp/cx.err"),
        );

        assert!(plist.contains("op://Private/A&amp;B/credential"));
        assert!(plist.contains("<string>dev.xiaotian.cx.service</string>"));
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
