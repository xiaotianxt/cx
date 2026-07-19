use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use rusqlite::Connection;
use serde::Deserialize;
use serde::Serialize;

use crate::cli::TransferExportArgs;
use crate::cli::TransferImportArgs;
use crate::paths::ManagerPaths;
use crate::slot;

// TODO: Add focused transfer round-trip tests.
const MANIFEST_NAME: &str = "cx-transfer.json";
const MANIFEST_SCHEMA_VERSION: u32 = 1;

const BASE_ITEMS: &[&str] = &[
    "auth.json",
    "accounts",
    "current",
    "config.toml",
    "AGENTS.md",
    "installation_id",
    "keychain.conf",
    "models_cache.json",
    "session_index.jsonl",
    "sessions",
    "sqlite",
    "version.json",
];

const BASE_COPY_ITEMS: &[&str] = &[
    "auth.json",
    "accounts",
    "current",
    "config.toml",
    "AGENTS.md",
    "installation_id",
    "keychain.conf",
    "models_cache.json",
    "session_index.jsonl",
    "sessions",
    "version.json",
];

const MANAGER_ITEMS: &[&str] = &[
    "rotation.txt",
    "profiles",
    "targets",
    "state",
    "stats-calibration.json",
    "price-cache.json",
];

const SLOT_ROOT_FILES: &[&str] = &["env.conf", "overrides.conf", "keychain.conf"];

const SLOT_HOME_FILES: &[&str] = &[
    "auth.json",
    ".codex-global-state.json",
    ".codex-global-state.json.bak",
];

const SLOT_HOME_DIRS: &[&str] = &[];

#[derive(Debug, Serialize, Deserialize)]
struct TransferManifest {
    schema_version: u32,
    created_unix_seconds: u64,
    slots: Vec<String>,
    base_items: Vec<String>,
    manager_items: Vec<String>,
}

pub fn export_with_paths(paths: &ManagerPaths, args: TransferExportArgs) -> Result<()> {
    let output_dir = absolute_path(&args.out)?;
    ensure_export_destination(paths, &output_dir, args.replace)?;

    let slots = export_slots(paths, &args.slots)?;
    let bundle = TransferBundle::new(output_dir);
    fs::create_dir_all(bundle.base_dir()).with_context(|| {
        format!(
            "create transfer bundle base directory {}",
            bundle.base_dir().display()
        )
    })?;
    fs::create_dir_all(bundle.profile_manager_dir()).with_context(|| {
        format!(
            "create transfer bundle profile-manager directory {}",
            bundle.profile_manager_dir().display()
        )
    })?;

    let mut base_items = copy_named_items(
        &paths.base_codex_home,
        bundle.base_dir(),
        BASE_COPY_ITEMS,
        true,
    )?;
    let sqlite_home = paths.shared_sqlite_home();
    if sqlite_home.is_dir() {
        snapshot_sqlite_home(&sqlite_home, &bundle.base_dir().join("sqlite"))?;
        base_items.push("sqlite".to_string());
    }
    let manager_items = copy_named_items(
        &paths.manager_dir,
        bundle.profile_manager_dir(),
        MANAGER_ITEMS,
        true,
    )?;
    write_export_rotation(bundle.profile_manager_dir(), &slots)?;
    export_slots_to_bundle(paths, &bundle, &slots)?;
    write_manifest(&bundle, &slots, base_items, manager_items)?;

    println!("exported transfer bundle: {}", bundle.root.display());
    println!("slots: {}", slots.join(", "));
    println!("warning: bundle contains live Codex credentials; keep it private");
    println!(
        "warning: keychain.conf entries are local PAT fallback references; recreate matching Keychain items on the destination before relying on PAT fallback"
    );
    Ok(())
}

pub fn import_with_paths(paths: &ManagerPaths, args: TransferImportArgs) -> Result<()> {
    let bundle = TransferBundle::new(absolute_path(&args.bundle)?);
    let manifest = read_manifest(&bundle)?;
    validate_manifest(&manifest)?;
    validate_bundle_sources(&bundle, &manifest)?;

    if !args.replace {
        preflight_import(paths, &bundle, &manifest)?;
    }

    fs::create_dir_all(&paths.base_codex_home)
        .with_context(|| format!("create {}", paths.base_codex_home.display()))?;
    fs::create_dir_all(&paths.manager_dir)
        .with_context(|| format!("create {}", paths.manager_dir.display()))?;

    import_named_items(
        bundle.base_dir(),
        &paths.base_codex_home,
        BASE_ITEMS,
        args.replace,
    )?;
    import_named_items(
        bundle.profile_manager_dir(),
        &paths.manager_dir,
        MANAGER_ITEMS,
        args.replace,
    )?;
    import_slots_from_bundle(paths, &bundle, &manifest.slots, args.replace)?;
    crate::upgrade::run_after_import(paths)?;

    println!("imported transfer bundle: {}", bundle.root.display());
    println!("slots: {}", manifest.slots.join(", "));
    println!("run `cx doctor` to verify the destination layout");
    println!(
        "if imported slots use keychain.conf, create the referenced Keychain items on this machine before relying on PAT fallback"
    );
    Ok(())
}

