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
use serde::Deserialize;
use serde::Serialize;

use crate::cli::TransferExportArgs;
use crate::cli::TransferImportArgs;
use crate::paths::ManagerPaths;
use crate::slot;

// TODO: Add focused transfer round-trip tests once the current app-server/channel
// changes compile again.
const MANIFEST_NAME: &str = "cx-transfer.json";
const MANIFEST_SCHEMA_VERSION: u32 = 1;

const BASE_ITEMS: &[&str] = &[
    "auth.json",
    "accounts",
    "current",
    "config.toml",
    "AGENTS.md",
    "installation_id",
    "models_cache.json",
    "session_index.jsonl",
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

const SLOT_ROOT_FILES: &[&str] = &["env.conf", "overrides.conf"];

const SLOT_HOME_FILES: &[&str] = &[
    "auth.json",
    ".personality_migration",
    ".codex-global-state.json",
    ".codex-global-state.json.bak",
];

const SLOT_HOME_DIRS: &[&str] = &["sqlite"];

#[derive(Debug, Serialize, Deserialize)]
struct TransferManifest {
    schema_version: u32,
    created_unix_seconds: u64,
    slots: Vec<String>,
    base_items: Vec<String>,
    manager_items: Vec<String>,
}

pub fn export(args: TransferExportArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.manager_dir.clone())?;
    let output_dir = absolute_path(&args.out)?;
    ensure_export_destination(&paths, &output_dir, args.replace)?;

    let slots = export_slots(&paths, &args.slots)?;
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

    let base_items = copy_named_items(&paths.base_codex_home, bundle.base_dir(), BASE_ITEMS, true)?;
    let manager_items = copy_named_items(
        &paths.manager_dir,
        bundle.profile_manager_dir(),
        MANAGER_ITEMS,
        true,
    )?;
    write_export_rotation(bundle.profile_manager_dir(), &slots)?;
    export_slots_to_bundle(&paths, &bundle, &slots)?;
    write_manifest(&bundle, &slots, base_items, manager_items)?;

    println!("exported transfer bundle: {}", bundle.root.display());
    println!("slots: {}", slots.join(", "));
    println!("warning: bundle contains live Codex credentials; keep it private");
    Ok(())
}

pub fn import(args: TransferImportArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.manager_dir.clone())?;
    let bundle = TransferBundle::new(absolute_path(&args.bundle)?);
    let manifest = read_manifest(&bundle)?;
    validate_manifest(&manifest)?;
    validate_bundle_sources(&bundle, &manifest)?;

    if !args.replace {
        preflight_import(&paths, &bundle, &manifest)?;
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
    import_slots_from_bundle(&paths, &bundle, &manifest.slots, args.replace)?;

    println!("imported transfer bundle: {}", bundle.root.display());
    println!("slots: {}", manifest.slots.join(", "));
    println!("run `cx doctor` to verify the destination layout");
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
        let auth_path = paths.slot_home(&slot).join("auth.json");
        let auth_meta = fs::symlink_metadata(&auth_path)
            .with_context(|| format!("stat slot auth {}", auth_path.display()))?;
        if !auth_meta.file_type().is_file() {
            anyhow::bail!("slot auth must be a private file: {}", auth_path.display());
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
        if !source_slot.join("home/auth.json").is_file() {
            anyhow::bail!(
                "bundle missing slot auth: {}",
                source_slot.join("home/auth.json").display()
            );
        }
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
