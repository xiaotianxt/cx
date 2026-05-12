use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
#[cfg(feature = "service")]
use std::thread;
#[cfg(feature = "service")]
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use clap::builder::OsStringValueParser;
use clap::error::ErrorKind as ClapErrorKind;
use clap::value_parser;
use clap::Arg;
use clap::ArgAction;
use clap::Command as ClapCommand;
use clap::ValueHint;

#[cfg(feature = "service")]
use crate::app_server::AppServerClient;
use crate::cli::LoginArgs;
use crate::envfile;
use crate::paths;
use crate::paths::ManagerPaths;
use crate::resume_id::ExplicitResumeId;
use crate::selector;
#[cfg(feature = "service")]
use crate::serve;
#[cfg(feature = "service")]
use crate::serve::ServeEndpointKind;
use crate::slot;
use crate::target::TargetSpec;
#[cfg(feature = "service")]
use crate::thread_resolver;
#[cfg(feature = "service")]
use crate::thread_resolver::ThreadResolverDecision;
#[cfg(feature = "service")]
use crate::thread_resolver::ThreadResolverScope;

const BYPASS_SUBCOMMANDS: &[&str] = &[
    "login",
    "logout",
    "completion",
    "plugin",
    "mcp",
    "mcp-server",
    "app-server",
    "app",
    "sandbox",
    "debug",
    "apply",
    "exec-server",
    "features",
    "help",
];

pub(crate) const CODEX_SQLITE_HOME: &str = "CODEX_SQLITE_HOME";

const ARG_SLOT: &str = "slot";
const ARG_TARGET: &str = "target";
const ARG_MANAGER_DIR: &str = "manager-dir";
const ARG_CODEX_BIN: &str = "codex-bin";
const ARG_CX_QUIET: &str = "cx-quiet";
const ARG_CX_DEBUG: &str = "cx-debug";
#[cfg(feature = "service")]
const ARG_CX_SERVICE_REMOTE: &str = "cx-service-remote";
const ARG_MANAGED: &str = "managed";
const ARG_CODEX_ARGS: &str = "codex-args";

pub(crate) fn launcher_command() -> ClapCommand {
    let command = ClapCommand::new("cx")
        .about("Launch Codex through a local cx slot")
        .override_usage("cx [CX_OPTIONS] [-- CODEX_ARGS]...")
        .after_help(
            "Codex arguments must follow `--`. Examples:\n  cx --slot dia1 -- resume THREAD\n  echo prompt | cx -- summarize this",
        )
        .arg(
            Arg::new(ARG_SLOT)
                .long(ARG_SLOT)
                .short('s')
                .value_name("SLOT")
                .help("Use a specific slot"),
        )
        .arg(
            Arg::new(ARG_TARGET)
                .long(ARG_TARGET)
                .value_name("TARGET")
                .help("Use a target config"),
        )
        .arg(
            Arg::new(ARG_MANAGER_DIR)
                .long(ARG_MANAGER_DIR)
                .value_name("DIR")
                .value_parser(value_parser!(PathBuf))
                .value_hint(ValueHint::DirPath)
                .help("Profile-manager directory"),
        )
        .arg(
            Arg::new(ARG_CODEX_BIN)
                .long(ARG_CODEX_BIN)
                .value_name("FILE")
                .value_parser(value_parser!(PathBuf))
                .value_hint(ValueHint::FilePath)
                .help("Path to the real Codex binary"),
        )
        .arg(
            Arg::new(ARG_CX_QUIET)
                .long(ARG_CX_QUIET)
                .action(ArgAction::SetTrue)
                .help("Suppress cx slot banner"),
        )
        .arg(
            Arg::new(ARG_CX_DEBUG)
                .long(ARG_CX_DEBUG)
                .action(ArgAction::SetTrue)
                .help("Print slot selection details"),
        );
    #[cfg(feature = "service")]
    let command = command.arg(
        Arg::new(ARG_CX_SERVICE_REMOTE)
            .long(ARG_CX_SERVICE_REMOTE)
            .action(ArgAction::SetTrue)
            .help("Use experimental cx service remote"),
    );
    command
        .arg(
            Arg::new(ARG_MANAGED)
                .long(ARG_MANAGED)
                .hide(true)
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(ARG_CODEX_ARGS)
                .value_name("CODEX_ARGS")
                .value_parser(OsStringValueParser::new())
                .num_args(0..)
                .allow_hyphen_values(true)
                .last(true)
                .help("Arguments passed verbatim to Codex after `--`"),
        )
}

#[derive(Debug, Clone, Default, PartialEq)]
struct RunOptions {
    slot: Option<String>,
    target: Option<String>,
    manager_dir: Option<PathBuf>,
    codex_bin: Option<PathBuf>,
    quiet: bool,
    debug: bool,
    #[cfg(feature = "service")]
    service_remote: bool,
    codex_args: Vec<OsString>,
}

pub(crate) struct LauncherArgsSplit {
    pub(crate) cx_args: Vec<OsString>,
    pub(crate) codex_args: Vec<OsString>,
}

pub(crate) struct RuntimeSelection {
    pub slot: String,
    pub target: Option<TargetSpec>,
}

#[derive(Debug, Clone)]
pub(crate) struct CodexCommandSpec {
    pub(crate) program: PathBuf,
    pub(crate) codex_home: PathBuf,
    pub(crate) envs: BTreeMap<String, String>,
    pub(crate) args: Vec<OsString>,
    pub(crate) slot: String,
    pub(crate) target_name: Option<String>,
    pub(crate) launch_context: CodexLaunchContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexLaunchContext {
    Slot,
    #[cfg(feature = "service")]
    ServiceBroker,
}

impl CodexCommandSpec {
    pub(crate) fn slot(&self) -> &str {
        &self.slot
    }