struct TransferBundle {
    root: PathBuf,
    base_dir: PathBuf,
    profile_manager_dir: PathBuf,
}

impl TransferBundle {
    fn new(root: PathBuf) -> Self {
        let base_dir = root.join("base");
        let profile_manager_dir = root.join("profile-manager");
        Self {
            root,
            base_dir,
            profile_manager_dir,
        }
    }

    fn manifest_file(&self) -> PathBuf {
        self.root.join(MANIFEST_NAME)
    }

    fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    fn profile_manager_dir(&self) -> &Path {
        &self.profile_manager_dir
    }

    fn slot_dir(&self, slot: &str) -> PathBuf {
        self.profile_manager_dir().join("slots").join(slot)
    }
}

fn ensure_export_destination(paths: &ManagerPaths, output_dir: &Path, replace: bool) -> Result<()> {
    ensure_outside_live_tree(output_dir, &paths.base_codex_home)?;
    ensure_outside_live_tree(output_dir, &paths.manager_dir)?;

    match fs::symlink_metadata(output_dir) {
        Ok(meta) if !meta.file_type().is_dir() => {
            anyhow::bail!(
                "export destination is not a directory: {}",
                output_dir.display()
            )
        }
        Ok(_) if replace => {
            fs::remove_dir_all(output_dir)
                .with_context(|| format!("remove {}", output_dir.display()))?;
        }
        Ok(_) if !dir_is_empty(output_dir)? => {
            anyhow::bail!(
                "export destination is not empty: {} (pass --replace to recreate it)",
                output_dir.display()
            );
        }
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("stat {}", output_dir.display())),
    }
    Ok(())
}

fn ensure_outside_live_tree(path: &Path, live_root: &Path) -> Result<()> {
    let live_root = absolute_path(live_root)?;
    if path == live_root || path.starts_with(&live_root) {
        anyhow::bail!(
            "export destination must be outside the live Codex tree: {}",
            live_root.display()
        );
    }
    Ok(())
}

fn export_slots(paths: &ManagerPaths, requested: &[String]) -> Result<Vec<String>> {
    let slots = if requested.is_empty() {
        slot::load_rotation(paths)?
    } else {
        requested.to_vec()
    };
    let slots: Vec<String> = slots.into_iter().filter(|s| s != "default").collect();
    if slots.is_empty() {
        anyhow::bail!("no slots to export");
    }
    validate_slots(paths, slots)
}

fn validate_slots(paths: &ManagerPaths, slots: Vec<String>) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for slot in slots {
        slot::validate_slot_name(&slot)?;
        if !seen.insert(slot.clone()) {
            continue;
        }
        let slot_dir = paths.slot_dir(&slot);
        let slot_meta = fs::symlink_metadata(&slot_dir)
            .with_context(|| format!("stat slot directory {}", slot_dir.display()))?;
        if !slot_meta.file_type().is_dir() {
            anyhow::bail!("slot must be a directory: {}", slot_dir.display());
        }
        let slot_home = paths.slot_home(&slot);
        let slot_home_meta = fs::symlink_metadata(&slot_home)
            .with_context(|| format!("stat slot home {}", slot_home.display()))?;
        if !slot_home_meta.file_type().is_dir() {
            anyhow::bail!("slot home must be a directory: {}", slot_home.display());
        }
        let auth_path = paths.slot_home(&slot).join("auth.json");
        match fs::symlink_metadata(&auth_path) {
            Ok(auth_meta) if !auth_meta.file_type().is_file() => {
                anyhow::bail!("slot auth must be a private file: {}", auth_path.display());
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("stat {}", auth_path.display())),
        }
        unique.push(slot);
    }
    Ok(unique)
}

fn copy_named_items(
    source_root: &Path,
    dest_root: &Path,
    names: &[&str],
    replace: bool,
) -> Result<Vec<String>> {
    let mut copied = Vec::new();
    for name in names {
        let source = source_root.join(name);
        if source.exists() {
            copy_entry(&source, &dest_root.join(name), replace)?;
            copied.push((*name).to_string());
        }
    }
    Ok(copied)
}

