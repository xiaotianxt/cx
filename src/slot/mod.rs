use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
#[cfg(test)]
use rusqlite::Connection;

use crate::cli::AddArgs;
use crate::cli::RemoveArgs;
use crate::envfile;
use crate::paths::ManagerPaths;

mod config;
mod rotation;
mod sqlite;

pub use config::read_config_string;
pub use config::read_override_lines;
pub use config::read_override_string;
pub use rotation::load_rotation;

const SHARED_NAMES: &[&str] = &[
    "AGENTS.md",
    "accounts",
    "current",
    "history.jsonl",
    "memories",
    "models_cache.json",
    "plugins",
    "prompts",
    "rules",
    "session_index.jsonl",
    "sessions",
    "shell_snapshots",
    "skills",
    "skills-data",
    "installation_id",
    "vendor_imports",
    "version.json",
];

const SHARED_SQLITE: &[&str] = &["state_5.sqlite", "logs_2.sqlite"];

const SHARED_DIRS: &[&str] = &[
    "accounts",
    "memories",
    "plugins",
    "prompts",
    "rules",
    "sessions",
    "shell_snapshots",
    "skills",
    "skills-data",
    "vendor_imports",
];

const APPEND_ONLY_FILES: &[&str] = &["history.jsonl", "session_index.jsonl"];

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
        rotation::append_rotation(paths, &args.slot)?;
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

pub fn remove_slot(paths: &ManagerPaths, args: RemoveArgs) -> Result<()> {
    validate_slot_name(&args.slot)?;

    let removed_from_rotation = rotation::remove_from_rotation(paths, &args.slot)?;
    let slot_dir = paths.slot_dir(&args.slot);

    println!("removed slot: {}", args.slot);
    if removed_from_rotation {
        println!("removed from rotation: {}", args.slot);
    } else {
        println!("not in rotation: {}", args.slot);
    }

    if args.delete_files {
        if remove_path_if_exists(&slot_dir)? {
            println!("deleted slot files: {}", slot_dir.display());
        } else {
            println!("slot files not found: {}", slot_dir.display());
        }
    } else {
        println!("slot files kept: {}", slot_dir.display());
    }

    Ok(())
}

pub fn ensure_slot_layout(paths: &ManagerPaths, slot: &str) -> Result<()> {
    validate_slot_name(slot)?;
    let slot_dir = paths.slot_dir(slot);
    let home_dir = slot_dir.join("home");
    fs::create_dir_all(&home_dir).with_context(|| format!("create {}", home_dir.display()))?;

    link_if_needed(
        &paths.base_codex_home.join("config.toml"),
        &home_dir.join("config.toml"),
    )?;
    for name in SHARED_NAMES {
        let source = paths.base_codex_home.join(name);
        if source.exists() {
            link_if_needed(&source, &home_dir.join(name))?;
        }
    }
    Ok(())
}

pub fn repair_slot_layout(paths: &ManagerPaths, slot: &str) -> Result<()> {
    validate_slot_name(slot)?;
    let slot_home = paths.slot_home(slot);
    fs::create_dir_all(&slot_home).with_context(|| format!("create {}", slot_home.display()))?;

    link_if_needed(
        &paths.base_codex_home.join("config.toml"),
        &slot_home.join("config.toml"),
    )?;
    for name in SHARED_NAMES {
        repair_shared_path(paths, &slot_home, name)?;
    }
    repair_shared_sqlite(paths, slot)?;
    Ok(())
}

fn repair_shared_path(paths: &ManagerPaths, slot_home: &Path, name: &str) -> Result<()> {
    let canonical_path = paths.base_codex_home.join(name);
    let slot_path = slot_home.join(name);

    if SHARED_DIRS.contains(&name) {
        fs::create_dir_all(&canonical_path)
            .with_context(|| format!("create {}", canonical_path.display()))?;
    }

    if is_symlink_to(&slot_path, &canonical_path)? {
        return Ok(());
    }

    if is_symlink(&slot_path) {
        fs::remove_file(&slot_path).with_context(|| format!("remove {}", slot_path.display()))?;
        return link_if_safe(&canonical_path, &slot_path);
    }

    if slot_path.exists() {
        merge_shared_path(name, &canonical_path, &slot_path)?;
        remove_path_if_exists(&slot_path)?;
    }

    if canonical_path.exists() || !SHARED_DIRS.contains(&name) {
        link_if_safe(&canonical_path, &slot_path)?;
    }

    Ok(())
}

