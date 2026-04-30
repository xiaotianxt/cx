use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use anyhow::Result;

use crate::cli::LoginArgs;
use crate::envfile;
use crate::paths;
use crate::paths::ManagerPaths;
use crate::selector;
use crate::slot;

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
    manager_dir: Option<PathBuf>,
    codex_bin: Option<PathBuf>,
    quiet: bool,
    debug: bool,
    codex_args: Vec<OsString>,
    first_non_option: Option<String>,
}

pub fn run_from_args(args: Vec<OsString>) -> Result<()> {
    let options = parse_run_args(args)?;
    let paths = ManagerPaths::new(options.manager_dir.clone())?;
    let real_codex = resolve_codex_bin(options.codex_bin.as_ref())?;

    if std::env::var_os("CODEX_HOME").is_some() && options.slot.is_none() {
        return exec_real_codex(&real_codex, options.codex_args);
    }

    if options.slot.is_none()
        && options
            .first_non_option
            .as_deref()
            .is_some_and(|arg| BYPASS_SUBCOMMANDS.contains(&arg))
    {
        return exec_real_codex(&real_codex, options.codex_args);
    }

    let rotation = slot::load_rotation(&paths)?;
    if rotation.is_empty() && options.slot.is_none() {
        return exec_real_codex(&real_codex, options.codex_args);
    }

    let selected_slot = if let Some(slot) = options.slot.clone() {
        slot
    } else {
        let results = selector::query_slots(&paths, &rotation, usage_timeout())?;
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

    exec_slot_codex(&paths, &real_codex, &selected_slot, options)
}

pub fn exec_slot_login(paths: &ManagerPaths, args: LoginArgs) -> Result<()> {
    let real_codex = resolve_codex_bin(args.codex_bin.as_ref())?;
    let slot_home = paths.slot_home(&args.slot);
    let mut command = Command::new(real_codex);
    command.env("CODEX_HOME", slot_home);
    command.arg("login");
    command.args(args.args);
    exec(command)
}

fn exec_slot_codex(
    paths: &ManagerPaths,
    real_codex: &PathBuf,
    selected_slot: &str,
    options: RunOptions,
) -> Result<()> {
    let slot_home = paths.slot_home(selected_slot);
    if !slot_home.is_dir() {
        anyhow::bail!(
            "missing slot home for {selected_slot}: {}",
            slot_home.display()
        );
    }

    slot::repair_slot_layout(paths, selected_slot)?;

    let slot_dir = paths.slot_dir(selected_slot);
    let overrides = slot::read_override_lines(&slot_dir)?;
    let envs = envfile::read_env_file(&slot_dir.join("env.conf"))?;

    if !options.quiet {
        eprintln!("codex slot: {selected_slot}");
    }

    let mut command = Command::new(real_codex);
    command.env("CODEX_HOME", slot_home);
    command.envs(envs);
    for override_line in overrides {
        command.arg("-c").arg(override_line);
    }
    command.args(options.codex_args);
    exec(command)
}

fn exec_real_codex(real_codex: &PathBuf, args: Vec<OsString>) -> Result<()> {
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

fn resolve_codex_bin(override_path: Option<&PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.clone());
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

fn usage_timeout() -> f32 {
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
}
