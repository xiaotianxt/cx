use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use anyhow::Context;
use anyhow::Result;
use toml::Value as TomlValue;

use crate::cli::DesktopArgs;
use crate::desktop_proxy;
use crate::envfile;
use crate::paths::ManagerPaths;
use crate::run;
use crate::slot;
use crate::target::TargetSpec;

const DEFAULT_DESKTOP_BIN: &str = "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT";
const LEGACY_DESKTOP_BIN: &str = "/Applications/Codex.app/Contents/MacOS/Codex";
const DESKTOP_PROCESS_NAMES: [&str; 2] = ["ChatGPT", "Codex"];

#[derive(Debug, Clone)]
struct DesktopLaunchSpec {
    program: PathBuf,
    codex_home: PathBuf,
    envs: BTreeMap<String, String>,
    launch_cwd: PathBuf,
    workspace_root: PathBuf,
    args: Vec<OsString>,
    slot: String,
    target_name: Option<String>,
}

impl DesktopLaunchSpec {
    fn slot(&self) -> &str {
        &self.slot
    }

    fn target_name(&self) -> Option<&str> {
        self.target_name.as_deref()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.env("CODEX_HOME", &self.codex_home);
        command.envs(&self.envs);
        command.current_dir(&self.launch_cwd);
        command.args(&self.args);
        command
    }
}

pub fn launch(args: DesktopArgs) -> Result<()> {
    ensure_desktop_not_running(args.allow_parallel)?;

    let paths = ManagerPaths::new(args.manager_dir.clone())?;
    crate::upgrade::run_startup(&paths)?;
    let launch_cwd = std::env::current_dir().context("resolve current directory")?;
    let command_progress = crate::output::CommandProgress::for_human_output(false);
    let mut progress = command_progress.slot_query("checking slots");
    let runtime = run::select_runtime_with_progress(
        &paths,
        args.slot.as_deref(),
        args.target.as_deref(),
        args.cx_debug,
        &mut progress,
    )?;
    let app_bin = resolve_desktop_bin(args.app_bin.as_deref())?;
    let spec = build_desktop_launch_spec(
        &paths,
        app_bin,
        &runtime.slot,
        runtime.target.as_ref(),
        &launch_cwd,
        args.args.into_iter().map(OsString::from).collect(),
    )?;

    eprintln!("cx desktop slot: {}", spec.slot());
    if let Some(target) = spec.target_name() {
        eprintln!("cx desktop target: {target}");
    }
    eprintln!("cx desktop CODEX_HOME: {}", spec.codex_home.display());
    eprintln!("cx desktop workspace: {}", spec.workspace_root.display());

    let mut command = spec.command();
    if args.wait {
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().context("run ChatGPT Desktop")?;
        let status = child.wait().context("wait for ChatGPT Desktop")?;
        if !status.success() {
            anyhow::bail!("ChatGPT Desktop exited with {status}");
        }
    } else {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().context("launch ChatGPT Desktop")?;
        eprintln!("cx desktop pid: {}", child.id());
    }

    Ok(())
}

fn ensure_desktop_not_running(allow_parallel: bool) -> Result<()> {
    ensure_desktop_not_running_with(allow_parallel, running_desktop_process)
}

fn ensure_desktop_not_running_with<F>(allow_parallel: bool, running: F) -> Result<()>
where
    F: FnOnce() -> Result<Option<RunningDesktop>>,
{
    if allow_parallel {
        return Ok(());
    }
    ensure_no_running_desktop(false, running()?)
}