fn repair_shared_sqlite(paths: &ManagerPaths, slot: &str) -> Result<()> {
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
            } else if let Err(err) = sqlite::merge_sqlite_databases(&canonical_path, &slot_path) {
                if std::env::var_os("CX_SLOT_DEBUG").is_some() {
                    eprintln!(
                        "cx: skipped sqlite merge for {}: {err:#}",
                        slot_path.display()
                    );
                }
                continue;
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
    copy_if_exists_unless_same_link(
        &paths.base_codex_home.join("current"),
        &home_dir.join("current"),
    )?;

    let source_accounts = paths.base_codex_home.join("accounts");
    let dest_accounts = home_dir.join("accounts");
    if source_accounts.is_dir() {
        if is_symlink_to(&dest_accounts, &source_accounts)? {
            return Ok(());
        }
        if dest_accounts.exists() || is_symlink(&dest_accounts) {
            remove_path_if_exists(&dest_accounts)?;
        }
        copy_dir_all(&source_accounts, &dest_accounts)?;
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

fn link_if_needed(source: &Path, dest: &Path) -> Result<()> {
    if is_symlink_to(dest, source)? {
        return Ok(());
    }
    link_if_safe(source, dest)
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

fn copy_if_exists_unless_same_link(source: &Path, dest: &Path) -> Result<()> {
    if source.exists() && !is_symlink_to(dest, source)? {
        fs::copy(source, dest)
            .with_context(|| format!("copy {} to {}", source.display(), dest.display()))?;
    }
    Ok(())
}

fn merge_shared_path(name: &str, canonical_path: &Path, slot_path: &Path) -> Result<()> {
    if slot_path.is_dir() {
        merge_dir_missing(slot_path, canonical_path)?;
    } else if APPEND_ONLY_FILES.contains(&name) && canonical_path.exists() {
        append_file(slot_path, canonical_path)?;
    } else if !canonical_path.exists() {
        copy_if_exists(slot_path, canonical_path)?;
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

fn merge_dir_missing(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for entry in fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            merge_dir_missing(&source_path, &dest_path)?;
        } else if !dest_path.exists() {
            fs::copy(&source_path, &dest_path).with_context(|| {
                format!("copy {} to {}", source_path.display(), dest_path.display())
            })?;
        }
    }
    Ok(())
}

fn append_file(source: &Path, dest: &Path) -> Result<()> {
    let mut source_file =
        fs::File::open(source).with_context(|| format!("open {}", source.display()))?;
    let mut dest_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dest)
        .with_context(|| format!("open {}", dest.display()))?;
    io::copy(&mut source_file, &mut dest_file)
        .with_context(|| format!("append {} to {}", source.display(), dest.display()))?;
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

fn remove_path_if_exists(path: &Path) -> Result<bool> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
    };

    if meta.file_type().is_dir() {
        fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
    } else {
        fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(true)
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
            "cx-slot-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths {
            base_codex_home: root.join("codex"),
            manager_dir: root.join("profile-manager"),
            slots_dir: root.join("profile-manager/slots"),
            targets_dir: root.join("profile-manager/targets"),
            rotation_file: root.join("profile-manager/rotation.txt"),
        }
    }

    fn remove_args(slot: &str, delete_files: bool) -> RemoveArgs {
        RemoveArgs {
            manager_dir: None,
            slot: slot.to_string(),
            delete_files,
        }
    }

    #[test]
    fn remove_slot_keeps_files_by_default() {
        let paths = temp_paths("keep");
        fs::create_dir_all(paths.slot_home("dia1")).unwrap();
        fs::create_dir_all(&paths.manager_dir).unwrap();
        fs::write(&paths.rotation_file, "primary\ndia1\nbus1\n").unwrap();

        remove_slot(&paths, remove_args("dia1", false)).unwrap();

        assert_eq!(load_rotation(&paths).unwrap(), vec!["primary", "bus1"]);
        assert!(paths.slot_dir("dia1").exists());

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn remove_slot_deletes_files_when_requested() {
        let paths = temp_paths("delete");
        fs::create_dir_all(paths.slot_home("old")).unwrap();
        fs::create_dir_all(&paths.manager_dir).unwrap();
        fs::write(&paths.rotation_file, "primary\nold\n").unwrap();

        remove_slot(&paths, remove_args("old", true)).unwrap();

        assert_eq!(load_rotation(&paths).unwrap(), vec!["primary"]);
        assert!(!paths.slot_dir("old").exists());

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn repair_slot_layout_merges_history_and_directories() {
        let paths = temp_paths("repair-files");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        fs::create_dir_all(paths.base_codex_home.join("sessions/2026")).unwrap();
        fs::create_dir_all(paths.slot_home("dia1").join("sessions/2026")).unwrap();
        fs::write(paths.base_codex_home.join("history.jsonl"), "base\n").unwrap();
        fs::write(paths.slot_home("dia1").join("history.jsonl"), "slot\n").unwrap();
        fs::write(
            paths.slot_home("dia1").join("sessions/2026/local.jsonl"),
            "{}\n",
        )
        .unwrap();

        repair_slot_layout(&paths, "dia1").unwrap();

        assert_eq!(
            fs::read_to_string(paths.base_codex_home.join("history.jsonl")).unwrap(),
            "base\nslot\n"
        );
        assert!(is_symlink_to(
            &paths.slot_home("dia1").join("history.jsonl"),
            &paths.base_codex_home.join("history.jsonl")
        )
        .unwrap());
        assert!(paths
            .base_codex_home
            .join("sessions/2026/local.jsonl")
            .exists());
        assert!(is_symlink_to(
            &paths.slot_home("dia1").join("sessions"),
            &paths.base_codex_home.join("sessions")
        )
        .unwrap());

        let _ = fs::remove_dir_all(&paths.manager_dir);
        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }

    #[test]
    fn repair_slot_layout_merges_sqlite() {
        let paths = temp_paths("repair-sqlite");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        fs::create_dir_all(paths.slot_home("dia1")).unwrap();

        let canonical_db = paths.base_codex_home.join("state_5.sqlite");
        let slot_db = paths.slot_home("dia1").join("state_5.sqlite");
        create_test_db(&canonical_db, "base").unwrap();
        create_test_db(&slot_db, "slot").unwrap();

        repair_slot_layout(&paths, "dia1").unwrap();

        let conn = Connection::open(&canonical_db).unwrap();
        let count = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(count, 2);
        assert!(is_symlink_to(&slot_db, &canonical_db).unwrap());

        let _ = fs::remove_dir_all(&paths.manager_dir);
        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }

    fn create_test_db(path: &Path, id: &str) -> Result<()> {
        let conn = Connection::open(path)?;
        conn.execute(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )?;
        conn.execute("INSERT INTO threads (id, value) VALUES (?1, ?2)", [id, id])?;
        Ok(())
    }
}
