use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use anyhow::Result;
use clap::builder::OsStringValueParser;
use clap::error::ErrorKind as ClapErrorKind;
use clap::value_parser;
use clap::Arg;
use clap::ArgAction;
use clap::Command as ClapCommand;
use clap::ValueHint;

use crate::cli::LoginArgs;
use crate::envfile;
use crate::paths;
use crate::paths::ManagerPaths;
use crate::resume_id::ExplicitResumeId;
use crate::selector;
use crate::slot;
use crate::target::TargetSpec;
use crate::terminal_resume;
use crate::terminal_resume::ResumeCandidate;
use crate::terminal_resume::TerminalKey;

const BYPASS_SUBCOMMANDS: &[&str] = &[
    "login",
    "logout",
    "completion",
    "plugin",
    "mcp",
    "app",
    "sandbox",
    "debug",
    "apply",
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
const ARG_NEW_SESSION: &str = "new";
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
        )
        .arg(
            Arg::new(ARG_NEW_SESSION)
                .long(ARG_NEW_SESSION)
                .action(ArgAction::SetTrue)
                .help("Start a fresh Codex session instead of auto-resuming this terminal"),
        );
    command.arg(
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
    new_session: bool,
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
struct TerminalResumeLaunch {
    cwd: PathBuf,
    key: Option<TerminalKey>,
    auto_candidate: Option<ResumeCandidate>,
    direct_launch: bool,
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
    crate::upgrade::run_startup(&paths)?;
    let real_codex = resolve_codex_bin(options.codex_bin.as_deref())?;
    let launch_cwd = std::env::current_dir().context("resolve current directory")?;
    let direct_launch = options.codex_args.is_empty();
    let explicit_resume = explicit_resume_id(&options.codex_args);
    let terminal_key = (direct_launch || explicit_resume.is_some())
        .then(terminal_resume::current_terminal_key)
        .flatten();
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

    let auto_resume_state_hint = if direct_launch
        && !options.new_session
        && std::env::var_os("CX_NO_AUTO_RESUME").is_none()
        && options.target.is_none()
    {
        terminal_key
            .as_ref()
            .map(|key| terminal_resume::load_resume_state(&paths, key, &launch_cwd))
            .transpose()?
            .flatten()
    } else {
        None
    };
    let auto_resume_candidate = if direct_launch
        && !options.new_session
        && std::env::var_os("CX_NO_AUTO_RESUME").is_none()
        && options.target.is_none()
    {
        auto_resume_candidate_for_launch(&paths, &launch_cwd, auto_resume_state_hint.as_ref())?
    } else {
        None
    };

    let target = crate::target::load_optional_target(&paths, options.target.as_deref())?;
    let candidates = if let Some(target) = &target {
        target.slots_or_rotation(&paths)?
    } else {
        slot::load_rotation(&paths)?
    };
    if candidates.is_empty() && options.slot.is_none() {
        let mut codex_args = options.codex_args;
        append_auto_resume_args_if_missing(
            &mut codex_args,
            auto_resume_candidate
                .as_ref()
                .map(|candidate| candidate.session_id.as_str()),
        );
        return exec_real_codex(&real_codex, codex_args);
    }

    let selected_slot = if let Some(slot) = options.slot.clone() {
        slot
    } else {
        let command_progress = crate::output::CommandProgress::for_human_output(false);
        let mut progress = command_progress.slot_query("checking slots");
        let results = selector::query_slots_with_progress(
            &paths,
            &candidates,
            usage_query_options(),
            &mut progress,
        )?;
        let selected = choose_launch_result(&results);
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
        TerminalResumeLaunch {
            cwd: launch_cwd,
            key: terminal_key,
            auto_candidate: auto_resume_candidate,
            direct_launch,
        },
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

pub(crate) fn select_runtime_with_progress<P: selector::SlotQueryProgress>(
    paths: &ManagerPaths,
    slot: Option<&str>,
    target_name: Option<&str>,
    debug: bool,
    progress: &mut P,
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

    let results =
        selector::query_slots_with_progress(paths, &candidates, usage_query_options(), progress)?;
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
    resume_launch: TerminalResumeLaunch,
) -> Result<()> {
    let mut spec = build_slot_command_spec(
        paths,
        real_codex.to_path_buf(),
        selected_slot,
        target,
        options.codex_args,
    )?;
    let explicit_resume = explicit_resume_id(&spec.args);
    let resumed_id = append_auto_resume_args_if_missing(
        &mut spec.args,
        resume_launch
            .auto_candidate
            .as_ref()
            .map(|candidate| candidate.session_id.as_str()),
    );

    if let (Some(key), Some(resume_id)) = (resume_launch.key.as_ref(), explicit_resume.as_ref()) {
        let _ = terminal_resume::record_resume_state(
            paths,
            key,
            selected_slot,
            &resume_launch.cwd,
            resume_id.as_str(),
            None,
        );
    }

    if resume_launch.direct_launch || explicit_resume.is_some() {
        if let Some(key) = resume_launch.key.as_ref() {
            let launch_started_unix_ms = terminal_resume::now_unix_ms();
            let _ = terminal_resume::spawn_session_watcher(
                paths,
                &spec.codex_home,
                selected_slot,
                &resume_launch.cwd,
                key,
                launch_started_unix_ms,
                std::process::id(),
            );
        }
    }

    print_launch(
        &spec,
        resumed_id.as_ref().map(ExplicitResumeId::as_str),
        options.quiet,
    );
    exec(spec.into_command())
}

fn append_resume_args(args: &mut Vec<OsString>, session_id: &str) {
    args.push(OsString::from("resume"));
    args.push(OsString::from(session_id));
}

fn append_auto_resume_args_if_missing(
    args: &mut Vec<OsString>,
    auto_session_id: Option<&str>,
) -> Option<ExplicitResumeId> {
    if let Some(resume_id) = explicit_resume_id(args) {
        return Some(resume_id);
    }
    let session_id = auto_session_id?;
    append_resume_args(args, session_id);
    Some(ExplicitResumeId::parse(session_id))
}

fn auto_resume_candidate_for_launch(
    paths: &ManagerPaths,
    cwd: &Path,
    hint: Option<&terminal_resume::ResumeState>,
) -> Result<Option<ResumeCandidate>> {
    if terminal_resume::has_active_session_in_cwd(paths, cwd) {
        return Ok(None);
    }
    terminal_resume::latest_cwd_resume_candidate(paths, cwd, hint)
}

fn choose_launch_result(results: &[crate::usage::SlotResult]) -> Option<&crate::usage::SlotResult> {
    selector::choose_result(results)
}

fn print_launch(spec: &CodexCommandSpec, resumed_id: Option<&str>, quiet: bool) {
    if quiet {
        return;
    }
    match spec.launch_context {
        CodexLaunchContext::Slot => eprintln!("codex slot: {}", spec.slot()),
    }
    if let Some(target) = spec.target_name() {
        eprintln!("codex target: {target}");
    }
    if let Some(resumed_id) = resumed_id {
        eprintln!("codex resume: {resumed_id}");
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
            "--cx-quiet" | "--cx-debug" => {}
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

fn usage_query_options() -> selector::SlotQueryOptions {
    let jobs = std::env::var("CX_SLOT_USAGE_JOBS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(selector::DEFAULT_SLOT_QUERY_JOBS);
    let retries = std::env::var("CX_SLOT_USAGE_RETRIES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(selector::DEFAULT_SLOT_QUERY_RETRIES);
    selector::SlotQueryOptions::new(usage_timeout(), jobs, retries)
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

    Ok(RunOptions {
        slot: matches.get_one::<String>(ARG_SLOT).cloned(),
        target: matches.get_one::<String>(ARG_TARGET).cloned(),
        manager_dir: matches.get_one::<PathBuf>(ARG_MANAGER_DIR).cloned(),
        codex_bin: matches.get_one::<PathBuf>(ARG_CODEX_BIN).cloned(),
        quiet: matches.get_flag(ARG_CX_QUIET),
        debug: matches.get_flag(ARG_CX_DEBUG),
        new_session: matches.get_flag(ARG_NEW_SESSION),
        codex_args: matches
            .get_many::<OsString>(ARG_CODEX_ARGS)
            .map(|values| values.cloned().collect())
            .unwrap_or_default(),
    })
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
    fn new_flag_disables_launcher_auto_resume_policy() {
        let options = parse_run_args(vec![OsString::from("--new")]).unwrap();

        assert!(options.new_session);
        assert!(options.codex_args.is_empty());
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

    #[test]
    fn auto_resume_args_append_session_without_slot_gate() {
        let mut args = vec![OsString::from("-m"), OsString::from("gpt-5.5")];

        let resumed = append_auto_resume_args_if_missing(&mut args, Some("thread-1"));

        assert_eq!(
            resumed,
            Some(ExplicitResumeId::AppThreadOrCodexSession(String::from(
                "thread-1"
            )))
        );
        assert_eq!(
            args,
            vec![
                OsString::from("-m"),
                OsString::from("gpt-5.5"),
                OsString::from("resume"),
                OsString::from("thread-1"),
            ]
        );
    }

    #[test]
    fn explicit_resume_args_win_over_auto_resume() {
        let mut args = vec![OsString::from("resume"), OsString::from("thread-explicit")];

        let resumed = append_auto_resume_args_if_missing(&mut args, Some("thread-auto"));

        assert_eq!(
            resumed,
            Some(ExplicitResumeId::AppThreadOrCodexSession(String::from(
                "thread-explicit"
            )))
        );
        assert_eq!(
            args,
            vec![OsString::from("resume"), OsString::from("thread-explicit"),]
        );
    }

    #[test]
    fn auto_resume_does_not_bias_slot_selection() {
        let state = crate::terminal_resume::test_resume_state(
            "dia7",
            PathBuf::from("/tmp/cx-run-test-project"),
        );
        let results = vec![
            crate::usage::SlotResult::new(
                "bus1",
                0,
                crate::usage::SlotStatus::Available,
                95.0,
                "fresh",
            ),
            crate::usage::SlotResult::new(
                "dia7",
                1,
                crate::usage::SlotStatus::Available,
                10.0,
                "resume",
            ),
        ];

        assert_eq!(
            choose_launch_result(&results).map(|result| result.slot.as_str()),
            Some("bus1")
        );
        assert_eq!(state.slot, "dia7");
    }

    #[test]
    fn auto_resume_args_survive_rotated_selected_slot() {
        let state = crate::terminal_resume::test_resume_state(
            "dia7",
            PathBuf::from("/tmp/cx-run-test-project"),
        );
        let results = vec![
            crate::usage::SlotResult::new(
                "dia7",
                0,
                crate::usage::SlotStatus::Available,
                10.0,
                "resume",
            ),
            crate::usage::SlotResult::new(
                "bus1",
                1,
                crate::usage::SlotStatus::Available,
                80.0,
                "fresh",
            ),
        ];

        assert_eq!(
            choose_launch_result(&results).map(|result| result.slot.as_str()),
            Some("bus1")
        );
        let mut args = Vec::new();

        let resumed =
            append_auto_resume_args_if_missing(&mut args, Some(state.session_id.as_str()));

        assert_eq!(
            resumed,
            Some(ExplicitResumeId::AppThreadOrCodexSession(String::from(
                "session-1"
            )))
        );
        assert_eq!(
            args,
            vec![OsString::from("resume"), OsString::from("session-1")]
        );
    }

    #[test]
    fn active_session_blocks_auto_resume_candidate() {
        let paths = test_paths("auto-resume-active-block");
        let cwd = paths.base_codex_home.join("project");
        let state = crate::terminal_resume::test_resume_state("dia7", cwd.clone());
        let watch_request =
            crate::terminal_resume::test_write_watch_request(&paths, &cwd, std::process::id())
                .unwrap();

        assert!(auto_resume_candidate_for_launch(&paths, &cwd, Some(&state))
            .unwrap()
            .is_none());

        let _ = fs::remove_file(watch_request);
        let _ = fs::remove_dir_all(&paths.manager_dir);
        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }

    #[test]
    fn active_session_in_other_cwd_does_not_block_auto_resume_candidate() {
        let paths = test_paths("auto-resume-other-cwd");
        let cwd = paths.base_codex_home.join("project");
        let other_cwd = paths.base_codex_home.join("other-project");
        let state = crate::terminal_resume::test_resume_state("dia7", cwd.clone());
        let watch_request = crate::terminal_resume::test_write_watch_request(
            &paths,
            &other_cwd,
            std::process::id(),
        )
        .unwrap();

        let candidate = auto_resume_candidate_for_launch(&paths, &cwd, Some(&state))
            .unwrap()
            .unwrap();

        assert_eq!(candidate.slot, "dia7");
        assert_eq!(candidate.session_id, "session-1");

        let _ = fs::remove_file(watch_request);
        let _ = fs::remove_dir_all(&paths.manager_dir);
        let _ = fs::remove_dir_all(&paths.base_codex_home);
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
