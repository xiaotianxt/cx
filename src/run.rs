use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use anyhow::Result;

use crate::autoresume;
use crate::cli::LoginArgs;
use crate::envfile;
use crate::paths;
use crate::paths::ManagerPaths;
use crate::selector;
use crate::slot;
use crate::target::TargetSpec;

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

#[derive(Debug, Clone, Default, PartialEq)]
struct RunOptions {
    slot: Option<String>,
    target: Option<String>,
    manager_dir: Option<PathBuf>,
    codex_bin: Option<PathBuf>,
    quiet: bool,
    debug: bool,
    managed: bool,
    codex_args: Vec<OsString>,
    first_non_option: Option<String>,
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
        && !options.managed
    {
        return exec_real_codex(&real_codex, options.codex_args);
    }

    if options.slot.is_none()
        && options.target.is_none()
        && options
            .first_non_option
            .as_deref()
            .is_some_and(|arg| BYPASS_SUBCOMMANDS.contains(&arg))
    {
        return exec_real_codex(&real_codex, options.codex_args);
    }

    let target = crate::target::load_optional_target(&paths, options.target.as_deref())?;
    let candidates = if let Some(target) = &target {
        target.slots_or_rotation(&paths)?
    } else {
        slot::load_rotation(&paths)?
    };
    if candidates.is_empty() && options.slot.is_none() {
        if options.managed {
            anyhow::bail!("cx --managed requires at least one configured slot or --slot");
        }
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
        candidates,
        options,
    )
}

pub fn exec_slot_login(paths: &ManagerPaths, args: LoginArgs) -> Result<()> {
    let real_codex = resolve_codex_bin(args.codex_bin.as_deref())?;
    let slot_home = paths.slot_home(&args.slot);
    let mut command = Command::new(real_codex);
    command.env("CODEX_HOME", slot_home);
    command.arg("login");
    command.args(args.args);
    exec(command)
}

pub(crate) fn should_skip_stdin_wrapper(args: &[OsString]) -> bool {
    if args.iter().any(|arg| arg == "--managed") {
        return true;
    }
    if first_forwarded_non_option(args).is_none()
        && args
            .iter()
            .any(|arg| matches!(arg.to_str(), Some("-V" | "--version")))
    {
        return true;
    }
    first_forwarded_non_option(args).is_some_and(|arg| BYPASS_SUBCOMMANDS.contains(&arg.as_str()))
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
    })
}

fn exec_slot_codex(
    paths: &ManagerPaths,
    real_codex: &Path,
    selected_slot: &str,
    target: Option<&TargetSpec>,
    candidates: Vec<String>,
    options: RunOptions,
) -> Result<()> {
    let should_consider_auto_resume = options.codex_args.is_empty();
    let original_codex_args = options.codex_args.clone();
    let mut spec = build_slot_command_spec(
        paths,
        real_codex.to_path_buf(),
        selected_slot,
        target,
        options.codex_args,
    )?;

    let workspace = resolve_workspace_from_args(&spec.args)?;
    let mut resumed_session_id = explicit_resume_session_id(&spec.args);
    if should_consider_auto_resume {
        if let Some(plan) = auto_resume_plan(&spec.args)? {
            if let Some(session_id) =
                autoresume::inactive_latest_session_id(&spec.codex_home, &plan.workspace)
            {
                insert_resume_args(&mut spec.args, plan.insert_index, &session_id);
                resumed_session_id = Some(session_id);
            }
        }
    }

    if !options.quiet {
        eprintln!("codex slot: {}", spec.slot());
        if let Some(target) = spec.target_name() {
            eprintln!("codex target: {target}");
        }
        if let Some(session_id) = &resumed_session_id {
            eprintln!("codex resume: {session_id}");
        }
    }

    if options.managed {
        return crate::supervisor::run(crate::supervisor::SupervisorLaunch {
            paths: paths.clone(),
            real_codex: real_codex.to_path_buf(),
            target: target.cloned(),
            candidates,
            selected_slot: selected_slot.to_string(),
            original_codex_args,
            initial_spec: spec,
            session_id: resumed_session_id,
            workspace,
            quiet: options.quiet,
            debug: options.debug,
        });
    }

    exec(spec.into_command())
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
    let fallback = paths::home_dir()?.join(".local/share/mise/installs/codex/0.125.0/codex");
    if fallback.exists() {
        return Ok(fallback);
    }
    Ok(PathBuf::from("codex"))
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
            "--managed" | "--cx-quiet" | "--cx-debug" => {}
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct AutoResumePlan {
    insert_index: usize,
    workspace: PathBuf,
}

fn auto_resume_plan(args: &[OsString]) -> Result<Option<AutoResumePlan>> {
    let mut workspace = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let arg_text = arg.to_string_lossy();
        if matches!(arg_text.as_ref(), "-h" | "--help" | "-V" | "--version") {
            return Ok(None);
        }
        if arg_text == "--remote" {
            return Ok(None);
        }
        if arg_text.starts_with("--remote=") {
            return Ok(None);
        }
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
        if arg_text.starts_with('-') {
            match codex_option_kind(arg_text.as_ref()) {
                CodexOptionKind::Flag => {
                    index += 1;
                    continue;
                }
                CodexOptionKind::Value => {
                    if args.get(index + 1).is_none() {
                        return Ok(None);
                    }
                    index += 2;
                    continue;
                }
                CodexOptionKind::Unknown => return Ok(None),
            }
        }
        if is_codex_subcommand(arg_text.as_ref()) {
            return Ok(None);
        }
        return Ok(None);
    }

    Ok(Some(AutoResumePlan {
        insert_index: args.len(),
        workspace: resolve_workspace(workspace)?,
    }))
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

pub(crate) fn is_codex_subcommand(arg: &str) -> bool {
    matches!(
        arg,
        "exec"
            | "e"
            | "review"
            | "login"
            | "logout"
            | "mcp"
            | "plugin"
            | "mcp-server"
            | "app-server"
            | "app"
            | "completion"
            | "update"
            | "sandbox"
            | "debug"
            | "apply"
            | "a"
            | "resume"
            | "fork"
            | "cloud"
            | "exec-server"
            | "features"
            | "help"
    )
}

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

pub(crate) fn explicit_resume_session_id(args: &[OsString]) -> Option<String> {
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
                .map(|value| value.to_string_lossy().to_string());
        }
        return None;
    }
    None
}

