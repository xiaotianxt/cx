use std::fs;
use std::path::Path;
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::process::Command;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use crate::paths;
use crate::paths::ManagerPaths;

const UPGRADE_SCHEMA_VERSION: u64 = 1;
const STARTUP_REPAIR_ID: &str = "runtime-surface-removal-v1";
const CURRENT_STATS_FILE_SCHEMA_VERSION: u64 = 2;
#[cfg(target_os = "macos")]
const REMOVED_LAUNCHD_LABEL: &str = "dev.xiaotian.cx.service";
const REMOVED_RUNTIME_DIRS: &[&str] = &["service", "serve"];
const SLOT_SQLITE_FILES: &[&str] = &["state_5.sqlite", "logs_2.sqlite"];
const SQLITE_SIDECARS: &[&str] = &["", "-wal", "-shm"];

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartupRepairReport {
    schema_version: u64,
    repair_id: &'static str,
    completed_at_unix: u64,
    public_release_floor: &'static str,
    public_release_ceiling: &'static str,
    actions: Vec<String>,
}

pub(crate) fn run_startup(paths: &ManagerPaths) -> Result<()> {
    run_startup_inner(paths, false)
}

pub(crate) fn run_after_import(paths: &ManagerPaths) -> Result<()> {
    run_startup_inner(paths, true)
}

fn run_startup_inner(paths: &ManagerPaths, force: bool) -> Result<()> {
    if std::env::var_os("CX_DISABLE_STARTUP_REPAIR").is_some() {
        return Ok(());
    }
    if !force && startup_repair_marker(paths).exists() {
        return Ok(());
    }
    if !force && !startup_repair_relevant(paths)? {
        return Ok(());
    }

    let mut actions = Vec::new();
    repair_stats_owned_files(paths, &mut actions)?;
    repair_slot_sqlite_layout(paths, &mut actions)?;
    retire_removed_runtime(paths, &mut actions)?;
    write_startup_repair_marker(paths, actions.clone())?;

    if !actions.is_empty() {
        eprintln!(
            "cx upgrade: applied {} local startup repair{}",
            actions.len(),
            if actions.len() == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

fn startup_repair_relevant(paths: &ManagerPaths) -> Result<bool> {
    if paths.manager_dir.exists() {
        return Ok(true);
    }
    Ok(removed_launch_agent_file()?.exists())
}

fn repair_stats_owned_files(paths: &ManagerPaths, actions: &mut Vec<String>) -> Result<()> {
    repair_json_schema_file(
        &paths.manager_dir.join("price-cache.json"),
        &["fetchedAt", "sourceUrl", "prices"],
        "price-cache.json",
        actions,
    )?;
    repair_json_schema_file(
        &paths.manager_dir.join("stats-calibration.json"),
        &[
            "calibratedAt",
            "samples",
            "sourceRollouts",
            "totalTokens",
            "tokenMix",
        ],
        "stats-calibration.json",
        actions,
    )
}

fn repair_json_schema_file(
    path: &Path,
    required_keys: &[&str],
    label: &str,
    actions: &mut Vec<String>,
) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let Ok(mut value) = serde_json::from_str::<Value>(&content) else {
        return Ok(());
    };
    let Some(object) = value.as_object_mut() else {
        return Ok(());
    };
    if required_keys.iter().any(|key| !object.contains_key(*key)) {
        return Ok(());
    }

    let schema_version = object.get("schemaVersion").and_then(Value::as_u64);
    if !matches!(schema_version, None | Some(1)) {
        return Ok(());
    }
    object.insert(
        "schemaVersion".to_string(),
        Value::from(CURRENT_STATS_FILE_SCHEMA_VERSION),
    );
    fs::write(path, serde_json::to_string_pretty(&value)? + "\n")
        .with_context(|| format!("write {}", path.display()))?;
    actions.push(format!("updated {label} schema marker"));
    Ok(())
}

fn repair_slot_sqlite_layout(paths: &ManagerPaths, actions: &mut Vec<String>) -> Result<()> {
    if !paths.slots_dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&paths.slots_dir)
        .with_context(|| format!("read {}", paths.slots_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let slot = entry.file_name().to_string_lossy().to_string();
        let slot_home = entry.path().join("home");
        for name in SLOT_SQLITE_FILES {
            for suffix in SQLITE_SIDECARS {
                repair_slot_sqlite_file(
                    paths,
                    &slot,
                    &slot_home,
                    &format!("{name}{suffix}"),
                    actions,
                )?;
            }
        }
    }
    Ok(())
}

fn repair_slot_sqlite_file(
    paths: &ManagerPaths,
    slot: &str,
    slot_home: &Path,
    file_name: &str,
    actions: &mut Vec<String>,
) -> Result<()> {
    let source = slot_home.join(file_name);
    let Ok(metadata) = fs::symlink_metadata(&source) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() {
        fs::remove_file(&source).with_context(|| format!("remove {}", source.display()))?;
        actions.push(format!("removed slot sqlite link for {slot}/{file_name}"));
        return Ok(());
    }

    let sqlite_home = slot_home.join("sqlite");
    fs::create_dir_all(&sqlite_home)
        .with_context(|| format!("create {}", sqlite_home.display()))?;
    let destination = sqlite_home.join(file_name);
    if !path_exists_or_symlink(&destination) {
        fs::rename(&source, &destination)
            .with_context(|| format!("move {} to {}", source.display(), destination.display()))?;
        actions.push(format!("moved slot sqlite file for {slot}/{file_name}"));
        return Ok(());
    }

    let archive_dir = retired_runtime_dir(paths)
        .join("slot-sqlite")
        .join(safe_path_component(slot));
    move_to_archive(&source, &archive_dir, file_name, actions)
}

fn retire_removed_runtime(paths: &ManagerPaths, actions: &mut Vec<String>) -> Result<()> {
    retire_launch_agent(paths, actions)?;
    for name in REMOVED_RUNTIME_DIRS {
        let source = paths.manager_dir.join(name);
        if !path_exists_or_symlink(&source) {
            continue;
        }
        move_to_archive(&source, &retired_runtime_dir(paths), name, actions)?;
    }
    Ok(())
}

fn retire_launch_agent(paths: &ManagerPaths, actions: &mut Vec<String>) -> Result<()> {
    let plist = removed_launch_agent_file()?;
    if !plist.exists() {
        return Ok(());
    }
    unload_launch_agent(&plist);
    let archive_dir = retired_runtime_dir(paths).join("launch-agent");
    move_to_archive(
        &plist,
        &archive_dir,
        "dev.xiaotian.cx.service.plist",
        actions,
    )
}

#[cfg(target_os = "macos")]
fn unload_launch_agent(plist: &Path) {
    let Ok(output) = Command::new("/usr/bin/id").arg("-u").output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid.is_empty() {
        return;
    }
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("gui/{uid}")])
        .arg(plist)
        .status();
    let _ = Command::new("/bin/launchctl")
        .args(["remove", REMOVED_LAUNCHD_LABEL])
        .status();
}