fn ensure_no_running_desktop(allow_parallel: bool, running: Option<RunningDesktop>) -> Result<()> {
    if allow_parallel {
        return Ok(());
    }

    let Some(running) = running else {
        return Ok(());
    };

    anyhow::bail!(
        "ChatGPT Desktop is already running ({}). Quit it before running `cx desktop`, or pass --allow-parallel to bypass this guard.",
        running.display()
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunningDesktop {
    pids: Vec<u32>,
}

impl RunningDesktop {
    fn display(&self) -> String {
        match self.pids.as_slice() {
            [] => "pid unknown".to_string(),
            [pid] => format!("pid {pid}"),
            pids => format!(
                "pids {}",
                pids.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

#[cfg(target_os = "macos")]
fn running_desktop_process() -> Result<Option<RunningDesktop>> {
    running_desktop_process_with(pgrep_exact)
}

fn running_desktop_process_with<F>(mut pgrep: F) -> Result<Option<RunningDesktop>>
where
    F: FnMut(&str) -> Result<Vec<u32>>,
{
    let mut pids = Vec::new();
    for process_name in DESKTOP_PROCESS_NAMES {
        pids.extend(pgrep(process_name)?);
    }
    pids.sort_unstable();
    pids.dedup();

    if pids.is_empty() {
        Ok(None)
    } else {
        Ok(Some(RunningDesktop { pids }))
    }
}

#[cfg(target_os = "macos")]
fn pgrep_exact(process_name: &str) -> Result<Vec<u32>> {
    let output = Command::new("/usr/bin/pgrep")
        .args(["-x", process_name])
        .output()
        .context("check for running ChatGPT Desktop")?;

    if output.status.success() {
        return Ok(parse_pgrep_pids(&output.stdout));
    }
    if output.status.code() == Some(1) {
        return Ok(Vec::new());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        anyhow::bail!("failed to check for running ChatGPT Desktop process `{process_name}`");
    }
    anyhow::bail!("failed to check for running ChatGPT Desktop process `{process_name}`: {stderr}");
}

#[cfg(not(target_os = "macos"))]
fn running_desktop_process() -> Result<Option<RunningDesktop>> {
    Ok(None)
}

fn parse_pgrep_pids(stdout: &[u8]) -> Vec<u32> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

fn resolve_desktop_bin(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(normalize_desktop_bin(path.to_path_buf()));
    }
    if let Some(path) = std::env::var_os("CX_CODEX_DESKTOP_BIN") {
        return Ok(normalize_desktop_bin(PathBuf::from(path)));
    }

    let default_paths = [
        PathBuf::from(DEFAULT_DESKTOP_BIN),
        PathBuf::from(LEGACY_DESKTOP_BIN),
    ];
    if let Some(path) = default_paths.into_iter().find(|path| path.exists()) {
        return Ok(path);
    }

    anyhow::bail!(
        "ChatGPT Desktop executable not found; checked {} and {}; install it with `brew install --cask codex-app` or pass --app-bin",
        DEFAULT_DESKTOP_BIN,
        LEGACY_DESKTOP_BIN
    );
}

fn normalize_desktop_bin(path: PathBuf) -> PathBuf {
    if path.extension().and_then(|value| value.to_str()) == Some("app") {
        let Some(executable_name) = path.file_stem().filter(|name| !name.is_empty()) else {
            return path;
        };
        let executable_name = executable_name.to_os_string();
        path.join("Contents/MacOS").join(executable_name)
    } else {
        path
    }
}

/// After `repair_slot_layout` re-symlinks `config.toml` to the global file,
/// merge `overrides.conf` lines into a slot-private `config.toml` so the
/// desktop app picks up custom model providers and settings.
fn materialize_slot_config_toml(slot_home: &Path, overrides: &[String]) -> Result<()> {
    let config_path = slot_home.join("config.toml");
    let base_content = if config_path.exists() {
        fs::read_to_string(&config_path)
            .with_context(|| format!("read config.toml from {}", config_path.display()))?
    } else {
        String::new()
    };

    let mut value: TomlValue = if base_content.trim().is_empty() {
        TomlValue::Table(Default::default())
    } else {
        toml::from_str(&base_content).with_context(|| "parse config.toml")?
    };

    for line in overrides {
        let parsed: TomlValue =
            toml::from_str(line).with_context(|| format!("parse override line: {line}"))?;
        if let Some(table) = parsed.as_table() {
            for (key, val) in table {
                merge_toml_value(&mut value, key, val.clone());
            }
        }
    }

    let merged = toml::to_string_pretty(&value).with_context(|| "serialize merged config.toml")?;

    // Replace the symlink (or file) with a real file containing merged config.
    if fs::symlink_metadata(&config_path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        fs::remove_file(&config_path)
            .with_context(|| format!("remove symlink {}", config_path.display()))?;
    }
    fs::write(&config_path, merged)
        .with_context(|| format!("write slot config.toml {}", config_path.display()))?;

    Ok(())
}

fn merge_toml_value(target: &mut TomlValue, key: &str, value: TomlValue) {
    if let Some(target_table) = target.as_table_mut() {
        if let Some(existing) = target_table.get_mut(key) {
            if let (Some(existing_table), Some(new_table)) =
                (existing.as_table_mut(), value.as_table())
            {
                for (sub_key, sub_val) in new_table {
                    existing_table.insert(sub_key.clone(), sub_val.clone());
                }
                return;
            }
        }
        target_table.insert(key.to_string(), value);
    }
}

fn build_desktop_launch_spec(
    paths: &ManagerPaths,
    app_bin: PathBuf,
    selected_slot: &str,
    target: Option<&TargetSpec>,
    launch_cwd: &Path,
    args: Vec<OsString>,
) -> Result<DesktopLaunchSpec> {
    let slot_home = paths.slot_home(selected_slot);
    if !slot_home.is_dir() {
        anyhow::bail!(
            "missing slot home for {selected_slot}: {}",
            slot_home.display()
        );
    }

    slot::repair_slot_layout(paths, selected_slot)?;

    let slot_dir = paths.slot_dir(selected_slot);
    let mut envs = envfile::read_env_file(&slot_dir.join("env.conf"))?;
    let overrides = slot::read_override_lines(&slot_dir)?;
    let target_overrides = target
        .map(|t| t.overrides().iter().cloned())
        .unwrap_or_default();
    let mut all_overrides: Vec<String> = overrides.into_iter().chain(target_overrides).collect();
    let runtime_overrides = crate::runtime_provider::resolve(
        &paths.base_codex_home.join("config.toml"),
        &all_overrides,
    )?;
    all_overrides.extend(runtime_overrides);
    materialize_slot_config_toml(&slot_home, &all_overrides)?;
    let target_name = target.map(|target| target.name().to_string());
    if let Some(target) = target {
        envs.extend(target.env().clone());
    }
    inject_desktop_codex_proxy(&app_bin, &mut envs)?;
    let sqlite_home = paths.shared_sqlite_home();
    fs::create_dir_all(&sqlite_home)
        .with_context(|| format!("create sqlite home {}", sqlite_home.display()))?;
    envs.insert(
        run::CODEX_SQLITE_HOME.to_string(),
        sqlite_home.display().to_string(),
    );
    run::inject_keychain_access_token(&slot_dir, selected_slot, &mut envs)?;
    let workspace_root = desktop_workspace_root(&args, launch_cwd);
    let args = ensure_open_project_arg(args, launch_cwd);
    let args = prepend_desktop_app_arg(&app_bin, args);

    Ok(DesktopLaunchSpec {
        program: app_bin,
        codex_home: slot_home,
        envs,
        launch_cwd: launch_cwd.to_path_buf(),
        workspace_root,
        args,
        slot: selected_slot.to_string(),
        target_name,
    })
}

fn inject_desktop_codex_proxy(app_bin: &Path, envs: &mut BTreeMap<String, String>) -> Result<()> {
    let cx_exe = std::env::current_exe().context("resolve cx executable")?;
    let contents_dir = app_bin
        .parent()
        .and_then(Path::parent)
        .with_context(|| format!("resolve app bundle from {}", app_bin.display()))?;
    let real_codex = contents_dir.join("Resources/codex");

    envs.insert(
        desktop_proxy::CODEX_CLI_PATH_ENV.to_string(),
        cx_exe.display().to_string(),
    );
    envs.insert(desktop_proxy::ENABLE_ENV.to_string(), "1".to_string());
    envs.insert(
        desktop_proxy::REAL_CODEX_ENV.to_string(),
        real_codex.display().to_string(),
    );
    envs.insert(desktop_proxy::FORCE_CLI_ENV.to_string(), "1".to_string());
    Ok(())
}

fn ensure_open_project_arg(mut args: Vec<OsString>, workspace_root: &Path) -> Vec<OsString> {
    if desktop_open_project_arg(&args).is_none() {
        args.push(OsString::from("--open-project"));
        args.push(workspace_root.as_os_str().to_os_string());
    }
    args
}

fn prepend_desktop_app_arg(app_bin: &Path, args: Vec<OsString>) -> Vec<OsString> {
    let Some(app_payload) = desktop_app_payload(app_bin) else {
        return args;
    };
    let mut with_payload = Vec::with_capacity(args.len() + 1);
    with_payload.push(app_payload.into_os_string());
    with_payload.extend(args);
    with_payload
}

fn desktop_app_payload(app_bin: &Path) -> Option<PathBuf> {
    let contents_dir = app_bin.parent()?.parent()?;
    let payload = contents_dir.join("Resources/app.asar");
    payload.exists().then_some(payload)
}

fn desktop_workspace_root(args: &[OsString], launch_cwd: &Path) -> PathBuf {
    let Some(path) = desktop_open_project_arg(args) else {
        return launch_cwd.to_path_buf();
    };
    if path.is_absolute() {
        path
    } else {
        launch_cwd.join(path)
    }
}

fn desktop_open_project_arg(args: &[OsString]) -> Option<PathBuf> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let arg_text = arg.to_string_lossy();
        if arg_text == "--open-project" {
            return args.get(index + 1).map(PathBuf::from);
        }
        if let Some(path) = arg_text.strip_prefix("--open-project=") {
            if !path.trim().is_empty() {
                return Some(PathBuf::from(path));
            }
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use crate::target;

    use super::*;

    #[test]
    fn default_desktop_bin_uses_current_chatgpt_bundle() {
        assert_eq!(
            DEFAULT_DESKTOP_BIN,
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"
        );
    }

    #[test]
    fn chatgpt_app_bundle_path_normalizes_to_executable() {
        assert_eq!(
            normalize_desktop_bin(PathBuf::from("/Applications/ChatGPT.app")),
            PathBuf::from("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT")
        );
    }

    #[test]
    fn legacy_codex_app_bundle_path_normalizes_to_executable() {
        assert_eq!(
            normalize_desktop_bin(PathBuf::from("/Applications/Codex.app")),
            PathBuf::from("/Applications/Codex.app/Contents/MacOS/Codex")
        );
    }

    #[test]
    fn running_desktop_guard_blocks_by_default() {
        let err = ensure_no_running_desktop(
            false,
            Some(RunningDesktop {
                pids: vec![123, 456],
            }),
        )
        .unwrap_err();

        let message = format!("{err:#}");
        assert!(message.contains("ChatGPT Desktop is already running"));
        assert!(message.contains("--allow-parallel"));
    }

    #[test]
    fn running_desktop_probe_checks_current_and_legacy_process_names() {
        let mut probed = Vec::new();
        let running = running_desktop_process_with(|process_name| {
            probed.push(process_name.to_string());
            Ok(match process_name {
                "ChatGPT" => vec![456],
                "Codex" => vec![123, 456],
                _ => Vec::new(),
            })
        })
        .unwrap();

        assert_eq!(probed, vec!["ChatGPT", "Codex"]);
        assert_eq!(
            running,
            Some(RunningDesktop {
                pids: vec![123, 456]
            })
        );
    }

    #[test]
    fn running_desktop_guard_allows_explicit_parallel_launch() {
        ensure_no_running_desktop(
            true,
            Some(RunningDesktop {
                pids: vec![123, 456],
            }),
        )
        .unwrap();
    }

    #[test]
    fn running_desktop_guard_skips_probe_when_parallel_is_allowed() {
        ensure_desktop_not_running_with(true, || {
            panic!("allow-parallel should skip the desktop process probe");
        })
        .unwrap();
    }

    #[test]
    fn pgrep_pid_parser_ignores_non_pid_lines() {
        assert_eq!(parse_pgrep_pids(b"123\nnot-a-pid\n456\n"), vec![123, 456]);
    }

    #[test]
    fn desktop_spec_merges_slot_and_target_env() {
        let root = temp_root("desktop_spec_merges_slot_and_target_env");
        let base = root.join(".codex");
        let manager = base.join("profile-manager");
        let cwd = root.join("work");
        let paths = ManagerPaths::from_roots(base, manager);
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(paths.slot_home("bus1")).unwrap();
        fs::write(
            paths.slot_dir("bus1").join("env.conf"),
            "export SLOT_ONLY=\"slot\"\nexport SHARED=\"slot\"\nexport CODEX_SQLITE_HOME=\"/tmp/slot-wrong\"\n",
        )
        .unwrap();
        target::save_target(
            &paths,
            target::TargetInput {
                name: "work".to_string(),
                slots: vec!["bus1".to_string()],
                overrides: Vec::new(),
                envs: vec![
                    "TARGET_ONLY=target".to_string(),
                    "SHARED=target".to_string(),
                    "CODEX_SQLITE_HOME=/tmp/target-wrong".to_string(),
                ],
            },
        )
        .unwrap();
        let target = target::load_target(&paths, "work").unwrap();

        let spec = build_desktop_launch_spec(
            &paths,
            PathBuf::from("/tmp/Codex"),
            "bus1",
            Some(&target),
            &cwd,
            Vec::new(),
        )
        .unwrap();

        assert_eq!(spec.codex_home, paths.slot_home("bus1"));
        assert_eq!(spec.envs.get("SLOT_ONLY"), Some(&"slot".to_string()));
        assert_eq!(spec.envs.get("TARGET_ONLY"), Some(&"target".to_string()));
        assert_eq!(spec.envs.get("SHARED"), Some(&"target".to_string()));
        assert_eq!(
            spec.envs.get(desktop_proxy::ENABLE_ENV),
            Some(&"1".to_string())
        );
        assert_eq!(
            spec.envs.get(desktop_proxy::FORCE_CLI_ENV),
            Some(&"1".to_string())
        );
        assert!(spec.envs.contains_key(desktop_proxy::CODEX_CLI_PATH_ENV));
        assert_eq!(
            spec.envs.get(desktop_proxy::REAL_CODEX_ENV),
            Some(&"/Resources/codex".to_string())
        );
        assert_eq!(
            spec.envs.get(run::CODEX_SQLITE_HOME),
            Some(&paths.shared_sqlite_home().display().to_string())
        );
        assert!(paths.shared_sqlite_home().is_dir());
        let command = spec.command();
        let command_sqlite_home = command
            .get_envs()
            .find_map(|(key, value)| {
                (key == run::CODEX_SQLITE_HOME)
                    .then(|| value.map(|value| value.to_string_lossy().into_owned()))
                    .flatten()
            })
            .unwrap();
        assert_eq!(
            command_sqlite_home,
            paths.shared_sqlite_home().display().to_string()
        );
        assert_eq!(command.get_current_dir(), Some(cwd.as_path()));
        assert_eq!(spec.target_name(), Some("work"));
    }

    #[test]
    fn desktop_spec_rejects_api_key_env_when_keychain_conf_is_present() {
        let root = temp_root("desktop_keychain_api_key_conflict");
        let base = root.join(".codex");
        let manager = base.join("profile-manager");
        let cwd = root.join("work");
        let paths = ManagerPaths::from_roots(base, manager);
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(paths.slot_home("bus1")).unwrap();
        fs::write(
            paths.slot_dir("bus1").join("keychain.conf"),
            "service=codex-pat\naccount=test@example.com\n",
        )
        .unwrap();
        fs::write(
            paths.slot_dir("bus1").join("env.conf"),
            "OPENAI_API_KEY=sk-test\n",
        )
        .unwrap();

        let err = build_desktop_launch_spec(
            &paths,
            PathBuf::from("/tmp/Codex"),
            "bus1",
            None,
            &cwd,
            Vec::new(),
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("OPENAI_API_KEY"));
    }

    #[test]
    fn desktop_spec_prefers_oauth_auth_json_over_keychain_pat() {
        let root = temp_root("desktop-oauth-before-keychain-pat");
        let base = root.join(".codex");
        let manager = base.join("profile-manager");
        let cwd = root.join("work");
        let paths = ManagerPaths::from_roots(base, manager);
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(paths.slot_home("bus1")).unwrap();
        fs::write(
            paths.slot_home("bus1").join("auth.json"),
            serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "id_token": "header.e30.signature",
                    "access_token": "oauth-access",
                    "refresh_token": "oauth-refresh"
                },
                "last_refresh": "2026-01-01T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            paths.slot_dir("bus1").join("keychain.conf"),
            "service=cx-test-missing\naccount=missing@example.com\n",
        )
        .unwrap();

        let spec = build_desktop_launch_spec(
            &paths,
            PathBuf::from("/tmp/Codex"),
            "bus1",
            None,
            &cwd,
            Vec::new(),
        )
        .unwrap();

        assert!(spec
            .envs
            .get("CODEX_ACCESS_TOKEN")
            .is_none_or(String::is_empty));
    }

    #[test]
    fn desktop_spec_does_not_forward_codex_overrides_to_electron() {
        let root = temp_root("desktop_spec_does_not_forward_codex_overrides_to_electron");
        let base = root.join(".codex");
        let manager = base.join("profile-manager");
        let cwd = root.join("work");
        let paths = ManagerPaths::from_roots(base, manager);
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(paths.slot_home("bus1")).unwrap();
        fs::write(
            paths.slot_dir("bus1").join("overrides.conf"),
            "model=\"gpt-5.5\"\n",
        )
        .unwrap();

        let spec = build_desktop_launch_spec(
            &paths,
            PathBuf::from("/tmp/Codex"),
            "bus1",
            None,
            &cwd,
            vec![OsString::from("--enable-logging")],
        )
        .unwrap();

        assert_eq!(
            spec.args,
            vec![
                OsString::from("--enable-logging"),
                OsString::from("--open-project"),
                cwd.as_os_str().to_os_string(),
            ]
        );
    }

    #[test]
    fn desktop_spec_materializes_cx_runtime_provider() {
        let root = temp_root("desktop-cx-runtime-provider");
        let base = root.join(".codex");
        let manager = base.join("profile-manager");
        let cwd = root.join("work");
        let paths = ManagerPaths::from_roots(base, manager);
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(paths.slot_home("pku")).unwrap();
        fs::write(
            paths.base_codex_home.join("config.toml"),
            "model = \"gpt-5.4\"\n",
        )
        .unwrap();
        fs::write(
            paths.slot_dir("pku").join("overrides.conf"),
            concat!(
                "model = \"GLM-5.2\"\n",
                "model_provider = \"pku\"\n",
                "model_providers.pku = { name = \"PKU\", base_url = \"https://example.test/v1\", wire_api = \"responses\", env_key = \"OPENAI_API_KEY\", requires_openai_auth = false }\n"
            ),
        )
        .unwrap();

        build_desktop_launch_spec(
            &paths,
            PathBuf::from("/tmp/Codex"),
            "pku",
            None,
            &cwd,
            Vec::new(),
        )
        .unwrap();
        let materialized = fs::read_to_string(paths.slot_home("pku").join("config.toml")).unwrap();

        assert!(materialized.contains("model_provider = \"cx\""));
        assert!(materialized.contains("[model_providers.cx]"));
    }

    #[test]
    fn desktop_spec_prepends_app_payload_for_direct_electron_launch() {
        let root = temp_root("desktop_spec_prepends_app_payload_for_direct_electron_launch");
        let base = root.join(".codex");
        let manager = base.join("profile-manager");
        let cwd = root.join("work");
        let app_bin = root.join("Codex.app/Contents/MacOS/Codex");
        let app_payload = root.join("Codex.app/Contents/Resources/app.asar");
        let paths = ManagerPaths::from_roots(base, manager);
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(paths.slot_home("bus1")).unwrap();
        fs::create_dir_all(app_bin.parent().unwrap()).unwrap();
        fs::create_dir_all(app_payload.parent().unwrap()).unwrap();
        fs::write(&app_bin, "").unwrap();
        fs::write(&app_payload, "").unwrap();

        let spec =
            build_desktop_launch_spec(&paths, app_bin, "bus1", None, &cwd, Vec::new()).unwrap();

        assert_eq!(
            spec.args,
            vec![
                app_payload.into_os_string(),
                OsString::from("--open-project"),
                cwd.as_os_str().to_os_string(),
            ]
        );
    }

    #[test]
    fn desktop_spec_adds_current_workspace_root() {
        let root = temp_root("desktop_spec_adds_current_workspace_root");
        let base = root.join(".codex");
        let manager = base.join("profile-manager");
        let cwd = root.join("project");
        let paths = ManagerPaths::from_roots(base, manager);
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(paths.slot_home("bus1")).unwrap();

        let spec = build_desktop_launch_spec(
            &paths,
            PathBuf::from("/tmp/Codex"),
            "bus1",
            None,
            &cwd,
            Vec::new(),
        )
        .unwrap();

        assert_eq!(spec.workspace_root, cwd);
        assert_eq!(
            spec.args,
            vec![
                OsString::from("--open-project"),
                spec.workspace_root.as_os_str().to_os_string(),
            ]
        );
    }

    #[test]
    fn desktop_spec_respects_explicit_open_project_arg() {
        let root = temp_root("desktop_spec_respects_explicit_open_project_arg");
        let base = root.join(".codex");
        let manager = base.join("profile-manager");
        let cwd = root.join("project");
        let paths = ManagerPaths::from_roots(base, manager);
        fs::create_dir_all(cwd.join("nested")).unwrap();
        fs::create_dir_all(paths.slot_home("bus1")).unwrap();

        let spec = build_desktop_launch_spec(
            &paths,
            PathBuf::from("/tmp/Codex"),
            "bus1",
            None,
            &cwd,
            vec![
                OsString::from("--open-project"),
                OsString::from("nested"),
                OsString::from("--trace-warnings"),
            ],
        )
        .unwrap();

        assert_eq!(spec.workspace_root, cwd.join("nested"));
        assert_eq!(
            spec.args,
            vec![
                OsString::from("--open-project"),
                OsString::from("nested"),
                OsString::from("--trace-warnings"),
            ]
        );
    }

    #[test]
    fn desktop_spec_respects_equals_open_project_arg() {
        let root = temp_root("desktop_spec_respects_equals_open_project_arg");
        let base = root.join(".codex");
        let manager = base.join("profile-manager");
        let cwd = root.join("project");
        let workspace = root.join("other");
        let paths = ManagerPaths::from_roots(base, manager);
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(paths.slot_home("bus1")).unwrap();

        let spec = build_desktop_launch_spec(
            &paths,
            PathBuf::from("/tmp/Codex"),
            "bus1",
            None,
            &cwd,
            vec![OsString::from(format!(
                "--open-project={}",
                workspace.display()
            ))],
        )
        .unwrap();

        assert_eq!(spec.workspace_root, workspace);
        assert_eq!(spec.args.len(), 1);
    }

    fn temp_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("cx-{name}-{}-{nanos}", std::process::id()))
    }
}
