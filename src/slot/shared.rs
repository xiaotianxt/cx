use std::path::PathBuf;

use crate::paths::ManagerPaths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedResourceKind {
    RegularFile,
    AppendOnlyFile,
    Directory,
    SqliteDatabase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotCreationPolicy {
    AlwaysLink,
    LinkWhenCanonicalExists,
    RepairOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SharedResource {
    name: &'static str,
    kind: SharedResourceKind,
    creation_policy: SlotCreationPolicy,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SharedProfile {
    resources: &'static [SharedResource],
}

impl SharedResource {
    const fn new(
        name: &'static str,
        kind: SharedResourceKind,
        creation_policy: SlotCreationPolicy,
    ) -> Self {
        Self {
            name,
            kind,
            creation_policy,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        self.name
    }

    pub(crate) fn kind(self) -> SharedResourceKind {
        self.kind
    }

    pub(crate) fn creation_policy(self) -> SlotCreationPolicy {
        self.creation_policy
    }

    pub(crate) fn canonical_path(self, paths: &ManagerPaths) -> PathBuf {
        paths.base_codex_home.join(self.name)
    }

    pub(crate) fn slot_path(self, paths: &ManagerPaths, slot: &str) -> PathBuf {
        paths.slot_home(slot).join(self.name)
    }
}

impl SharedProfile {
    pub(crate) fn codex_slot_default() -> Self {
        Self {
            resources: CODEX_SLOT_SHARED_RESOURCES,
        }
    }

    pub(crate) fn resources(self) -> &'static [SharedResource] {
        self.resources
    }
}

const CODEX_SLOT_SHARED_RESOURCES: &[SharedResource] = &[
    SharedResource::new(
        "config.toml",
        SharedResourceKind::RegularFile,
        SlotCreationPolicy::AlwaysLink,
    ),
    SharedResource::new(
        "AGENTS.md",
        SharedResourceKind::RegularFile,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "accounts",
        SharedResourceKind::Directory,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "current",
        SharedResourceKind::RegularFile,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "history.jsonl",
        SharedResourceKind::AppendOnlyFile,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "memories",
        SharedResourceKind::Directory,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "models_cache.json",
        SharedResourceKind::RegularFile,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "plugins",
        SharedResourceKind::Directory,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "prompts",
        SharedResourceKind::Directory,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "rules",
        SharedResourceKind::Directory,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "session_index.jsonl",
        SharedResourceKind::AppendOnlyFile,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "sessions",
        SharedResourceKind::Directory,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "shell_snapshots",
        SharedResourceKind::Directory,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "skills",
        SharedResourceKind::Directory,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "skills-data",
        SharedResourceKind::Directory,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "installation_id",
        SharedResourceKind::RegularFile,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "vendor_imports",
        SharedResourceKind::Directory,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "version.json",
        SharedResourceKind::RegularFile,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "state_5.sqlite",
        SharedResourceKind::SqliteDatabase,
        SlotCreationPolicy::RepairOnly,
    ),
    SharedResource::new(
        "logs_2.sqlite",
        SharedResourceKind::SqliteDatabase,
        SlotCreationPolicy::RepairOnly,
    ),
];
