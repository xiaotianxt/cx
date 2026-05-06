use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use anyhow::Context;
use anyhow::Result;

use crate::cli::DesktopArgs;
use crate::envfile;
use crate::paths::ManagerPaths;
use crate::run;
use crate::slot;
use crate::target::TargetSpec;

const DEFAULT_DESKTOP_BIN: &str = "/Applications/Codex.app/Contents/MacOS/Codex";

#[derive(Debug, Clone)]
struct DesktopLaunchSpec {
    program: PathBuf,
    codex_home: PathBuf,
    envs: BTreeMap<String, String>,
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

    fn into_command(self) -> Command {
        let mut command = Command::new(self.program);
        command.env("CODEX_HOME", self.codex_home);
        command.envs(self.envs);
        command.args(self.args);
        command
    }
}

pub fn launch(args: DesktopArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.manager_dir.clone())?;
    let runtime = run::select_runtime(
        &paths,
        args.slot.as_deref(),
        args.target.as_deref(),
        args.cx_debug,
    )?;
    let app_bin = resolve_desktop_bin(args.app_bin.as_deref())?;
    let spec = build_desktop_launch_spec(
        &paths,
        app_bin,
        &runtime.slot,
        runtime.target.as_ref(),
        args.args.into_iter().map(OsString::from).collect(),
    )?;

    eprintln!("cx desktop slot: {}", spec.slot());
    if let Some(target) = spec.target_name() {
        eprintln!("cx desktop target: {target}");
    }
    eprintln!("cx desktop CODEX_HOME: {}", spec.codex_home.display());

    let mut command = spec.into_command();
    if args.wait {
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let status = command.status().context("run Codex Desktop")?;
        if !status.success() {
            anyhow::bail!("Codex Desktop exited with {status}");
        }
    } else {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().context("launch Codex Desktop")?;
        eprintln!("cx desktop pid: {}", child.id());
    }

    Ok(())
}

fn resolve_desktop_bin(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(normalize_desktop_bin(path.to_path_buf()));
    }
    if let Some(path) = std::env::var_os("CX_CODEX_DESKTOP_BIN") {
        return Ok(normalize_desktop_bin(PathBuf::from(path)));
    }

    let path = PathBuf::from(DEFAULT_DESKTOP_BIN);
    if path.exists() {
        return Ok(path);
    }

    anyhow::bail!(
        "Codex Desktop executable not found at {}; install it with `brew install --cask codex-app` or pass --app-bin",
        DEFAULT_DESKTOP_BIN
    );
}

fn normalize_desktop_bin(path: PathBuf) -> PathBuf {
    if path.extension().and_then(|value| value.to_str()) == Some("app") {
        path.join("Contents/MacOS/Codex")
    } else {
        path
    }
}

fn build_desktop_launch_spec(
    paths: &ManagerPaths,
    app_bin: PathBuf,
    selected_slot: &str,
    target: Option<&TargetSpec>,
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
    let target_name = target.map(|target| target.name().to_string());
    if let Some(target) = target {
        envs.extend(target.env().clone());
    }

    Ok(DesktopLaunchSpec {
        program: app_bin,
        codex_home: slot_home,
        envs,
        args,
        slot: selected_slot.to_string(),
        target_name,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use crate::target;

    use super::*;

    #[test]
    fn app_bundle_path_normalizes_to_executable() {
        assert_eq!(
            normalize_desktop_bin(PathBuf::from("/Applications/Codex.app")),
            PathBuf::from("/Applications/Codex.app/Contents/MacOS/Codex")
        );
    }

    #[test]
    fn desktop_spec_merges_slot_and_target_env() {
        let root = temp_root("desktop_spec_merges_slot_and_target_env");
        let base = root.join(".codex");
        let manager = base.join("profile-manager");
        let paths = ManagerPaths::from_roots(base, manager);
        fs::create_dir_all(paths.slot_home("bus1")).unwrap();
        fs::write(
            paths.slot_dir("bus1").join("env.conf"),
            "export SLOT_ONLY=\"slot\"\nexport SHARED=\"slot\"\n",
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
            Vec::new(),
        )
        .unwrap();

        assert_eq!(spec.codex_home, paths.slot_home("bus1"));
        assert_eq!(spec.envs.get("SLOT_ONLY"), Some(&"slot".to_string()));
        assert_eq!(spec.envs.get("TARGET_ONLY"), Some(&"target".to_string()));
        assert_eq!(spec.envs.get("SHARED"), Some(&"target".to_string()));
        assert_eq!(spec.target_name(), Some("work"));
    }

    #[test]
    fn desktop_spec_does_not_forward_codex_overrides_to_electron() {
        let root = temp_root("desktop_spec_does_not_forward_codex_overrides_to_electron");
        let base = root.join(".codex");
        let manager = base.join("profile-manager");
        let paths = ManagerPaths::from_roots(base, manager);
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
            vec![OsString::from("--enable-logging")],
        )
        .unwrap();

        assert_eq!(spec.args, vec![OsString::from("--enable-logging")]);
    }

    fn temp_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("cx-{name}-{}-{nanos}", std::process::id()))
    }
}