fn insert_resume_args(args: &mut Vec<OsString>, insert_index: usize, session_id: &str) {
    args.insert(insert_index, OsString::from("resume"));
    args.insert(insert_index + 1, OsString::from(session_id));
}

pub(crate) fn usage_timeout() -> f32 {
    std::env::var("CX_SLOT_USAGE_TIMEOUT")
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(2.0)
}

fn parse_run_args(args: Vec<OsString>) -> Result<RunOptions> {
    let mut options = RunOptions::default();
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        let arg_text = arg.to_string_lossy();
        match arg_text.as_ref() {
            "--" => {
                options.codex_args.extend(iter);
                break;
            }
            "--slot" => {
                let value = next_value(&mut iter, "--slot")?;
                options.slot = Some(value);
            }
            "--target" => {
                let value = next_value(&mut iter, "--target")?;
                options.target = Some(value);
            }
            "--manager-dir" => {
                let value = next_os_value(&mut iter, "--manager-dir")?;
                options.manager_dir = Some(PathBuf::from(value));
            }
            "--codex-bin" => {
                let value = next_os_value(&mut iter, "--codex-bin")?;
                options.codex_bin = Some(PathBuf::from(value));
            }
            "--cx-quiet" => {
                options.quiet = true;
            }
            "--cx-debug" => {
                options.debug = true;
            }
            "--managed" => {
                options.managed = true;
            }
            "-s" => {
                if let Some(next) = iter.peek() {
                    let value = next.to_string_lossy();
                    if matches!(
                        value.as_ref(),
                        "read-only" | "workspace-write" | "danger-full-access"
                    ) {
                        options.codex_args.push(arg);
                        options
                            .codex_args
                            .push(iter.next().expect("peeked value exists"));
                    } else {
                        options.slot = Some(next_value(&mut iter, "-s")?);
                    }
                } else {
                    options.codex_args.push(arg);
                }
            }
            _ if arg_text.starts_with("--slot=") => {
                options.slot = Some(arg_text["--slot=".len()..].to_string());
            }
            _ if arg_text.starts_with("--target=") => {
                options.target = Some(arg_text["--target=".len()..].to_string());
            }
            _ if arg_text.starts_with("--manager-dir=") => {
                options.manager_dir = Some(PathBuf::from(&arg_text["--manager-dir=".len()..]));
            }
            _ if arg_text.starts_with("--codex-bin=") => {
                options.codex_bin = Some(PathBuf::from(&arg_text["--codex-bin=".len()..]));
            }
            _ => {
                if options.first_non_option.is_none() && !arg_text.starts_with('-') {
                    options.first_non_option = Some(arg_text.to_string());
                }
                options.codex_args.push(arg);
            }
        }
    }
    Ok(options)
}