    pub(crate) fn target_name(&self) -> Option<&str> {
        self.target_name.as_deref()
    }

    pub(crate) fn into_command(self) -> Command {
        let mut command = Command::new(self.program);
        command.env("CODEX_HOME", self.codex_home);
        command.envs(self.envs);
        command.args(self.args);
        command
    }
}

pub fn run_from_args(args: Vec<OsString>) -> Result<()> {
    let options = parse_run_args(args)?;
    let paths = ManagerPaths::new(options.manager_dir.clone())?;
    let real_codex = resolve_codex_bin(options.codex_bin.as_deref())?;

    if std::env::var_os("CODEX_HOME").is_some()
        && options.slot.is_none()
        && options.target.is_none()
    {
        return exec_real_codex(&real_codex, options.codex_args);
    }

    if options.slot.is_none()
        && options.target.is_none()
        && first_forwarded_non_option(&options.codex_args)
            .as_deref()
            .is_some_and(|arg| BYPASS_SUBCOMMANDS.contains(&arg))
    {
        return exec_real_codex(&real_codex, options.codex_args);
    }

    #[cfg(feature = "service")]
    let clean_tui_launch = options.codex_args.is_empty();
    #[cfg(feature = "service")]
    let workspace = resolve_workspace_from_args(&options.codex_args)?;
    #[cfg(feature = "service")]
    let requested_resume_id = explicit_resume_id(&options.codex_args);
    #[cfg(feature = "service")]
    if should_try_service_remote(&options) {
        match remote_attach_spec(
            &paths,
            real_codex.clone(),
            &options.codex_args,
            &workspace,
            requested_resume_id,
            clean_tui_launch,
        ) {
            Ok(Some(spec)) => {
                warn_remote_ignores_local_selection(&options, &spec);
                let resumed_id = explicit_resume_id(&spec.args);
                print_launch(
                    &spec,
                    resumed_id.as_ref().map(ExplicitResumeId::as_str),
                    options.quiet,
                );
                if let Err(err) = supervise_remote_tui(&paths, spec) {
                    warn_remote_fallback(&err);
                } else {
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(err) => warn_remote_fallback(&err),
        }
    }

    let target = crate::target::load_optional_target(&paths, options.target.as_deref())?;
    let candidates = if let Some(target) = &target {
        target.slots_or_rotation(&paths)?
    } else {
        slot::load_rotation(&paths)?
    };
    if candidates.is_empty() && options.slot.is_none() {
        return exec_real_codex(&real_codex, options.codex_args);
    }

    let selected_slot = if let Some(slot) = options.slot.clone() {
        slot
    } else {
        let results = selector::query_slots(&paths, &candidates, usage_timeout())?;
        let selected = selector::choose_result(&results);
        if options.debug || std::env::var_os("CX_SLOT_DEBUG").is_some() {
            crate::output::print_report(
                &results,
                selected.map(|result| result.slot.as_str()),
                false,
            )?;
        }
        selected
            .map(|result| result.slot.clone())
            .context("no usable Codex slot found")?
    };

    exec_slot_codex(
        &paths,
        &real_codex,
        &selected_slot,
        target.as_ref(),
        options,
    )
}

pub fn exec_slot_login(paths: &ManagerPaths, args: LoginArgs) -> Result<()> {
    let real_codex = resolve_codex_bin(args.codex_bin.as_deref())?;
    let slot_home = paths.slot_home(&args.slot);
    let sqlite_home = paths.slot_sqlite_home(&args.slot);
    ensure_sqlite_home(&sqlite_home)?;
    let mut command = Command::new(real_codex);
    command.env("CODEX_HOME", slot_home);
    command.env(CODEX_SQLITE_HOME, sqlite_home);
    command.arg("login");
    command.args(args.args);
    exec(command)
}

pub(crate) fn should_skip_stdin_wrapper(args: &[OsString]) -> bool {
    if args.iter().any(|arg| arg == "--managed") {
        return true;
    }
    let Ok(options) = parse_run_args(args.to_vec()) else {
        return true;
    };
    if first_forwarded_non_option(&options.codex_args).is_none()
        && options
            .codex_args
            .iter()
            .any(|arg| matches!(arg.to_str(), Some("-V" | "--version")))
    {
        return true;
    }
    first_forwarded_non_option(&options.codex_args)
        .is_some_and(|arg| BYPASS_SUBCOMMANDS.contains(&arg.as_str()))
}

pub(crate) fn select_runtime(
    paths: &ManagerPaths,
    slot: Option<&str>,
    target_name: Option<&str>,
    debug: bool,
) -> Result<RuntimeSelection> {
    let target = crate::target::load_optional_target(paths, target_name)?;
    if let Some(slot) = slot {
        return Ok(RuntimeSelection {
            slot: slot.to_string(),
            target,
        });
    }

    let candidates = if let Some(target) = &target {
        target.slots_or_rotation(paths)?
    } else {
        slot::load_rotation(paths)?
    };
    if candidates.is_empty() {
        anyhow::bail!("no configured slots");
    }

    let results = selector::query_slots(paths, &candidates, usage_timeout())?;
    let selected = selector::choose_result(&results);
    if debug || std::env::var_os("CX_SLOT_DEBUG").is_some() {
        crate::output::print_report(&results, selected.map(|result| result.slot.as_str()), false)?;
    }
    let slot = selected
        .map(|result| result.slot.clone())
        .context("no usable Codex slot found")?;
    Ok(RuntimeSelection { slot, target })
}

pub(crate) fn build_slot_command_spec(
    paths: &ManagerPaths,
    real_codex: PathBuf,
    selected_slot: &str,
    target: Option<&TargetSpec>,
    codex_args: Vec<OsString>,
) -> Result<CodexCommandSpec> {
    let slot_home = paths.slot_home(selected_slot);
    if !slot_home.is_dir() {
        anyhow::bail!(
            "missing slot home for {selected_slot}: {}",
            slot_home.display()
        );
    }

    slot::repair_slot_layout(paths, selected_slot)?;

    let slot_dir = paths.slot_dir(selected_slot);
    let mut overrides = slot::read_override_lines(&slot_dir)?;
    let mut envs = envfile::read_env_file(&slot_dir.join("env.conf"))?;
    let target_name = target.map(|target| target.name().to_string());
    if let Some(target) = target {
        overrides.extend(target.overrides().iter().cloned());
        envs.extend(target.env().clone());
    }
    insert_sqlite_home_env(&mut envs, paths.slot_sqlite_home(selected_slot))?;

    let mut args = Vec::new();
    for override_line in overrides {
        args.push(OsString::from("-c"));
        args.push(OsString::from(override_line));
    }
    args.extend(codex_args);

    Ok(CodexCommandSpec {
        program: real_codex,
        codex_home: slot_home,
        envs,
        args,
        slot: selected_slot.to_string(),
        target_name,
        launch_context: CodexLaunchContext::Slot,
    })
}

fn exec_slot_codex(
    paths: &ManagerPaths,
    real_codex: &Path,
    selected_slot: &str,
    target: Option<&TargetSpec>,
    options: RunOptions,
) -> Result<()> {
    let spec = build_slot_command_spec(
        paths,
        real_codex.to_path_buf(),
        selected_slot,
        target,
        options.codex_args,
    )?;
    let resumed_id = explicit_resume_id(&spec.args);
    print_launch(
        &spec,
        resumed_id.as_ref().map(ExplicitResumeId::as_str),
        options.quiet,
    );
    exec(spec.into_command())
}

fn print_launch(spec: &CodexCommandSpec, resumed_id: Option<&str>, quiet: bool) {
    if quiet {
        return;
    }
    match spec.launch_context {
        CodexLaunchContext::Slot => eprintln!("codex slot: {}", spec.slot()),
        #[cfg(feature = "service")]
        CodexLaunchContext::ServiceBroker => eprintln!("codex service: broker"),
    }
    if let Some(target) = spec.target_name() {
        eprintln!("codex target: {target}");
    }
    if let Some(resumed_id) = resumed_id {
        eprintln!("codex resume: {resumed_id}");
    }
}

#[cfg(feature = "service")]
fn warn_remote_fallback(err: &anyhow::Error) {
    eprintln!("cx warning: service remote unavailable ({err:#}); falling back to local Codex");
}

#[cfg(feature = "service")]
fn should_try_service_remote(options: &RunOptions) -> bool {
    options.service_remote && options.slot.is_none() && options.target.is_none()
}

#[cfg(feature = "service")]
fn warn_remote_ignores_local_selection(options: &RunOptions, spec: &CodexCommandSpec) {
    if options.quiet {
        return;
    }
    if let Some(slot) = options.slot.as_deref() {
        match spec.launch_context {
            CodexLaunchContext::Slot if slot != spec.slot() => {
                eprintln!(
                    "cx warning: --slot {slot} ignored because service remote is using slot {}",
                    spec.slot()
                );
            }
            CodexLaunchContext::ServiceBroker => {
                eprintln!(
                    "cx warning: --slot {slot} ignored because service remote is using broker"
                );
            }
            _ => {}
        }
    }
    if let Some(target) = options
        .target
        .as_deref()
        .filter(|target| Some(*target) != spec.target_name())
    {
        eprintln!(
            "cx warning: --target {target} ignored because service remote is using target {}",
            spec.target_name().unwrap_or("<none>")
        );
    }
}

#[cfg(feature = "service")]
const MAX_REMOTE_TUI_RESTARTS: u64 = 3;

#[cfg(feature = "service")]
fn supervise_remote_tui(paths: &ManagerPaths, spec: CodexCommandSpec) -> Result<()> {
    let mut restarts = 0_u64;
    loop {
        preload_remote_thread(paths, &spec)?;
        let status = spec
            .clone()
            .into_command()
            .status()
            .context("run remote Codex TUI")?;
        if status.success() {
            return Ok(());
        }
        restarts = restarts
            .checked_add(1)
            .context("remote TUI restart counter overflow")?;
        if serve::ready_app_server(paths).is_err() {
            anyhow::bail!("remote Codex TUI exited with {status} and cx service is not ready");
        }
        if restarts >= MAX_REMOTE_TUI_RESTARTS {
            anyhow::bail!(
                "remote Codex TUI exited with {status} after {restarts} attempts; \
                 the cached session may be stale (e.g. after a codex upgrade). \
                 Try `cx service restart`."
            );
        }
        eprintln!(
            "cx remote tui exited with {status}; restarting on same app-server thread (attempt {restarts})"
        );
        thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(feature = "service")]
fn preload_remote_thread(paths: &ManagerPaths, spec: &CodexCommandSpec) -> Result<()> {
    let Some(app_server_url) = remote_arg_value(&spec.args, "--remote") else {
        return Ok(());
    };
    let Some(resume_id) = remote_resume_id(&spec.args) else {
        return Ok(());
    };
    let workspace = resolve_workspace_from_args(&spec.args)?;
    let server = serve::ready_app_server(paths).context("service app-server is not ready")?;
    let mut client = AppServerClient::connect(app_server_url, Duration::from_secs(5))
        .context("connect app-server before remote TUI resume")?;
    client.initialize("cx-tui", env!("CARGO_PKG_VERSION"))?;
    let outcome = thread_resolver::resolve_app_thread(
        paths,
        &mut client,
        ThreadResolverScope {
            cwd: workspace,
            channel_id: None,
            explicit_resume_id: Some(resume_id),
            slot: server.app_slot(),
            generation: 0,
        },
    )?;
    if let ThreadResolverDecision::Refuse { reason } = outcome.decision {
        anyhow::bail!("{reason}");
    }
    Ok(())
}

#[cfg(feature = "service")]
fn remote_arg_value<'a>(args: &'a [OsString], name: &str) -> Option<&'a str> {
    let mut index = 0;
    while index < args.len() {
        let Some(arg) = args[index].to_str() else {
            index += 1;
            continue;
        };
        if arg == name {
            return args.get(index + 1).and_then(|value| value.to_str());
        }
        if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
            return Some(value);
        }
        index += 1;
    }
    None
}

#[cfg(feature = "service")]
fn remote_resume_id(args: &[OsString]) -> Option<ExplicitResumeId> {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].to_string_lossy();
        if arg.starts_with('-') {
            match codex_option_kind(arg.as_ref()) {
                CodexOptionKind::Value if !arg.contains('=') => index += 2,
                _ => index += 1,
            }
            continue;
        }
        if arg == "resume" {
            return args
                .get(index + 1)
                .and_then(|value| value.to_str())
                .map(ExplicitResumeId::parse);
        }
        index += 1;
    }
    None
}