fn snapshot_sqlite_home(source: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        remove_path(dest)?;
    }
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for entry in fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name_text = file_name.to_string_lossy();
        if file_name_text.ends_with("-wal") || file_name_text.ends_with("-shm") {
            continue;
        }
        let destination = dest.join(&file_name);
        if path.extension().and_then(|extension| extension.to_str()) == Some("sqlite") {
            let conn =
                Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .with_context(|| format!("open {} for transfer snapshot", path.display()))?;
            conn.execute("VACUUM INTO ?1", [destination.display().to_string()])
                .with_context(|| {
                    format!(
                        "snapshot SQLite database {} to {}",
                        path.display(),
                        destination.display()
                    )
                })?;
            let permissions = fs::metadata(&path)?.permissions();
            fs::set_permissions(&destination, permissions)?;
        } else {
            copy_entry(&path, &destination, true)?;
        }
    }
    Ok(())
}

fn import_named_items(
    source_root: &Path,
    dest_root: &Path,
    names: &[&str],
    replace: bool,
) -> Result<()> {
    for name in names {
        let source = source_root.join(name);
        if source.exists() {
            copy_entry(&source, &dest_root.join(name), replace)?;
        }
    }
    Ok(())
}

fn write_export_rotation(profile_manager_dir: &Path, slots: &[String]) -> Result<()> {
    let content = slots.join("\n") + "\n";
    fs::write(profile_manager_dir.join("rotation.txt"), content).with_context(|| {
        format!(
            "write {}",
            profile_manager_dir.join("rotation.txt").display()
        )
    })
}

fn export_slots_to_bundle(
    paths: &ManagerPaths,
    bundle: &TransferBundle,
    slots: &[String],
) -> Result<()> {
    for slot in slots {
        let source_slot = paths.slot_dir(slot);
        let dest_slot = bundle.slot_dir(slot);
        fs::create_dir_all(dest_slot.join("home"))
            .with_context(|| format!("create {}", dest_slot.join("home").display()))?;
        copy_named_items(&source_slot, &dest_slot, SLOT_ROOT_FILES, true)?;
        copy_named_items(
            &source_slot.join("home"),
            &dest_slot.join("home"),
            SLOT_HOME_FILES,
            true,
        )?;
        copy_named_items(
            &source_slot.join("home"),
            &dest_slot.join("home"),
            SLOT_HOME_DIRS,
            true,
        )?;
    }
    Ok(())
}

fn import_slots_from_bundle(
    paths: &ManagerPaths,
    bundle: &TransferBundle,
    slots: &[String],
    replace: bool,
) -> Result<()> {
    for slot in slots {
        slot::validate_slot_name(slot)?;
        let source_slot = bundle.slot_dir(slot);
        if !source_slot.is_dir() {
            anyhow::bail!("bundle missing slot directory: {}", source_slot.display());
        }
        copy_named_items(
            &source_slot,
            &paths.slot_dir(slot),
            SLOT_ROOT_FILES,
            replace,
        )?;
        copy_named_items(
            &source_slot.join("home"),
            &paths.slot_home(slot),
            SLOT_HOME_FILES,
            replace,
        )?;
        copy_named_items(
            &source_slot.join("home"),
            &paths.slot_home(slot),
            SLOT_HOME_DIRS,
            replace,
        )?;
        slot::repair_slot_layout(paths, slot)?;
    }
    Ok(())
}

fn write_manifest(
    bundle: &TransferBundle,
    slots: &[String],
    base_items: Vec<String>,
    manager_items: Vec<String>,
) -> Result<()> {
    let manifest = TransferManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        created_unix_seconds: unix_seconds()?,
        slots: slots.to_vec(),
        base_items,
        manager_items,
    };
    let content = serde_json::to_string_pretty(&manifest).context("serialize transfer manifest")?;
    fs::write(bundle.manifest_file(), content + "\n")
        .with_context(|| format!("write {}", bundle.manifest_file().display()))
}

fn read_manifest(bundle: &TransferBundle) -> Result<TransferManifest> {
    let content = fs::read_to_string(bundle.manifest_file())
        .with_context(|| format!("read {}", bundle.manifest_file().display()))?;
    serde_json::from_str(&content).context("parse transfer manifest")
}

fn validate_manifest(manifest: &TransferManifest) -> Result<()> {
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported transfer manifest version: {}",
            manifest.schema_version
        );
    }
    if manifest.slots.is_empty() {
        anyhow::bail!("transfer manifest has no slots");
    }
    for slot in &manifest.slots {
        slot::validate_slot_name(slot)?;
    }
    Ok(())
}