fn next_value(
    iter: &mut std::iter::Peekable<impl Iterator<Item = OsString>>,
    flag: &str,
) -> Result<String> {
    let value = next_os_value(iter, flag)?;
    value
        .into_string()
        .map_err(|_| anyhow::anyhow!("{flag} requires UTF-8 value"))
}

fn next_os_value(
    iter: &mut std::iter::Peekable<impl Iterator<Item = OsString>>,
    flag: &str,
) -> Result<OsString> {
    iter.next()
        .with_context(|| format!("{flag} requires a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_flag_is_removed_from_forwarded_args() {
        let options = parse_run_args(vec![
            OsString::from("--slot"),
            OsString::from("bus1"),
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
    fn managed_flag_is_removed_from_forwarded_args() {
        let options = parse_run_args(vec![
            OsString::from("--managed"),
            OsString::from("-m"),
            OsString::from("gpt-5.5"),
        ])
        .unwrap();

        assert!(options.managed);
        assert_eq!(
            options.codex_args,
            vec![OsString::from("-m"), OsString::from("gpt-5.5")]
        );
    }

    #[test]
    fn app_server_subcommand_skips_stdin_wrapper() {
        assert!(should_skip_stdin_wrapper(&[
            OsString::from("--slot"),
            OsString::from("bus1"),
            OsString::from("app-server"),
            OsString::from("--help"),
        ]));
    }

    #[test]
    fn managed_launch_skips_stdin_wrapper() {
        assert!(should_skip_stdin_wrapper(&[
            OsString::from("--managed"),
            OsString::from("resume"),
            OsString::from("sid"),
        ]));
    }

    #[test]
    fn version_flag_skips_stdin_wrapper() {
        assert!(should_skip_stdin_wrapper(&[OsString::from("--version")]));
        assert!(should_skip_stdin_wrapper(&[OsString::from("-V")]));
    }

    #[test]
    fn sandbox_short_flag_is_forwarded() {
        let options = parse_run_args(vec![
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
    fn auto_resume_plan_skips_prompt() {
        let args = vec![
            OsString::from("-m"),
            OsString::from("gpt-5.5"),
            OsString::from("continue"),
        ];

        let plan = auto_resume_plan(&args).unwrap();

        assert_eq!(plan, None);
    }

    #[test]
    fn explicit_resume_session_id_skips_root_overrides() {
        let args = vec![
            OsString::from("-c"),
            OsString::from("model=\"gpt-5.5\""),
            OsString::from("resume"),
            OsString::from("019dfdd3-debc-7da2-88fc-b15b73f5e138"),
        ];

        assert_eq!(
            explicit_resume_session_id(&args),
            Some(String::from("019dfdd3-debc-7da2-88fc-b15b73f5e138"))
        );
    }

    #[test]
    fn auto_resume_plan_uses_current_workspace_for_clean_launch() {
        let cwd = std::env::current_dir().unwrap();
        let args = Vec::new();

        let plan = auto_resume_plan(&args).unwrap().unwrap();

        assert_eq!(plan.insert_index, 0);
        assert_eq!(plan.workspace, cwd);
    }

    #[test]
    fn auto_resume_plan_skips_explicit_subcommands() {
        let args = vec![OsString::from("resume"), OsString::from("--last")];

        let plan = auto_resume_plan(&args).unwrap();

        assert_eq!(plan, None);
    }

    #[test]
    fn auto_resume_plan_skips_remote_and_help() {
        assert_eq!(
            auto_resume_plan(&[
                OsString::from("--remote"),
                OsString::from("ws://127.0.0.1:1")
            ])
            .unwrap(),
            None
        );
        assert_eq!(auto_resume_plan(&[OsString::from("-h")]).unwrap(), None);
    }

    #[test]
    fn insert_resume_args_appends_after_overrides() {
        let mut args = vec![OsString::from("-c"), OsString::from("model=\"gpt-5.5\"")];

        insert_resume_args(&mut args, 2, "019dfdd3-debc-7da2-88fc-b15b73f5e138");

        assert_eq!(
            args,
            vec![
                OsString::from("-c"),
                OsString::from("model=\"gpt-5.5\""),
                OsString::from("resume"),
                OsString::from("019dfdd3-debc-7da2-88fc-b15b73f5e138"),
            ]
        );
    }
}
