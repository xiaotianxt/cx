use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;

use crate::cli::AddArgs;
use crate::cli::RemoveArgs;
use crate::envfile;
use crate::paths::ManagerPaths;

mod config;
mod rotation;
mod shared;

pub use config::read_config_string;
pub use config::read_override_lines;
pub use config::read_override_string;
pub use rotation::load_rotation;
use shared::SharedProfile;
use shared::SharedResource;
use shared::SharedResourceKind;
use shared::SlotCreationPolicy;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotLayoutAudit {
    pub slot: String,
    pub home_exists: bool,
    pub auth_exists: bool,
    pub issues: Vec<SlotLayoutIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotLayoutIssue {
    pub path: PathBuf,
    pub message: String,
}

struct SlotMaterializer<'a> {
    paths: &'a ManagerPaths,
    slot: &'a str,
    slot_home: PathBuf,
    profile: SharedProfile,
}

impl<'a> SlotMaterializer<'a> {
    fn new(paths: &'a ManagerPaths, slot: &'a str) -> Result<Self> {
        validate_slot_name(slot)?;
        Ok(Self {
            paths,
            slot,
            slot_home: paths.slot_home(slot),
            profile: SharedProfile::codex_slot_default(),
        })
    }

    fn create_home(&self) -> Result<()> {
        fs::create_dir_all(&self.slot_home)
            .with_context(|| format!("create {}", self.slot_home.display()))
    }

