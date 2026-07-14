use std::path::PathBuf;

use crate::paths::ManagerPaths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedResourceKind {
    RegularFile,
    AppendOnlyFile,
    Directory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotCreationPolicy {
    AlwaysLink,
    LinkWhenCanonicalExists,
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

    pub(crate) fn is_blacklisted(name: &str) -> bool {
        CODEX_SLOT_PRIVATE_RESOURCES.contains(&name) || name.starts_with("config.toml.bak-")
    }

    pub(crate) fn is_known_shared(name: &str) -> bool {
        Self::codex_slot_default()
            .resources()
            .iter()
            .any(|resource| resource.name() == name)
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
        "models_catalog_static.json",
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
        "hooks.json",
        SharedResourceKind::RegularFile,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
    SharedResource::new(
        "herdr-agent-state.sh",
        SharedResourceKind::RegularFile,
        SlotCreationPolicy::LinkWhenCanonicalExists,
    ),
];

/// Files and directories that must stay slot-private and must never be
/// symlinked from the base CODEX_HOME.
const CODEX_SLOT_PRIVATE_RESOURCES: &[&str] = &[
    "auth.json",
    "auth.json.oauth.bak",
    "keychain.conf",
    "keychain-meta.json",
    "overrides.conf",
    "env.conf",
    "sqlite",
    ".tmp",
    "tmp",
    "log",
    "cache",
    "computer-use",
    ".codex-global-state.json",
    ".codex-global-state.json.bak",
    ".personality_migration",
    "state_5.sqlite",
    "state_5.sqlite-shm",
    "state_5.sqlite-wal",
    "logs_2.sqlite",
    "logs_2.sqlite-shm",
    "logs_2.sqlite-wal",
    "goals_1.sqlite",
    "goals_1.sqlite-shm",
    "goals_1.sqlite-wal",
    "memories_1.sqlite",
    "memories_1.sqlite-shm",
    "memories_1.sqlite-wal",
    "profile-manager",
    "cxrun",
    "archived_sessions",
    "attachments",
    "bin",
    "process_manager",
];