#[cfg(feature = "service")]
fn remote_attach_spec(
    paths: &ManagerPaths,
    real_codex: PathBuf,
    base_args: &[OsString],
    workspace: &Path,
    explicit_resume_id: Option<ExplicitResumeId>,
    clean_tui_launch: bool,
) -> Result<Option<CodexCommandSpec>> {
    if explicit_resume_id.is_none() && !clean_tui_launch {
        return Ok(None);
    }
    let server = serve::ready_app_server(paths).context("cx service app-server is not ready")?;
    if !remote_attach_supports_resume(server.kind, explicit_resume_id.as_ref()) {
        return Ok(None);
    }

    let mut client =
        AppServerClient::connect(&server.listen_url, std::time::Duration::from_secs(5))
            .context("connect ready app-server for remote attach")?;
    client.initialize("cx-tui", env!("CARGO_PKG_VERSION"))?;
    let scope = ThreadResolverScope {
        cwd: workspace.to_path_buf(),
        channel_id: None,
        explicit_resume_id,
        slot: server.app_slot(),
        generation: 0,
    };
    let outcome = thread_resolver::resolve_app_thread(paths, &mut client, scope)?;
    let thread_id = match outcome.decision {
        ThreadResolverDecision::AttachExisting { thread_id } => thread_id,
        ThreadResolverDecision::StartNew { .. } => outcome
            .thread_id
            .context("thread resolver started a thread without returning its id")?,
        ThreadResolverDecision::Refuse { reason } => anyhow::bail!("{reason}"),
    };

    Ok(Some(build_remote_tui_command_spec(
        paths, real_codex, &server, base_args, workspace, &thread_id,
    )?))
}