fn validate_bundle_sources(bundle: &TransferBundle, manifest: &TransferManifest) -> Result<()> {
    if !bundle.base_dir().is_dir() {
        anyhow::bail!(
            "bundle missing base directory: {}",
            bundle.base_dir().display()
        );
    }
    if !bundle.profile_manager_dir().is_dir() {
        anyhow::bail!(
            "bundle missing profile-manager directory: {}",
            bundle.profile_manager_dir().display()
        );
    }
    check_named_sources(bundle.base_dir(), BASE_ITEMS)?;
    check_named_sources(bundle.profile_manager_dir(), MANAGER_ITEMS)?;
    for slot in &manifest.slots {
        let source_slot = bundle.slot_dir(slot);
        if !source_slot.is_dir() {
            anyhow::bail!("bundle missing slot directory: {}", source_slot.display());
        }
        check_named_sources(&source_slot, SLOT_ROOT_FILES)?;
        check_named_sources(&source_slot.join("home"), SLOT_HOME_FILES)?;
        check_named_sources(&source_slot.join("home"), SLOT_HOME_DIRS)?;
    }
    Ok(())
}

fn check_named_sources(source_root: &Path, names: &[&str]) -> Result<()> {
    for name in names {
        let source = source_root.join(name);
        if source.exists() {
            check_copy_source(&source)?;
        }
    }
    Ok(())
}

fn check_copy_source(source: &Path) -> Result<()> {
    let source_meta =
        fs::symlink_metadata(source).with_context(|| format!("stat {}", source.display()))?;
    if source_meta.file_type().is_symlink() {
        anyhow::bail!(
            "transfer bundle contains unsupported symlink: {}",
            source.display()
        );
    }
    if source_meta.file_type().is_dir() {
        for entry in fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
            let entry = entry?;
            check_copy_source(&entry.path())?;
        }
    }
    Ok(())
}

fn preflight_import(
    paths: &ManagerPaths,
    bundle: &TransferBundle,
    manifest: &TransferManifest,
) -> Result<()> {
    check_named_item_conflicts(bundle.base_dir(), &paths.base_codex_home, BASE_ITEMS)?;
    check_named_item_conflicts(
        bundle.profile_manager_dir(),
        &paths.manager_dir,
        MANAGER_ITEMS,
    )?;
    for slot in &manifest.slots {
        let source_slot = bundle.slot_dir(slot);
        check_named_item_conflicts(&source_slot, &paths.slot_dir(slot), SLOT_ROOT_FILES)?;
        check_named_item_conflicts(
            &source_slot.join("home"),
            &paths.slot_home(slot),
            SLOT_HOME_FILES,
        )?;
        check_named_item_conflicts(
            &source_slot.join("home"),
            &paths.slot_home(slot),
            SLOT_HOME_DIRS,
        )?;
    }
    Ok(())
}

fn check_named_item_conflicts(source_root: &Path, dest_root: &Path, names: &[&str]) -> Result<()> {
    for name in names {
        let source = source_root.join(name);
        if source.exists() {
            check_copy_conflicts(&source, &dest_root.join(name))?;
        }
    }
    Ok(())
}

fn check_copy_conflicts(source: &Path, dest: &Path) -> Result<()> {
    let source_meta =
        fs::symlink_metadata(source).with_context(|| format!("stat {}", source.display()))?;
    if source_meta.file_type().is_symlink() {
        anyhow::bail!(
            "transfer bundle contains unsupported symlink: {}",
            source.display()
        );
    }
    match fs::symlink_metadata(dest) {
        Ok(dest_meta) if source_meta.file_type().is_dir() && dest_meta.file_type().is_dir() => {}
        Ok(_) => {
            anyhow::bail!(
                "destination already exists: {} (pass --replace to overwrite)",
                dest.display()
            );
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("stat {}", dest.display())),
    }
    if source_meta.file_type().is_dir() {
        for entry in fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
            let entry = entry?;
            check_copy_conflicts(&entry.path(), &dest.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn copy_entry(source: &Path, dest: &Path, replace: bool) -> Result<()> {
    let source_meta =
        fs::symlink_metadata(source).with_context(|| format!("stat {}", source.display()))?;
    if source_meta.file_type().is_symlink() {
        anyhow::bail!(
            "transfer bundle does not support symlinks: {}",
            source.display()
        );
    }
    if source_meta.file_type().is_dir() {
        copy_dir_contents(source, dest, replace)?;
    } else if source_meta.file_type().is_file() {
        copy_file(source, dest, replace)?;
    } else {
        anyhow::bail!("unsupported transfer file type: {}", source.display());
    }
    Ok(())
}

fn copy_dir_contents(source: &Path, dest: &Path, replace: bool) -> Result<()> {
    match fs::symlink_metadata(dest) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) if replace => remove_path(dest)?,
        Ok(_) => {
            anyhow::bail!(
                "destination already exists: {} (pass --replace to overwrite)",
                dest.display()
            );
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("stat {}", dest.display())),
    }
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for entry in fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry?;
        copy_entry(&entry.path(), &dest.join(entry.file_name()), replace)?;
    }
    Ok(())
}

