use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
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

use crate::app_server::AppServerClient;
use crate::app_server::AppServerProxy;
use crate::app_server::AppThreadSummary;
use crate::app_server::InitializeInfo;
use crate::app_server::LoopbackWsUrl;
use crate::app_server::ThreadListInfo;
use crate::cli::ServeProbeArgs;
use crate::cli::ServeStartArgs;
use crate::cli::ServeStatusArgs;
use crate::cli::ServeStopArgs;
use crate::cli::ServeThreadsArgs;
use crate::paths::ManagerPaths;
use crate::run;

const SERVE_STATE_SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListenUrl {
    host: String,
    port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServeStateFile {
    schema_version: u64,
    pid: u32,
    slot: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    listen_url: String,
    readyz_url: String,
    started_at_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadyAppServer {
    pub(crate) listen_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServeStatusReport {
    schema_version: u64,
    state: String,
    ready: bool,
    state_file: String,
    server: Option<ServeStateFile>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServeStopReport {
    schema_version: u64,
    state: String,
    stopped: bool,
    cleaned: bool,
    forced: bool,
    state_file: String,
    server: Option<ServeStateFile>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServeProbeReport {
    schema_version: u64,
    listen_url: String,
    ready: bool,
    initialized: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    initialize: InitializeProbeReport,
    thread_list: ThreadListProbeReport,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeProbeReport {
    user_agent: String,
    codex_home: String,
    platform_family: String,
    platform_os: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadListProbeReport {
    thread_count: usize,
    has_next_cursor: bool,
    has_backwards_cursor: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServeThreadsReport {
    schema_version: u64,
    listen_url: String,
    ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    threads: Vec<ThreadReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backwards_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadReport {
    upstream_thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    preview: String,
    cwd: String,
    source: String,
    status: String,
    active: bool,
    created_at_unix: i64,
    updated_at_unix: i64,
}

impl ListenUrl {
    fn parse(raw: &str) -> Result<Self> {
        let Some(rest) = raw.strip_prefix("ws://") else {
            anyhow::bail!("serve --listen only supports ws:// loopback URLs");
        };
        if rest.contains('/') {
            anyhow::bail!("serve --listen must not include a path");
        }
        let Some((host, port)) = rest.rsplit_once(':') else {
            anyhow::bail!("serve --listen requires host:port");
        };
        if !matches!(host, "127.0.0.1" | "localhost") {
            anyhow::bail!("serve --listen must bind a loopback host: 127.0.0.1 or localhost");
        }
        let port = port
            .parse::<u16>()
            .with_context(|| format!("invalid serve --listen port: {port}"))?;
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

pub fn start(args: ServeStartArgs) -> Result<()> {
    if args.ready_timeout <= 0.0 {
        anyhow::bail!("--ready-timeout must be positive");
    }

    let paths = ManagerPaths::new(args.manager_dir.clone())?;
    let runtime = run::select_runtime(&paths, args.slot.as_deref(), args.target.as_deref(), false)?;
    let real_codex = run::resolve_codex_bin(args.codex_bin.as_deref())?;
    let listen = ListenUrl::parse(&args.listen)?.resolve()?;
    let proxy_listen_url = listen.websocket_url();
    let upstream = ListenUrl {
        host: listen.host.clone(),
        port: 0,
    }
    .resolve()?;
    let upstream_listen_url = upstream.websocket_url();

    let mut codex_args = vec![
        OsString::from("app-server"),
        OsString::from("--listen"),
        OsString::from(upstream_listen_url.clone()),
    ];
    codex_args.extend(args.args.into_iter().map(OsString::from));

    let spec = run::build_slot_command_spec(
        &paths,
        real_codex,
        &runtime.slot,
        runtime.target.as_ref(),
        codex_args,
    )?;

    eprintln!("cx serve slot: {}", spec.slot());
    if let Some(target) = spec.target_name() {
        eprintln!("cx serve target: {target}");
    }
    eprintln!("cx serve listen: {proxy_listen_url}");
    eprintln!("cx serve upstream: {upstream_listen_url}");

    let slot = spec.slot().to_string();
    let target = spec.target_name().map(str::to_string);
    let mut command = spec.into_command();
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = command.spawn().context("spawn codex app-server")?;

    if let Err(err) = wait_for_ready(&upstream.readyz_url(), args.ready_timeout, &mut child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }

    let proxy = AppServerProxy::new(
        proxy_listen_url.clone(),
        upstream_listen_url,
        paths.serve_dir().join("events").join("default.jsonl"),
    );
    let _proxy_handle = match proxy.spawn() {
        Ok(handle) => handle,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };

    let state = ServeStateFile {
        schema_version: SERVE_STATE_SCHEMA_VERSION,
        pid: child.id(),
        slot,
        target,
        listen_url: proxy_listen_url,
        readyz_url: listen.readyz_url(),
        started_at_unix: unix_now()?,
    };
    if let Err(err) = write_state(&paths, &state) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }

    eprintln!("cx serve ready: {}", state.readyz_url);
    let exit_status = child.wait().context("wait for codex app-server")?;
    let _ = remove_state_if_current(&paths, state.pid);
    if !exit_status.success() {
        anyhow::bail!("codex app-server exited with {exit_status}");
    }
    Ok(())
}

pub fn status(args: ServeStatusArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.manager_dir)?;
    let state_file = paths.serve_state_file();
    let Some(state) = read_state_if_exists(&state_file)? else {
        let report = ServeStatusReport {
            schema_version: SERVE_STATE_SCHEMA_VERSION,
            state: String::from("missing"),
            ready: false,
            state_file: state_file.display().to_string(),
            server: None,
        };
        return print_status(report, args.json);
    };

    let ready = state_is_ready(&state);
    let report = ServeStatusReport {
        schema_version: SERVE_STATE_SCHEMA_VERSION,
        state: if ready { "ready" } else { "stale" }.to_string(),
        ready,
        state_file: state_file.display().to_string(),
        server: Some(state),
    };
    print_status(report, args.json)
}

pub fn stop(args: ServeStopArgs) -> Result<()> {
    if args.wait_timeout <= 0.0 {
        anyhow::bail!("--wait-timeout must be positive");
    }

    let paths = ManagerPaths::new(args.manager_dir)?;
    let state_file = paths.serve_state_file();
    let Some(state) = read_state_if_exists(&state_file)? else {
        let report = ServeStopReport {
            schema_version: SERVE_STATE_SCHEMA_VERSION,
            state: String::from("missing"),
            stopped: false,
            cleaned: false,
            forced: false,
            state_file: state_file.display().to_string(),
            server: None,
        };
        return print_stop(report, args.json);
    };

    if !state_is_ready(&state) {
        let cleaned = remove_state_if_current(&paths, state.pid)?;
        let report = ServeStopReport {
            schema_version: SERVE_STATE_SCHEMA_VERSION,
            state: String::from("stale"),
            stopped: false,
            cleaned,
            forced: false,
            state_file: state_file.display().to_string(),
            server: Some(state),
        };
        return print_stop(report, args.json);
    }

    send_signal(state.pid, Signal::Terminate)?;
    let mut stopped = wait_for_stopped(&state, args.wait_timeout);
    let mut forced = false;
    if !stopped && args.force {
        send_signal(state.pid, Signal::Kill)?;
        forced = true;
        stopped = wait_for_stopped(&state, args.wait_timeout);
    }

    let cleaned = if stopped {
        remove_state_if_current(&paths, state.pid)? || !state_file.exists()
    } else {
        false
    };
    let report = ServeStopReport {
        schema_version: SERVE_STATE_SCHEMA_VERSION,
        state: if stopped { "stopped" } else { "timeout" }.to_string(),
        stopped,
        cleaned,
        forced,
        state_file: state_file.display().to_string(),
        server: Some(state),
    };
    print_stop(report, args.json)?;
    if !stopped {
        anyhow::bail!("codex app-server did not stop within --wait-timeout");
    }
    Ok(())
}

pub fn probe(args: ServeProbeArgs) -> Result<()> {
    if args.timeout <= 0.0 {
        anyhow::bail!("--timeout must be positive");
    }

    let paths = ManagerPaths::new(args.manager_dir)?;
    let state_file = paths.serve_state_file();
    let explicit_listen = args.listen.is_some();
    let (listen_url, state) = match args.listen {
        Some(listen_url) => (listen_url, None),
        None => {
            let state = read_state_if_exists(&state_file)?
                .with_context(|| format!("serve state not found: {}", state_file.display()))?;
            (state.listen_url.clone(), Some(state))
        }
    };
    LoopbackWsUrl::parse(&listen_url)?;

    let ready = if let Some(state) = &state {
        state_is_ready(state)
    } else {
        ListenUrl::parse(&listen_url)
            .map(|listen| probe_ready(&listen.readyz_url()))
            .unwrap_or(false)
    };
    if !ready && !explicit_listen {
        anyhow::bail!("serve state is stale: {}", state_file.display());
    }

    let mut client = AppServerClient::connect(&listen_url, Duration::from_secs_f32(args.timeout))?;
    let initialize = client.initialize("cx-probe", env!("CARGO_PKG_VERSION"))?;
    let thread_list = client.thread_list_probe()?;
    let report = ServeProbeReport {
        schema_version: SERVE_STATE_SCHEMA_VERSION,
        listen_url,
        ready,
        initialized: true,
        state_file: state.as_ref().map(|_| state_file.display().to_string()),
        slot: state.as_ref().map(|state| state.slot.clone()),
        target: state.as_ref().and_then(|state| state.target.clone()),
        initialize: initialize.into(),
        thread_list: thread_list.into(),
    };
    print_probe(report, args.json)
}

pub fn threads(args: ServeThreadsArgs) -> Result<()> {
    if args.timeout <= 0.0 {
        anyhow::bail!("--timeout must be positive");
    }
    if !(1..=100).contains(&args.limit) {
        anyhow::bail!("--limit must be between 1 and 100");
    }

    let paths = ManagerPaths::new(args.manager_dir)?;
    let state_file = paths.serve_state_file();
    let explicit_listen = args.listen.is_some();
    let (listen_url, state) = match args.listen {
        Some(listen_url) => (listen_url, None),
        None => {
            let state = read_state_if_exists(&state_file)?
                .with_context(|| format!("serve state not found: {}", state_file.display()))?;
            (state.listen_url.clone(), Some(state))
        }
    };
    LoopbackWsUrl::parse(&listen_url)?;

    let ready = if let Some(state) = &state {
        state_is_ready(state)
    } else {
        ListenUrl::parse(&listen_url)
            .map(|listen| probe_ready(&listen.readyz_url()))
            .unwrap_or(false)
    };
    if !ready && !explicit_listen {
        anyhow::bail!("serve state is stale: {}", state_file.display());
    }

    let mut client = AppServerClient::connect(&listen_url, Duration::from_secs_f32(args.timeout))?;
    client.initialize("cx-threads", env!("CARGO_PKG_VERSION"))?;
    let page = client.thread_list(args.limit)?;
    let report = ServeThreadsReport {
        schema_version: SERVE_STATE_SCHEMA_VERSION,
        listen_url,
        ready,
        state_file: state.as_ref().map(|_| state_file.display().to_string()),
        slot: state.as_ref().map(|state| state.slot.clone()),
        target: state.as_ref().and_then(|state| state.target.clone()),
        threads: page.threads.into_iter().map(ThreadReport::from).collect(),
        next_cursor: page.next_cursor,
        backwards_cursor: page.backwards_cursor,
    };
    print_threads(report, args.json)
}

pub(crate) fn ready_app_server(paths: &ManagerPaths) -> Result<ReadyAppServer> {
    let state_file = paths.serve_state_file();
    let state = read_state_if_exists(&state_file)?
        .with_context(|| format!("serve state not found: {}", state_file.display()))?;
    LoopbackWsUrl::parse(&state.listen_url)?;
    if !state_is_ready(&state) {
        anyhow::bail!("serve state is stale: {}", state_file.display());
    }
    Ok(ReadyAppServer {
        listen_url: state.listen_url,
    })
}

pub(crate) fn registered_app_server(paths: &ManagerPaths) -> Result<ReadyAppServer> {
    let state_file = paths.serve_state_file();
    let state = read_state_if_exists(&state_file)?
        .with_context(|| format!("serve state not found: {}", state_file.display()))?;
    LoopbackWsUrl::parse(&state.listen_url)?;
    Ok(ReadyAppServer {
        listen_url: state.listen_url,
    })
}

fn wait_for_ready(
    readyz_url: &str,
    timeout_secs: f32,
    child: &mut std::process::Child,
) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .context("build readyz client")?;
    let deadline = Instant::now() + Duration::from_secs_f32(timeout_secs);
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
            anyhow::bail!("timed out waiting for codex app-server ready endpoint");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn write_state(paths: &ManagerPaths, state: &ServeStateFile) -> Result<()> {
    fs::create_dir_all(paths.serve_dir())
        .with_context(|| format!("create {}", paths.serve_dir().display()))?;
    set_private_dir_permissions(&paths.serve_dir())?;

    let state_file = paths.serve_state_file();
    let tmp_file = paths.serve_dir().join("default.json.tmp");
    let content = serde_json::to_vec_pretty(state).context("serialize serve state")?;
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

fn read_state_if_exists(path: &Path) -> Result<Option<ServeStateFile>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let state = serde_json::from_str::<ServeStateFile>(&content)
        .with_context(|| format!("parse {}", path.display()))?;
    if state.schema_version != SERVE_STATE_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported serve state schema version: {}",
            state.schema_version
        );
    }
    Ok(Some(state))
}

fn remove_state_if_current(paths: &ManagerPaths, pid: u32) -> Result<bool> {
    let state_file = paths.serve_state_file();
    let Some(state) = read_state_if_exists(&state_file)? else {
        return Ok(false);
    };
    if state.pid == pid {
        fs::remove_file(&state_file).with_context(|| format!("remove {}", state_file.display()))?;
        return Ok(true);
    }
    Ok(false)
}

fn probe_ready(readyz_url: &str) -> bool {
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    else {
        return false;
    };
    client
        .get(readyz_url)
        .send()
        .is_ok_and(|response| response.status().is_success())
}

fn state_is_ready(state: &ServeStateFile) -> bool {
    process_looks_like_app_server(state.pid).unwrap_or(false) && probe_ready(&state.readyz_url)
}

fn wait_for_stopped(state: &ServeStateFile, timeout_secs: f32) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f32(timeout_secs);
    loop {
        if !state_is_ready(state) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
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

    fn name(self) -> &'static str {
        match self {
            Self::Terminate => "SIGTERM",
            Self::Kill => "SIGKILL",
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
        .with_context(|| format!("send {} to pid {pid}", signal.name()))?;
    if !status.success() {
        anyhow::bail!("failed to send {} to pid {pid}", signal.name());
    }
    Ok(())
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _signal: Signal) -> Result<()> {
    anyhow::bail!("cx serve stop is only supported on Unix platforms");
}

#[cfg(unix)]
fn process_looks_like_app_server(pid: u32) -> Result<bool> {
    let output = Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg("command=")
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("check pid {pid}"))?;
    if !output.status.success() {
        return Ok(false);
    }
    let command = String::from_utf8_lossy(&output.stdout);
    Ok(command.contains("app-server"))
}

#[cfg(not(unix))]
fn process_looks_like_app_server(_pid: u32) -> Result<bool> {
    Ok(true)
}

fn print_status(report: ServeStatusReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("cx serve: {}", report.state);
    println!("state: {}", report.state_file);
    if let Some(server) = report.server {
        println!("pid: {}", server.pid);
        println!("slot: {}", server.slot);
        if let Some(target) = server.target {
            println!("target: {target}");
        }
        println!("listen: {}", server.listen_url);
        println!("readyz: {}", server.readyz_url);
        println!("started_at_unix: {}", server.started_at_unix);
    }
    Ok(())
}

fn print_stop(report: ServeStopReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("cx serve stop: {}", report.state);
    println!("state: {}", report.state_file);
    println!("stopped: {}", report.stopped);
    println!("cleaned: {}", report.cleaned);
    if report.forced {
        println!("forced: true");
    }
    if let Some(server) = report.server {
        println!("pid: {}", server.pid);
        println!("slot: {}", server.slot);
        if let Some(target) = server.target {
            println!("target: {target}");
        }
        println!("listen: {}", server.listen_url);
    }
    Ok(())
}

fn print_probe(report: ServeProbeReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("cx serve probe: ok");
    println!("listen: {}", report.listen_url);
    println!("ready: {}", report.ready);
    if let Some(state_file) = report.state_file {
        println!("state: {state_file}");
    }
    if let Some(slot) = report.slot {
        println!("slot: {slot}");
    }
    if let Some(target) = report.target {
        println!("target: {target}");
    }
    println!("user_agent: {}", report.initialize.user_agent);
    println!("codex_home: {}", report.initialize.codex_home);
    println!(
        "platform: {}/{}",
        report.initialize.platform_family, report.initialize.platform_os
    );
    println!("threads_seen: {}", report.thread_list.thread_count);
    Ok(())
}

fn print_threads(report: ServeThreadsReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("cx serve threads: {}", report.threads.len());
    println!("listen: {}", report.listen_url);
    println!("ready: {}", report.ready);
    if let Some(state_file) = report.state_file {
        println!("state: {state_file}");
    }
    if let Some(slot) = report.slot {
        println!("slot: {slot}");
    }
    if let Some(target) = report.target {
        println!("target: {target}");
    }
    for thread in report.threads {
        let title = thread.title.as_deref().unwrap_or("");
        println!(
            "{}\t{}\t{}\t{}\t{}",
            thread.upstream_thread_id, thread.status, thread.source, thread.updated_at_unix, title
        );
    }
    Ok(())
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?
        .as_secs())
}

impl From<InitializeInfo> for InitializeProbeReport {
    fn from(info: InitializeInfo) -> Self {
        Self {
            user_agent: info.user_agent,
            codex_home: info.codex_home,
            platform_family: info.platform_family,
            platform_os: info.platform_os,
        }
    }
}

impl From<ThreadListInfo> for ThreadListProbeReport {
    fn from(info: ThreadListInfo) -> Self {
        Self {
            thread_count: info.thread_count,
            has_next_cursor: info.has_next_cursor,
            has_backwards_cursor: info.has_backwards_cursor,
        }
    }
}

impl From<AppThreadSummary> for ThreadReport {
    fn from(thread: AppThreadSummary) -> Self {
        Self {
            upstream_thread_id: thread.upstream_thread_id,
            title: thread.title,
            preview: thread.preview,
            cwd: thread.cwd,
            source: thread.source,
            status: thread.status,
            active: thread.active,
            created_at_unix: thread.created_at_unix,
            updated_at_unix: thread.updated_at_unix,
        }
    }
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
        let root = std::env::temp_dir().join(format!(
            "cx-serve-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
    }

    #[test]
    fn parse_rejects_non_loopback_listen_url() {
        let err = ListenUrl::parse("ws://0.0.0.0:1234").unwrap_err();

        assert!(format!("{err:#}").contains("loopback"));
    }

    #[test]
    fn parse_accepts_loopback_with_zero_port() {
        let listen = ListenUrl::parse("ws://127.0.0.1:0").unwrap();

        assert_eq!(
            listen,
            ListenUrl {
                host: String::from("127.0.0.1"),
                port: 0,
            }
        );
    }

    #[test]
    fn readyz_url_uses_http_scheme() {
        let listen = ListenUrl {
            host: String::from("127.0.0.1"),
            port: 17654,
        };

        assert_eq!(listen.readyz_url(), "http://127.0.0.1:17654/readyz");
    }

    #[test]
    fn state_round_trips_without_sensitive_fields() {
        let paths = temp_paths("state");
        let state = ServeStateFile {
            schema_version: SERVE_STATE_SCHEMA_VERSION,
            pid: 123,
            slot: String::from("bus1"),
            target: Some(String::from("research")),
            listen_url: String::from("ws://127.0.0.1:17654"),
            readyz_url: String::from("http://127.0.0.1:17654/readyz"),
            started_at_unix: 1_800_000_000,
        };

        write_state(&paths, &state).unwrap();
        let read_back = read_state_if_exists(&paths.serve_state_file()).unwrap();

        assert_eq!(read_back.unwrap().slot, "bus1");
        let content = fs::read_to_string(paths.serve_state_file()).unwrap();
        assert!(!content.contains("auth"));
        assert!(!content.contains("env"));

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn registered_app_server_reads_recorded_listen_url_without_probe() {
        let paths = temp_paths("registered");
        let state = ServeStateFile {
            schema_version: SERVE_STATE_SCHEMA_VERSION,
            pid: u32::MAX,
            slot: String::from("bus1"),
            target: None,
            listen_url: String::from("ws://127.0.0.1:9"),
            readyz_url: String::from("http://127.0.0.1:9/readyz"),
            started_at_unix: 1_800_000_000,
        };
        write_state(&paths, &state).unwrap();

        let server = registered_app_server(&paths).unwrap();

        assert_eq!(server.listen_url, "ws://127.0.0.1:9");
        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let paths = temp_paths("mode");
        let state = ServeStateFile {
            schema_version: SERVE_STATE_SCHEMA_VERSION,
            pid: 123,
            slot: String::from("bus1"),
            target: None,
            listen_url: String::from("ws://127.0.0.1:17654"),
            readyz_url: String::from("http://127.0.0.1:17654/readyz"),
            started_at_unix: 1_800_000_000,
        };

        write_state(&paths, &state).unwrap();

        let dir_mode = fs::metadata(paths.serve_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(paths.serve_state_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn stop_cleans_stale_state() {
        let paths = temp_paths("stop-stale");
        let state = ServeStateFile {
            schema_version: SERVE_STATE_SCHEMA_VERSION,
            pid: u32::MAX,
            slot: String::from("bus1"),
            target: None,
            listen_url: String::from("ws://127.0.0.1:9"),
            readyz_url: String::from("http://127.0.0.1:9/readyz"),
            started_at_unix: 1_800_000_000,
        };
        write_state(&paths, &state).unwrap();

        stop(ServeStopArgs {
            manager_dir: Some(paths.manager_dir.clone()),
            wait_timeout: 0.1,
            force: false,
            json: false,
        })
        .unwrap();

        assert!(!paths.serve_state_file().exists());
        let _ = fs::remove_dir_all(&paths.manager_dir);
    }
}