#[cfg(feature = "service")]
fn build_remote_tui_command_spec(
    paths: &ManagerPaths,
    real_codex: PathBuf,
    server: &serve::ReadyAppServer,
    base_args: &[OsString],
    workspace: &Path,
    thread_id: &str,
) -> Result<CodexCommandSpec> {
    let (codex_home, slot, launch_context) = remote_launch_home(paths, server);
    let envs = remote_tui_env(paths, launch_context, &codex_home)?;
    Ok(CodexCommandSpec {
        program: real_codex,
        codex_home,
        envs,
        args: remote_tui_args(base_args, &server.listen_url, workspace, thread_id),
        slot,
        target_name: server.target.clone(),
        launch_context,
    })
}

#[cfg(feature = "service")]
fn remote_attach_supports_resume(
    server_kind: ServeEndpointKind,
    explicit_resume_id: Option<&ExplicitResumeId>,
) -> bool {
    explicit_resume_id.is_none() || server_kind == ServeEndpointKind::Broker
}

#[cfg(feature = "service")]
fn remote_tui_env(
    paths: &ManagerPaths,
    launch_context: CodexLaunchContext,
    codex_home: &Path,
) -> Result<BTreeMap<String, String>> {
    let sqlite_home = match launch_context {
        CodexLaunchContext::Slot => codex_home.join("sqlite"),
        CodexLaunchContext::ServiceBroker => paths.remote_tui_sqlite_home(),
    };
    let mut envs = BTreeMap::new();
    insert_sqlite_home_env(&mut envs, sqlite_home)?;
    Ok(envs)
}

#[cfg(feature = "service")]
fn remote_launch_home(
    paths: &ManagerPaths,
    server: &serve::ReadyAppServer,
) -> (PathBuf, String, CodexLaunchContext) {
    match server.kind {
        ServeEndpointKind::AppServer => {
            let codex_home = server
                .codex_home
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| paths.slot_home(&server.slot));
            (codex_home, server.slot.clone(), CodexLaunchContext::Slot)
        }
        ServeEndpointKind::Broker => (
            paths.base_codex_home.clone(),
            String::from("service-broker"),
            CodexLaunchContext::ServiceBroker,
        ),
    }
}

