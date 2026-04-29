use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use anyhow::Result;
use toml::Value;

use crate::cli::AddArgs;
use crate::envfile;
use crate::paths::ManagerPaths;

const SHARED_NAMES: &[&str] = &[
    "AGENTS.md",
    "plugins",
    "vendor_imports",
    "rules",
    "installation_id",
    "version.json",
];

const SHARED_SQLITE: &[&str] = &["state_5.sqlite", "logs_2.sqlite"];

pub fn load_rotation(paths: &ManagerPaths) -> Result<Vec<String>> {
    if !paths.rotation_file.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&paths.rotation_file)
        .with_context(|| format!("read {}", paths.rotation_file.display()))?;
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

pub fn read_override_lines(slot_dir: &Path) -> Result<Vec<String>> {
    let path = slot_dir.join("overrides.conf");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

pub fn read_override_string(slot_dir: &Path, key: &str) -> Result<Option<String>> {
    for line in read_override_lines(slot_dir)? {
        let Ok(value) = toml::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(value) = value.get(key).and_then(Value::as_str) {
            return Ok(Some(value.to_string()));
        }
    }
    Ok(None)
}

pub fn read_config_string(path: &Path, key: &str) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value =
        toml::from_str::<Value>(&content).with_context(|| format!("parse {}", path.display()))?;
    Ok(value.get(key).and_then(Value::as_str).map(str::to_string))
}

pub fn add_slot(paths: &ManagerPaths, args: AddArgs) -> Result<()> {
    validate_slot_name(&args.slot)?;
    ensure_slot_layout(paths, &args.slot)?;

    let slot_dir = paths.slot_dir(&args.slot);
    let overrides_file = slot_dir.join("overrides.conf");
    let env_file = slot_dir.join("env.conf");

    if !args.sets.is_empty() {
        fs::write(&overrides_file, args.sets.join("\n") + "\n")
            .with_context(|| format!("write {}", overrides_file.display()))?;
    } else if !overrides_file.exists() {
        fs::write(&overrides_file, "")
            .with_context(|| format!("write {}", overrides_file.display()))?;
    }

    if !args.envs.is_empty() {
        envfile::write_env_file(&env_file, &args.envs)?;
    } else if !env_file.exists() {
        fs::write(&env_file, "").with_context(|| format!("write {}", env_file.display()))?;
    }

    if args.from_current {
        copy_auth_from_current(paths, &args.slot)?;
    }
    if args.rotate {
        append_rotation(paths, &args.slot)?;
    }

    println!("updated slot: {}", args.slot);
    if args.from_current {
        println!("copied auth from current ~/.codex into slot: {}", args.slot);
    }
    if args.rotate {
        println!("added to rotation: {}", args.slot);
    }
    println!("slot home: {}", paths.slot_home(&args.slot).display());
    Ok(())
}

pub fn ensure_slot_layout(paths: &ManagerPaths, slot: &str) -> Result<()> {
    validate_slot_name(slot)?;
    let slot_dir = paths.slot_dir(slot);
    let home_dir = slot_dir.join("home");
    fs::create_dir_all(home_dir.join("accounts"))
        .with_context(|| format!("create {}", home_dir.display()))?;

    link_if_safe(
        &paths.base_codex_home.join("config.toml"),
        &home_dir.join("config.toml"),
    )?;
    for name in SHARED_NAMES {
        let source = paths.base_codex_home.join(name);
        if source.exists() {
            link_if_safe(&source, &home_dir.join(name))?;
        }
    }
    Ok(())
}

pub fn ensure_shared_sqlite(paths: &ManagerPaths, slot: &str) -> Result<()> {
    let slot_home = paths.slot_home(slot);
    for name in SHARED_SQLITE {
        let slot_path = slot_home.join(name);
        let canonical_path = paths.base_codex_home.join(name);
        if is_symlink_to(&slot_path, &canonical_path)? && canonical_path.exists() {
            continue;
        }
        if slot_path.exists() && !is_symlink(&slot_path) {
            if !canonical_path.exists() {
                fs::copy(&slot_path, &canonical_path).with_context(|| {
                    format!(
                        "copy {} to {}",
                        slot_path.display(),
                        canonical_path.display()
                    )
                })?;
            } else if *name == "state_5.sqlite" {
                let _ = merge_threads_with_sqlite3(&canonical_path, &slot_path);
            }
            remove_sqlite_family(&slot_path)?;
        }
        remove_sqlite_family(&slot_path)?;
        link_if_safe(&canonical_path, &slot_path)?;
    }
    Ok(())
}