    fn ensure_for_slot_creation(&self) -> Result<()> {
        self.create_home()?;
        for resource in self.profile.resources() {
            match resource.creation_policy() {
                SlotCreationPolicy::AlwaysLink => self.link_resource_if_needed(*resource)?,
                SlotCreationPolicy::LinkWhenCanonicalExists => {
                    if resource.canonical_path(self.paths).exists() {
                        self.link_resource_if_needed(*resource)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn repair(&self) -> Result<()> {
        self.create_home()?;
        for resource in self.profile.resources() {
            self.repair_resource(*resource)?;
        }
        Ok(())
    }

    fn audit(&self) -> Result<SlotLayoutAudit> {
        let auth_path = self.slot_home.join("auth.json");
        let mut issues = Vec::new();

        let home_exists = self.slot_home.is_dir();
        if !home_exists {
            issues.push(SlotLayoutIssue {
                path: self.slot_home.clone(),
                message: "missing slot home".to_string(),
            });
            return Ok(SlotLayoutAudit {
                slot: self.slot.to_string(),
                home_exists,
                auth_exists: false,
                issues,
            });
        }

        let auth_exists = auth_path.exists();
        if !auth_exists {
            issues.push(SlotLayoutIssue {
                path: auth_path.clone(),
                message: "missing slot auth.json".to_string(),
            });
        } else if is_symlink(&auth_path) {
            issues.push(SlotLayoutIssue {
                path: auth_path.clone(),
                message: "auth.json should be slot-private, not a symlink".to_string(),
            });
        }

        for resource in self.profile.resources() {
            self.audit_resource(*resource, &mut issues)?;
        }

        Ok(SlotLayoutAudit {
            slot: self.slot.to_string(),
            home_exists,
            auth_exists,
            issues,
        })
    }

    fn repair_resource(&self, resource: SharedResource) -> Result<()> {
        let canonical_path = resource.canonical_path(self.paths);
        let slot_path = resource.slot_path(self.paths, self.slot);

        if resource.kind() == SharedResourceKind::Directory {
            fs::create_dir_all(&canonical_path)
                .with_context(|| format!("create {}", canonical_path.display()))?;
        }

        if is_symlink_to(&slot_path, &canonical_path)? {
            return Ok(());
        }

        if is_symlink(&slot_path) {
            fs::remove_file(&slot_path)
                .with_context(|| format!("remove {}", slot_path.display()))?;
            return link_if_safe(&canonical_path, &slot_path);
        }

        if slot_path.exists() {
            merge_shared_resource(resource, &canonical_path, &slot_path)?;
            remove_path_if_exists(&slot_path)?;
        }

        if canonical_path.exists() || resource.kind() != SharedResourceKind::Directory {
            link_if_safe(&canonical_path, &slot_path)?;
        }

        Ok(())
    }

    fn audit_resource(
        &self,
        resource: SharedResource,
        issues: &mut Vec<SlotLayoutIssue>,
    ) -> Result<()> {
        let canonical_path = resource.canonical_path(self.paths);
        let slot_path = resource.slot_path(self.paths, self.slot);
        let canonical_exists = canonical_path.exists() || is_symlink(&canonical_path);
        let slot_exists = slot_path.exists() || is_symlink(&slot_path);

        if !canonical_exists && !slot_exists {
            return Ok(());
        }

        if !slot_exists {
            issues.push(SlotLayoutIssue {
                path: slot_path,
                message: format!("missing shared link to {}", canonical_path.display()),
            });
            return Ok(());
        }

        if !is_symlink(&slot_path) {
            issues.push(SlotLayoutIssue {
                path: slot_path,
                message: format!(
                    "private copy; expected symlink to {}",
                    canonical_path.display()
                ),
            });
            return Ok(());
        }

        if !is_symlink_to(&slot_path, &canonical_path)? {
            let target = fs::read_link(&slot_path)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| "<unreadable>".to_string());
            issues.push(SlotLayoutIssue {
                path: slot_path,
                message: format!("points to {target}; expected {}", canonical_path.display()),
            });
        }

        Ok(())
    }

    fn link_resource_if_needed(&self, resource: SharedResource) -> Result<()> {
        link_if_needed(
            &resource.canonical_path(self.paths),
            &resource.slot_path(self.paths, self.slot),
        )
    }
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
    SlotMaterializer::new(paths, slot)?.ensure_for_slot_creation()
}

pub fn repair_slot_layout(paths: &ManagerPaths, slot: &str) -> Result<()> {
    SlotMaterializer::new(paths, slot)?.repair()
}

pub fn audit_slot_layout(paths: &ManagerPaths, slot: &str) -> Result<SlotLayoutAudit> {
    SlotMaterializer::new(paths, slot)?.audit()
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

pub(crate) fn validate_slot_name(slot: &str) -> Result<()> {
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

fn merge_shared_resource(
    resource: SharedResource,
    canonical_path: &Path,
    slot_path: &Path,
) -> Result<()> {
    if resource.kind() == SharedResourceKind::Directory {
        merge_dir_missing(slot_path, canonical_path)?;
    } else if resource.kind() == SharedResourceKind::AppendOnlyFile && canonical_path.exists() {
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
    fn ensure_slot_layout_uses_creation_policy() {
        let paths = temp_paths("ensure-policy");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        fs::write(paths.base_codex_home.join("config.toml"), "").unwrap();
        fs::write(paths.base_codex_home.join("history.jsonl"), "base\n").unwrap();
        fs::write(paths.base_codex_home.join("state_5.sqlite"), "").unwrap();

        ensure_slot_layout(&paths, "dia1").unwrap();

        assert!(is_symlink_to(
            &paths.slot_home("dia1").join("config.toml"),
            &paths.base_codex_home.join("config.toml")
        )
        .unwrap());
        assert!(is_symlink_to(
            &paths.slot_home("dia1").join("history.jsonl"),
            &paths.base_codex_home.join("history.jsonl")
        )
        .unwrap());
        assert!(!paths.slot_home("dia1").join("state_5.sqlite").exists());

        let _ = fs::remove_dir_all(&paths.manager_dir);
        let _ = fs::remove_dir_all(&paths.base_codex_home);
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
    fn audit_slot_layout_accepts_shared_links() {
        let paths = temp_paths("audit-ok");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        fs::write(paths.base_codex_home.join("history.jsonl"), "base\n").unwrap();
        fs::create_dir_all(paths.slot_home("dia1")).unwrap();
        fs::write(paths.slot_home("dia1").join("auth.json"), "{}\n").unwrap();
        link_if_safe(
            &paths.base_codex_home.join("history.jsonl"),
            &paths.slot_home("dia1").join("history.jsonl"),
        )
        .unwrap();

        let audit = audit_slot_layout(&paths, "dia1").unwrap();

        assert!(audit.issues.is_empty());

        let _ = fs::remove_dir_all(&paths.manager_dir);
        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }

    #[test]
    fn audit_slot_layout_reports_private_shared_files() {
        let paths = temp_paths("audit-private");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        fs::write(paths.base_codex_home.join("history.jsonl"), "base\n").unwrap();
        fs::create_dir_all(paths.slot_home("dia1")).unwrap();
        fs::write(paths.slot_home("dia1").join("auth.json"), "{}\n").unwrap();
        fs::write(paths.slot_home("dia1").join("history.jsonl"), "slot\n").unwrap();

        let audit = audit_slot_layout(&paths, "dia1").unwrap();
        let messages = audit
            .issues
            .iter()
            .map(|issue| issue.message.as_str())
            .collect::<Vec<_>>();

        assert!(messages
            .iter()
            .any(|message| message.starts_with("private copy; expected symlink")));

        let _ = fs::remove_dir_all(&paths.manager_dir);
        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }
}