#[cfg(not(target_os = "macos"))]
fn unload_launch_agent(_plist: &Path) {}

fn move_to_archive(
    source: &Path,
    archive_dir: &Path,
    label: &str,
    actions: &mut Vec<String>,
) -> Result<()> {
    fs::create_dir_all(archive_dir).with_context(|| format!("create {}", archive_dir.display()))?;
    let destination = available_archive_path(archive_dir, label);
    fs::rename(source, &destination)
        .with_context(|| format!("move {} to {}", source.display(), destination.display()))?;
    actions.push(format!("retired {} to {}", label, destination.display()));
    Ok(())
}

fn available_archive_path(archive_dir: &Path, label: &str) -> PathBuf {
    let mut candidate = archive_dir.join(label);
    if !path_exists_or_symlink(&candidate) {
        return candidate;
    }
    for index in 1.. {
        candidate = archive_dir.join(format!("{label}.{index}"));
        if !path_exists_or_symlink(&candidate) {
            return candidate;
        }
    }
    unreachable!("unbounded archive suffix search must return")
}

fn removed_launch_agent_file() -> Result<PathBuf> {
    Ok(paths::home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join("dev.xiaotian.cx.service.plist"))
}

fn retired_runtime_dir(paths: &ManagerPaths) -> PathBuf {
    paths
        .manager_dir
        .join("state")
        .join("retired-runtime")
        .join(STARTUP_REPAIR_ID)
}

fn startup_repair_marker(paths: &ManagerPaths) -> PathBuf {
    paths
        .manager_dir
        .join("state")
        .join("upgrades")
        .join(format!("{STARTUP_REPAIR_ID}.json"))
}

fn write_startup_repair_marker(paths: &ManagerPaths, actions: Vec<String>) -> Result<()> {
    let path = startup_repair_marker(paths);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let report = StartupRepairReport {
        schema_version: UPGRADE_SCHEMA_VERSION,
        repair_id: STARTUP_REPAIR_ID,
        completed_at_unix: unix_now()?,
        public_release_floor: "v0.1.2",
        public_release_ceiling: "v0.4.1",
        actions,
    };
    fs::write(&path, serde_json::to_string_pretty(&report)? + "\n")
        .with_context(|| format!("write {}", path.display()))
}