fn copy_auth_from_current(paths: &ManagerPaths, slot: &str) -> Result<()> {
    let home_dir = paths.slot_home(slot);
    copy_if_exists(
        &paths.base_codex_home.join("auth.json"),
        &home_dir.join("auth.json"),
    )?;
    copy_if_exists(
        &paths.base_codex_home.join("current"),
        &home_dir.join("current"),
    )?;

    let source_accounts = paths.base_codex_home.join("accounts");
    let dest_accounts = home_dir.join("accounts");
    if source_accounts.is_dir() {
        if dest_accounts.exists() {
            fs::remove_dir_all(&dest_accounts)
                .with_context(|| format!("remove {}", dest_accounts.display()))?;
        }
        copy_dir_all(&source_accounts, &dest_accounts)?;
    }
    Ok(())
}

fn append_rotation(paths: &ManagerPaths, slot: &str) -> Result<()> {
    fs::create_dir_all(&paths.manager_dir)
        .with_context(|| format!("create {}", paths.manager_dir.display()))?;
    let mut slots = load_rotation(paths)?;
    if !slots.iter().any(|existing| existing == slot) {
        slots.push(slot.to_string());
        fs::write(&paths.rotation_file, slots.join("\n") + "\n")
            .with_context(|| format!("write {}", paths.rotation_file.display()))?;
    }
    Ok(())
}

fn validate_slot_name(slot: &str) -> Result<()> {
    if slot.is_empty()
        || !slot
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        anyhow::bail!("invalid slot name: {slot}");
    }
    Ok(())
}

fn link_if_safe(source: &Path, dest: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(dest) {
        if meta.file_type().is_symlink() {
            fs::remove_file(dest).with_context(|| format!("remove {}", dest.display()))?;
        } else {
            return Ok(());
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, dest)
            .with_context(|| format!("link {} -> {}", dest.display(), source.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::copy(source, dest)
            .with_context(|| format!("copy {} to {}", source.display(), dest.display()))?;
    }
    Ok(())
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

fn is_symlink_to(path: &Path, target: &Path) -> Result<bool> {
    if !is_symlink(path) {
        return Ok(false);
    }
    Ok(fs::read_link(path).with_context(|| format!("read link {}", path.display()))? == target)
}

fn copy_if_exists(source: &Path, dest: &Path) -> Result<()> {
    if source.exists() {
        fs::copy(source, dest)
            .with_context(|| format!("copy {} to {}", source.display(), dest.display()))?;
    }
    Ok(())
}

fn copy_dir_all(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for entry in fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&source_path, &dest_path)?;
        } else {
            fs::copy(&source_path, &dest_path).with_context(|| {
                format!("copy {} to {}", source_path.display(), dest_path.display())
            })?;
        }
    }
    Ok(())
}

fn remove_sqlite_family(path: &Path) -> Result<()> {
    remove_file_if_exists(path)?;
    remove_file_if_exists(&PathBuf::from(format!("{}-wal", path.display())))?;
    remove_file_if_exists(&PathBuf::from(format!("{}-shm", path.display())))?;
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

fn merge_threads_with_sqlite3(canonical_path: &Path, slot_path: &Path) -> Result<()> {
    let sql = format!(
        "ATTACH DATABASE '{}' AS slot_db; INSERT OR IGNORE INTO threads SELECT * FROM slot_db.threads; DETACH slot_db;",
        escape_sqlite_path(slot_path)
    );
    let status = Command::new("sqlite3")
        .arg(canonical_path)
        .arg(sql)
        .status()
        .with_context(|| "run sqlite3")?;
    if !status.success() {
        anyhow::bail!("sqlite3 merge failed");
    }
    Ok(())
}

fn escape_sqlite_path(path: &Path) -> String {
    path.display().to_string().replace('\'', "''")
}