fn insert_sqlite_home_env(envs: &mut BTreeMap<String, String>, sqlite_home: PathBuf) -> Result<()> {
    ensure_sqlite_home(&sqlite_home)?;
    envs.insert(
        CODEX_SQLITE_HOME.to_string(),
        sqlite_home.display().to_string(),
    );
    Ok(())
}

fn ensure_sqlite_home(sqlite_home: &Path) -> Result<()> {
    fs::create_dir_all(sqlite_home)
        .with_context(|| format!("create sqlite home {}", sqlite_home.display()))
}

#[cfg(feature = "service")]
fn remote_tui_args(
    base_args: &[OsString],
    app_server_url: &str,
    workspace: &Path,
    thread_id: &str,
) -> Vec<OsString> {
    let mut args = strip_resume_subcommand(base_args);
    args.push(OsString::from("--remote"));
    args.push(OsString::from(app_server_url));
    args.push(OsString::from("-C"));
    args.push(OsString::from(workspace.as_os_str()));
    args.push(OsString::from("resume"));
    args.push(OsString::from(thread_id));
    args
}

#[cfg(feature = "service")]
fn strip_resume_subcommand(base_args: &[OsString]) -> Vec<OsString> {
    let mut args = Vec::new();
    let mut index = 0;
    while index < base_args.len() {
        let arg = &base_args[index];
        let arg_text = arg.to_string_lossy();
        if arg_text.starts_with('-') {
            args.push(arg.clone());
            match codex_option_kind(arg_text.as_ref()) {
                CodexOptionKind::Value if !arg_text.contains('=') => {
                    if let Some(value) = base_args.get(index + 1) {
                        args.push(value.clone());
                    }
                    index += 2;
                }
                _ => index += 1,
            }
            continue;
        }
        if arg_text == "resume" {
            index += if base_args
                .get(index + 1)
                .is_some_and(|value| !value.to_string_lossy().starts_with('-'))
            {
                2
            } else {
                1
            };
            continue;
        }
        args.push(arg.clone());
        index += 1;
    }
    args
}

fn exec_real_codex(real_codex: &Path, args: Vec<OsString>) -> Result<()> {
    let mut command = Command::new(real_codex);
    command.args(args);
    exec(command)
}

fn exec(mut command: Command) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("exec codex")
    }
    #[cfg(not(unix))]
    {
        let status = command.status().context("run codex")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

pub(crate) fn resolve_codex_bin(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("CX_CODEX_BIN") {
        return Ok(PathBuf::from(path));
    }
    if let Ok(output) = Command::new("mise").arg("which").arg("codex").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }
    if let Some(path) = find_executable_on_path(Path::new("codex")) {
        return Ok(path);
    }
    for fallback in default_codex_bin_candidates()? {
        if fallback.is_file() {
            return Ok(fallback);
        }
    }
    Ok(PathBuf::from("codex"))
}

fn default_codex_bin_candidates() -> Result<Vec<PathBuf>> {
    let home = paths::home_dir()?;
    Ok(vec![
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
        home.join(".local/bin/codex"),
        home.join(".local/share/mise/installs/codex/0.125.0/codex"),
    ])
}

pub(crate) fn find_executable_on_path(command: &Path) -> Option<PathBuf> {
    find_executable_on_path_with_env(command, std::env::var_os("PATH").as_deref())
}

fn find_executable_on_path_with_env(command: &Path, path_env: Option<&OsStr>) -> Option<PathBuf> {
    if command.components().count() != 1 {
        return None;
    }
    let path_env = path_env?;
    std::env::split_paths(path_env)
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
}