fn copy_file(source: &Path, dest: &Path, replace: bool) -> Result<()> {
    match fs::symlink_metadata(dest) {
        Ok(_) if replace => remove_path(dest)?,
        Ok(_) => {
            anyhow::bail!(
                "destination already exists: {} (pass --replace to overwrite)",
                dest.display()
            );
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("stat {}", dest.display())),
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::copy(source, dest)
        .with_context(|| format!("copy {} to {}", source.display(), dest.display()))?;
    let permissions = fs::metadata(source)
        .with_context(|| format!("stat {}", source.display()))?
        .permissions();
    fs::set_permissions(dest, permissions)
        .with_context(|| format!("set permissions on {}", dest.display()))?;
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.file_type().is_dir() {
        fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
    } else {
        fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

fn dir_is_empty(path: &Path) -> Result<bool> {
    Ok(fs::read_dir(path)
        .with_context(|| format!("read {}", path.display()))?
        .next()
        .is_none())
}

fn unix_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("read current directory")?
            .join(path)
    };
    Ok(normalize_path(&absolute))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use super::*;
    use crate::cli::TransferExportArgs;
    use crate::cli::TransferImportArgs;

    #[test]
    fn transfer_round_trip_allows_slot_without_auth() {
        let root = temp_root("no-auth-slot");
        let _ = fs::remove_dir_all(&root);

        let source = ManagerPaths::from_roots(
            root.join("source/.codex"),
            root.join("source/.codex/profile-manager"),
        );
        let dest = ManagerPaths::from_roots(
            root.join("dest/.codex"),
            root.join("dest/.codex/profile-manager"),
        );
        let bundle = root.join("bundle");

        fs::create_dir_all(source.slot_home("deepseek")).unwrap();
        fs::write(
            source.base_codex_home.join("config.toml"),
            "model = \"test\"\n",
        )
        .unwrap();
        fs::write(source.rotation_file.as_path(), "deepseek\n").unwrap();
        fs::write(
            source.slot_dir("deepseek").join("env.conf"),
            "export DEEPSEEK_API_KEY=\"redacted\"\n",
        )
        .unwrap();
        fs::write(
            source.slot_dir("deepseek").join("overrides.conf"),
            "model_provider=\"deepseek\"\n",
        )
        .unwrap();

        export_with_paths(
            &source,
            TransferExportArgs {
                manager_dir: None,
                out: bundle.clone(),
                replace: false,
                slots: Vec::new(),
            },
        )
        .unwrap();

        assert!(bundle
            .join("profile-manager/slots/deepseek/env.conf")
            .is_file());
        assert!(!bundle
            .join("profile-manager/slots/deepseek/home/auth.json")
            .exists());

        import_with_paths(
            &dest,
            TransferImportArgs {
                manager_dir: None,
                replace: false,
                bundle,
            },
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(dest.slot_dir("deepseek").join("overrides.conf")).unwrap(),
            "model_provider=\"deepseek\"\n"
        );
        assert!(!dest.slot_home("deepseek").join("auth.json").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sqlite_snapshot_captures_wal_transactions_without_sidecars() {
        let root = temp_root("sqlite-wal-snapshot");
        let source = root.join("source");
        let dest = root.join("dest");
        fs::create_dir_all(&source).unwrap();
        let source_db = source.join("state_5.sqlite");
        let conn = Connection::open(&source_db).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             CREATE TABLE threads (id TEXT PRIMARY KEY);
             INSERT INTO threads VALUES ('from-wal');",
        )
        .unwrap();

        snapshot_sqlite_home(&source, &dest).unwrap();

        let snapshot = Connection::open(dest.join("state_5.sqlite")).unwrap();
        let id: String = snapshot
            .query_row("SELECT id FROM threads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(id, "from-wal");
        assert!(!dest.join("state_5.sqlite-wal").exists());
        assert!(!dest.join("state_5.sqlite-shm").exists());

        drop(conn);
        let _ = fs::remove_dir_all(root);
    }

    fn temp_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "cx-transfer-test-{name}-{}-{nanos}",
            std::process::id()
        ))
    }
}