fn path_exists_or_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn safe_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs())
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
            "cx-upgrade-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths::from_roots(root.join("codex"), root.join("codex/profile-manager"))
    }

    #[test]
    fn startup_repair_adds_schema_markers_to_owned_stats_files() {
        let paths = temp_paths("stats-schema");
        fs::create_dir_all(&paths.manager_dir).unwrap();
        fs::write(
            paths.manager_dir.join("price-cache.json"),
            r#"{
              "fetchedAt": 123,
              "sourceUrl": "https://example.test/pricing",
              "prices": {}
            }"#,
        )
        .unwrap();
        fs::write(
            paths.manager_dir.join("stats-calibration.json"),
            r#"{
              "schemaVersion": 1,
              "calibratedAt": 123,
              "samples": 1,
              "sourceRollouts": 1,
              "totalTokens": 1050,
              "tokenMix": {
                "uncachedInputShare": 0.1,
                "cachedInputShare": 0.85,
                "outputShare": 0.05
              }
            }"#,
        )
        .unwrap();

        run_startup(&paths).unwrap();

        let price_cache: Value = serde_json::from_str(
            &fs::read_to_string(paths.manager_dir.join("price-cache.json")).unwrap(),
        )
        .unwrap();
        let calibration: Value = serde_json::from_str(
            &fs::read_to_string(paths.manager_dir.join("stats-calibration.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(price_cache["schemaVersion"], Value::from(2));
        assert_eq!(calibration["schemaVersion"], Value::from(2));
        assert!(startup_repair_marker(&paths).is_file());

        let _ = fs::remove_dir_all(paths.base_codex_home);
    }

    #[test]
    fn startup_repair_moves_slot_sqlite_files_to_current_home() {
        let paths = temp_paths("slot-sqlite");
        let slot_home = paths.slot_home("dia1");
        fs::create_dir_all(&slot_home).unwrap();
        fs::write(slot_home.join("state_5.sqlite"), "state").unwrap();
        fs::write(slot_home.join("state_5.sqlite-wal"), "wal").unwrap();

        run_startup(&paths).unwrap();

        assert!(!slot_home.join("state_5.sqlite").exists());
        assert_eq!(
            fs::read_to_string(paths.slot_sqlite_home("dia1").join("state_5.sqlite")).unwrap(),
            "state"
        );
        assert_eq!(
            fs::read_to_string(paths.slot_sqlite_home("dia1").join("state_5.sqlite-wal")).unwrap(),
            "wal"
        );

        let _ = fs::remove_dir_all(paths.base_codex_home);
    }

    #[cfg(unix)]
    #[test]
    fn startup_repair_removes_slot_sqlite_symlinks() {
        let paths = temp_paths("slot-sqlite-link");
        let slot_home = paths.slot_home("dia1");
        fs::create_dir_all(&slot_home).unwrap();
        fs::write(paths.base_codex_home.join("state_5.sqlite"), "base").unwrap();
        std::os::unix::fs::symlink(
            paths.base_codex_home.join("state_5.sqlite"),
            slot_home.join("state_5.sqlite"),
        )
        .unwrap();

        run_startup(&paths).unwrap();

        assert!(!path_exists_or_symlink(&slot_home.join("state_5.sqlite")));
        assert_eq!(
            fs::read_to_string(paths.base_codex_home.join("state_5.sqlite")).unwrap(),
            "base"
        );

        let _ = fs::remove_dir_all(paths.base_codex_home);
    }

    #[test]
    fn startup_repair_retires_removed_runtime_dirs() {
        let paths = temp_paths("runtime-dirs");
        fs::create_dir_all(paths.manager_dir.join("service")).unwrap();
        fs::create_dir_all(paths.manager_dir.join("serve")).unwrap();
        fs::write(paths.manager_dir.join("service/default.json"), "{}").unwrap();
        fs::write(paths.manager_dir.join("serve/default.json"), "{}").unwrap();

        run_startup(&paths).unwrap();

        assert!(!paths.manager_dir.join("service").exists());
        assert!(!paths.manager_dir.join("serve").exists());
        assert!(retired_runtime_dir(&paths)
            .join("service/default.json")
            .is_file());
        assert!(retired_runtime_dir(&paths)
            .join("serve/default.json")
            .is_file());

        let _ = fs::remove_dir_all(paths.base_codex_home);
    }

    #[test]
    fn import_repair_runs_even_after_startup_marker_exists() {
        let paths = temp_paths("import-force");
        fs::create_dir_all(&paths.manager_dir).unwrap();

        run_startup(&paths).unwrap();
        fs::write(
            paths.manager_dir.join("price-cache.json"),
            r#"{
              "fetchedAt": 123,
              "sourceUrl": "https://example.test/pricing",
              "prices": {}
            }"#,
        )
        .unwrap();

        run_after_import(&paths).unwrap();

        let price_cache: Value = serde_json::from_str(
            &fs::read_to_string(paths.manager_dir.join("price-cache.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(price_cache["schemaVersion"], Value::from(2));

        let _ = fs::remove_dir_all(paths.base_codex_home);
    }
}