fn first_forwarded_non_option(args: &[OsString]) -> Option<String> {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let arg_text = arg.to_string_lossy();
        match arg_text.as_ref() {
            "--" => return iter.next().map(|value| value.to_string_lossy().to_string()),
            "--slot" | "--target" | "--manager-dir" | "--codex-bin" | "-s" => {
                let _ = iter.next();
            }
            "--managed" | "--cx-quiet" | "--cx-debug" | "--cx-service-remote" => {}
            _ if arg_text.starts_with("--slot=")
                || arg_text.starts_with("--target=")
                || arg_text.starts_with("--manager-dir=")
                || arg_text.starts_with("--codex-bin=") => {}
            _ if arg_text.starts_with('-') => {}
            _ => return Some(arg_text.to_string()),
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexOptionKind {
    Flag,
    Value,
    Unknown,
}

pub(crate) fn codex_option_kind(arg: &str) -> CodexOptionKind {
    if arg.contains('=') {
        let name = arg.split_once('=').map(|(name, _)| name).unwrap_or(arg);
        return match name {
            "--config"
            | "--enable"
            | "--disable"
            | "--remote-auth-token-env"
            | "--image"
            | "--model"
            | "--local-provider"
            | "--profile"
            | "--sandbox"
            | "--cd"
            | "--add-dir"
            | "--ask-for-approval" => CodexOptionKind::Flag,
            _ => CodexOptionKind::Unknown,
        };
    }

    match arg {
        "--oss" | "--dangerously-bypass-approvals-and-sandbox" | "--search" | "--no-alt-screen" => {
            CodexOptionKind::Flag
        }
        "-c"
        | "--config"
        | "--enable"
        | "--disable"
        | "--remote-auth-token-env"
        | "-i"
        | "--image"
        | "-m"
        | "--model"
        | "--local-provider"
        | "-p"
        | "--profile"
        | "-s"
        | "--sandbox"
        | "--add-dir"
        | "-a"
        | "--ask-for-approval" => CodexOptionKind::Value,
        _ => CodexOptionKind::Unknown,
    }
}

#[cfg(feature = "service")]
fn resolve_workspace(workspace: Option<PathBuf>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("resolve current directory")?;
    let workspace = match workspace {
        Some(workspace) if workspace.is_absolute() => workspace,
        Some(workspace) => cwd.join(workspace),
        None => cwd,
    };
    match std::fs::canonicalize(&workspace) {
        Ok(path) => Ok(path),
        Err(_) => Ok(workspace),
    }
}

#[cfg(feature = "service")]
pub(crate) fn resolve_workspace_from_args(args: &[OsString]) -> Result<PathBuf> {
    let mut workspace = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let arg_text = arg.to_string_lossy();
        if matches!(arg_text.as_ref(), "-C" | "--cd") {
            index += 1;
            let value = args
                .get(index)
                .with_context(|| format!("{arg_text} requires a value"))?;
            workspace = Some(PathBuf::from(value));
            index += 1;
            continue;
        }
        if let Some(value) = arg_text.strip_prefix("--cd=") {
            workspace = Some(PathBuf::from(value));
            index += 1;
            continue;
        }
        index += 1;
    }
    resolve_workspace(workspace)
}

pub(crate) fn explicit_resume_id(args: &[OsString]) -> Option<ExplicitResumeId> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let arg_text = arg.to_string_lossy();
        if arg_text.starts_with('-') {
            match codex_option_kind(arg_text.as_ref()) {
                CodexOptionKind::Flag => {
                    index += 1;
                }
                CodexOptionKind::Value => {
                    index += 2;
                }
                CodexOptionKind::Unknown => {
                    index += 1;
                }
            }
            continue;
        }
        if arg_text == "resume" {
            return args
                .get(index + 1)
                .filter(|value| !value.to_string_lossy().starts_with('-'))
                .map(|value| ExplicitResumeId::parse(value.to_string_lossy().to_string()));
        }
        return None;
    }
    None
}

pub(crate) fn usage_timeout() -> f32 {
    std::env::var("CX_SLOT_USAGE_TIMEOUT")
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(2.0)
}

fn parse_run_args(args: Vec<OsString>) -> Result<RunOptions> {
    let matches = match launcher_command()
        .try_get_matches_from(std::iter::once(OsString::from("cx")).chain(args))
    {
        Ok(matches) => matches,
        Err(err) if matches!(err.kind(), ClapErrorKind::DisplayHelp) => {
            err.print().context("print launcher help")?;
            std::process::exit(0);
        }
        Err(err) => {
            return Err(anyhow::anyhow!(
                "{err}\nCodex arguments now require `--`; for example: `cx -- resume THREAD`."
            ));
        }
    };

    if matches.get_flag(ARG_MANAGED) {
        anyhow::bail!("{}", removed_managed_message());
    }

    Ok(RunOptions {
        slot: matches.get_one::<String>(ARG_SLOT).cloned(),
        target: matches.get_one::<String>(ARG_TARGET).cloned(),
        manager_dir: matches.get_one::<PathBuf>(ARG_MANAGER_DIR).cloned(),
        codex_bin: matches.get_one::<PathBuf>(ARG_CODEX_BIN).cloned(),
        quiet: matches.get_flag(ARG_CX_QUIET),
        debug: matches.get_flag(ARG_CX_DEBUG),
        #[cfg(feature = "service")]
        service_remote: matches.get_flag(ARG_CX_SERVICE_REMOTE),
        codex_args: matches
            .get_many::<OsString>(ARG_CODEX_ARGS)
            .map(|values| values.cloned().collect())
            .unwrap_or_default(),
    })
}

fn removed_managed_message() -> &'static str {
    #[cfg(feature = "service")]
    {
        "`cx --managed` was removed; service remote is experimental. Start the service with `cx service start --no-telegram`, then opt in with `cx --cx-service-remote`."
    }
    #[cfg(not(feature = "service"))]
    {
        "`cx --managed` was removed; service remote is not compiled into this build. Rebuild cx with `--features service` to use service remote."
    }
}

pub(crate) fn split_launcher_args(args: &[OsString]) -> Result<LauncherArgsSplit> {
    let options = parse_run_args(args.to_vec())?;
    let cx_args = args
        .iter()
        .position(|arg| arg == "--")
        .map_or_else(|| args.to_vec(), |index| args[..index].to_vec());
    Ok(LauncherArgsSplit {
        cx_args,
        codex_args: options.codex_args,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn test_paths(name: &str) -> ManagerPaths {
        let root = std::env::temp_dir().join(format!("cx-run-test-{name}-{}", std::process::id()));
        ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
    }

    #[test]
    fn slot_flag_is_removed_from_forwarded_args() {
        let options = parse_run_args(vec![
            OsString::from("--slot"),
            OsString::from("bus1"),
            OsString::from("--"),
            OsString::from("-m"),
            OsString::from("gpt-5.4"),
        ])
        .unwrap();

        assert_eq!(options.slot, Some("bus1".to_string()));
        assert_eq!(
            options.codex_args,
            vec![OsString::from("-m"), OsString::from("gpt-5.4")]
        );
    }

    #[test]
    fn target_flag_is_removed_from_forwarded_args() {
        let options = parse_run_args(vec![
            OsString::from("--target=research"),
            OsString::from("--"),
            OsString::from("-m"),
            OsString::from("gpt-5.5"),
        ])
        .unwrap();

        assert_eq!(options.target, Some("research".to_string()));
        assert_eq!(
            options.codex_args,
            vec![OsString::from("-m"), OsString::from("gpt-5.5")]
        );
    }

    #[cfg(feature = "service")]
    #[test]
    fn service_remote_is_skipped_for_explicit_slot() {
        let options =
            parse_run_args(vec![OsString::from("--slot"), OsString::from("deepseek")]).unwrap();

        assert_eq!(options.slot, Some(String::from("deepseek")));
        assert!(!should_try_service_remote(&options));
    }

    #[cfg(feature = "service")]
    #[test]
    fn service_remote_is_skipped_for_explicit_target() {
        let options =
            parse_run_args(vec![OsString::from("--target"), OsString::from("research")]).unwrap();

        assert_eq!(options.target, Some(String::from("research")));
        assert!(!should_try_service_remote(&options));
    }

    #[cfg(feature = "service")]
    #[test]
    fn service_remote_is_skipped_for_plain_launch_by_default() {
        let options = parse_run_args(Vec::new()).unwrap();

        assert!(!options.service_remote);
        assert!(!should_try_service_remote(&options));
    }

    #[cfg(feature = "service")]
    #[test]
    fn service_remote_requires_explicit_experimental_opt_in() {
        let options = parse_run_args(vec![OsString::from("--cx-service-remote")]).unwrap();

        assert!(options.service_remote);
        assert!(should_try_service_remote(&options));
    }

    #[test]
    fn managed_flag_is_rejected() {
        let err = parse_run_args(vec![OsString::from("--managed")]).unwrap_err();

        assert!(format!("{err:#}").contains("was removed"));
    }

    #[test]
    fn app_server_subcommand_skips_stdin_wrapper() {
        assert!(should_skip_stdin_wrapper(&[
            OsString::from("--slot"),
            OsString::from("bus1"),
            OsString::from("--"),
            OsString::from("app-server"),
            OsString::from("--help"),
        ]));
    }

    #[test]
    fn removed_managed_flag_skips_stdin_wrapper_for_error_reporting() {
        assert!(should_skip_stdin_wrapper(&[
            OsString::from("--managed"),
            OsString::from("resume"),
            OsString::from("sid"),
        ]));
    }

    #[test]
    fn version_flag_skips_stdin_wrapper() {
        assert!(should_skip_stdin_wrapper(&[
            OsString::from("--"),
            OsString::from("--version")
        ]));
        assert!(should_skip_stdin_wrapper(&[
            OsString::from("--"),
            OsString::from("-V")
        ]));
    }

    #[test]
    fn sandbox_short_flag_is_forwarded() {
        let options = parse_run_args(vec![
            OsString::from("--"),
            OsString::from("-s"),
            OsString::from("workspace-write"),
            OsString::from("hello"),
        ])
        .unwrap();

        assert_eq!(options.slot, None);
        assert_eq!(
            options.codex_args,
            vec![
                OsString::from("-s"),
                OsString::from("workspace-write"),
                OsString::from("hello")
            ]
        );
    }

    #[test]
    fn codex_args_require_separator() {
        let err =
            parse_run_args(vec![OsString::from("resume"), OsString::from("thread-1")]).unwrap_err();

        assert!(format!("{err:#}").contains("Codex arguments now require `--`"));
    }

    #[test]
    fn launcher_split_preserves_cx_options_for_stdin_relaunch() {
        let split = split_launcher_args(&[
            OsString::from("--slot"),
            OsString::from("dia1"),
            OsString::from("--"),
            OsString::from("summarize"),
        ])
        .unwrap();

        assert_eq!(
            split.cx_args,
            vec![OsString::from("--slot"), OsString::from("dia1")]
        );
        assert_eq!(split.codex_args, vec![OsString::from("summarize")]);
    }

    #[test]
    fn explicit_resume_id_skips_root_overrides() {
        let args = vec![
            OsString::from("-c"),
            OsString::from("model=\"gpt-5.5\""),
            OsString::from("resume"),
            OsString::from("019dfdd3-debc-7da2-88fc-b15b73f5e138"),
        ];

        assert_eq!(
            explicit_resume_id(&args),
            Some(ExplicitResumeId::AppThreadOrCodexSession(String::from(
                "019dfdd3-debc-7da2-88fc-b15b73f5e138"
            )))
        );
    }

    #[cfg(feature = "service")]
    #[test]
    fn remote_tui_args_injects_remote_cd_and_resume() {
        let args = remote_tui_args(
            &[OsString::from("-c"), OsString::from("model=\"gpt-5.5\"")],
            "ws://127.0.0.1:17654",
            Path::new("/tmp/project"),
            "thread-1",
        );

        assert_eq!(
            args,
            vec![
                OsString::from("-c"),
                OsString::from("model=\"gpt-5.5\""),
                OsString::from("--remote"),
                OsString::from("ws://127.0.0.1:17654"),
                OsString::from("-C"),
                OsString::from("/tmp/project"),
                OsString::from("resume"),
                OsString::from("thread-1"),
            ]
        );
    }

    #[cfg(feature = "service")]
    #[test]
    fn remote_tui_args_replaces_explicit_resume() {
        let args = remote_tui_args(
            &[OsString::from("resume"), OsString::from("old-thread")],
            "ws://127.0.0.1:17654",
            Path::new("/tmp/project"),
            "new-thread",
        );

        assert_eq!(
            args,
            vec![
                OsString::from("--remote"),
                OsString::from("ws://127.0.0.1:17654"),
                OsString::from("-C"),
                OsString::from("/tmp/project"),
                OsString::from("resume"),
                OsString::from("new-thread"),
            ]
        );
    }

    #[cfg(feature = "service")]
    #[test]
    fn remote_resume_id_skips_root_options() {
        let args = vec![
            OsString::from("--remote"),
            OsString::from("ws://127.0.0.1:17654"),
            OsString::from("-C"),
            OsString::from("/tmp/project"),
            OsString::from("resume"),
            OsString::from("thread-1"),
        ];

        assert_eq!(
            remote_arg_value(&args, "--remote"),
            Some("ws://127.0.0.1:17654")
        );
        assert_eq!(
            remote_resume_id(&args),
            Some(ExplicitResumeId::AppThreadOrCodexSession(
                "thread-1".to_string()
            ))
        );
    }

    #[test]
    fn slot_command_spec_injects_slot_sqlite_home() {
        let paths = test_paths("slot-sqlite-home");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        fs::create_dir_all(paths.slot_home("dia4")).unwrap();
        fs::create_dir_all(paths.slot_dir("dia4")).unwrap();
        fs::write(
            paths.slot_dir("dia4").join("env.conf"),
            "CODEX_SQLITE_HOME=/tmp/wrong\n",
        )
        .unwrap();

        let spec = build_slot_command_spec(
            &paths,
            PathBuf::from("/bin/codex"),
            "dia4",
            None,
            Vec::new(),
        )
        .unwrap();
        let sqlite_home = spec.envs.get(CODEX_SQLITE_HOME).unwrap();

        assert_eq!(PathBuf::from(sqlite_home), paths.slot_sqlite_home("dia4"));
        assert_ne!(PathBuf::from(sqlite_home), paths.base_codex_home);
        assert!(paths.slot_sqlite_home("dia4").is_dir());

        let _ = fs::remove_dir_all(&paths.manager_dir);
        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }

    #[cfg(feature = "service")]
    #[test]
    fn broker_remote_launch_uses_base_home_not_broker_slot_home() {
        let paths = test_paths("broker-home");
        let server = serve::ReadyAppServer {
            kind: ServeEndpointKind::Broker,
            listen_url: String::from("ws://127.0.0.1:17654"),
            slot: String::from("broker"),
            codex_home: None,
            target: None,
        };

        let (codex_home, slot, launch_context) = remote_launch_home(&paths, &server);

        assert_eq!(codex_home, paths.base_codex_home);
        assert_ne!(codex_home, paths.slot_home("broker"));
        assert_eq!(slot, "service-broker");
        assert_eq!(launch_context, CodexLaunchContext::ServiceBroker);
        assert_eq!(server.app_slot(), None);
    }

    #[cfg(feature = "service")]
    #[test]
    fn broker_remote_tui_command_spec_injects_client_sqlite_home() {
        let paths = test_paths("broker-remote-sqlite-home");
        let server = serve::ReadyAppServer {
            kind: ServeEndpointKind::Broker,
            listen_url: String::from("ws://127.0.0.1:17654"),
            slot: String::from("broker"),
            codex_home: None,
            target: None,
        };

        let spec = build_remote_tui_command_spec(
            &paths,
            PathBuf::from("/bin/codex"),
            &server,
            &[],
            Path::new("/tmp/project"),
            "thread-1",
        )
        .unwrap();
        let sqlite_home = PathBuf::from(spec.envs.get(CODEX_SQLITE_HOME).unwrap());

        assert_eq!(spec.codex_home, paths.base_codex_home);
        assert_eq!(spec.slot, "service-broker");
        assert_eq!(spec.launch_context, CodexLaunchContext::ServiceBroker);
        assert_ne!(sqlite_home, paths.base_codex_home);
        assert!(sqlite_home.starts_with(paths.serve_dir()));
        assert!(sqlite_home.is_dir());

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[cfg(feature = "service")]
    #[test]
    fn app_server_remote_launch_uses_recorded_slot_home() {
        let paths = test_paths("app-server-home");
        let server = serve::ReadyAppServer {
            kind: ServeEndpointKind::AppServer,
            listen_url: String::from("ws://127.0.0.1:17654"),
            slot: String::from("dia4"),
            codex_home: None,
            target: None,
        };

        let (codex_home, slot, launch_context) = remote_launch_home(&paths, &server);

        assert_eq!(codex_home, paths.slot_home("dia4"));
        assert_eq!(slot, "dia4");
        assert_eq!(launch_context, CodexLaunchContext::Slot);
        assert_eq!(server.app_slot(), Some(String::from("dia4")));
    }

    #[cfg(feature = "service")]
    #[test]
    fn explicit_resume_requires_broker_remote() {
        let resume_id = ExplicitResumeId::AppThreadOrCodexSession(String::from("thread-1"));

        assert!(!remote_attach_supports_resume(
            ServeEndpointKind::AppServer,
            Some(&resume_id)
        ));
        assert!(remote_attach_supports_resume(
            ServeEndpointKind::Broker,
            Some(&resume_id)
        ));
        assert!(remote_attach_supports_resume(
            ServeEndpointKind::AppServer,
            None
        ));
    }

    #[test]
    fn find_executable_on_path_uses_absolute_path_entries() {
        let root = std::env::temp_dir().join(format!("cx-path-test-{}", std::process::id()));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("codex");
        std::fs::write(&executable, "").unwrap();
        let path_env = std::env::join_paths([bin]).unwrap();

        assert_eq!(
            find_executable_on_path_with_env(Path::new("codex"), Some(path_env.as_os_str())),
            Some(executable)
        );
        assert_eq!(
            find_executable_on_path_with_env(Path::new("bin/codex"), Some(path_env.as_os_str())),
            None
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
