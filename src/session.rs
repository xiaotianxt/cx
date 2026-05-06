use std::fmt;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use sha1::Digest;
use sha1::Sha1;

use crate::paths::ManagerPaths;

const SESSION_SCHEMA_VERSION: u64 = 1;
const JOURNAL_SCHEMA_VERSION: u64 = 1;
static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelId(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventId(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseToken(String);

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SessionStatus {
    Active,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum JournalEventKind {
    SessionCreated,
    LeaseAcquired,
    LeaseReleased,
    ChannelMessageReceived,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelLease {
    pub channel_id: ChannelId,
    pub lease_token: LeaseToken,
    pub epoch: u64,
    pub acquired_at_unix: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub schema_version: u64,
    pub session_id: SessionId,
    pub primary_channel_id: ChannelId,
    pub current_channel_id: ChannelId,
    pub status: SessionStatus,
    pub lease_epoch: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_lease: Option<ChannelLease>,
    pub created_at_unix: u64,
    pub updated_at_unix: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct JournalEvent {
    pub schema_version: u64,
    pub event_id: EventId,
    pub event_kind: JournalEventKind,
    pub session_id: SessionId,
    pub channel_id: ChannelId,
    pub occurred_at_unix: u64,
}

#[derive(Debug, Clone)]
pub struct CreateSessionRequest {
    pub session_id: Option<SessionId>,
    pub channel_id: ChannelId,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionResult {
    pub session: SessionRecord,
    pub event: JournalEvent,
}

#[derive(Debug, Clone)]
pub struct AcquireLeaseRequest {
    pub session_id: SessionId,
    pub channel_id: ChannelId,
    pub steal: bool,
}

#[derive(Debug, Clone)]
pub struct ReleaseLeaseRequest {
    pub session_id: SessionId,
    pub lease_token: LeaseToken,
}

#[derive(Debug, Clone)]
pub struct RecordChannelMessageRequest {
    pub session_id: SessionId,
    pub channel_id: ChannelId,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseResult {
    pub session: SessionRecord,
    pub event: JournalEvent,
}

impl SessionId {
    pub fn parse(raw: impl Into<String>) -> Result<Self> {
        let raw = raw.into();
        parse_id(raw, "session id", "sess_").map(Self)
    }

    fn generated(paths: &ManagerPaths) -> Result<Self> {
        let digest = stable_digest("session", paths, unix_now_nanos()?);
        Self::parse(format!("sess_{digest}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ChannelId {
    pub fn parse(raw: impl Into<String>) -> Result<Self> {
        let raw = raw.into();
        if raw.is_empty() {
            anyhow::bail!("channel id must not be empty");
        }
        if raw.len() > 128 {
            anyhow::bail!("channel id is too long");
        }
        if !raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
        {
            anyhow::bail!("channel id contains unsupported characters");
        }
        Ok(Self(raw))
    }
}

impl EventId {
    fn generated(paths: &ManagerPaths) -> Result<Self> {
        let digest = stable_digest("event", paths, unix_now_nanos()?);
        parse_id(format!("evt_{digest}"), "event id", "evt_").map(Self)
    }
}

impl LeaseToken {
    fn generated(paths: &ManagerPaths) -> Result<Self> {
        let digest = stable_digest("lease", paths, unix_now_nanos()?);
        parse_id(format!("lease_{digest}"), "lease token", "lease_").map(Self)
    }

    pub fn parse(raw: impl Into<String>) -> Result<Self> {
        parse_id(raw.into(), "lease token", "lease_").map(Self)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for LeaseToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for SessionId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(raw).map_err(serde::de::Error::custom)
    }
}

impl Serialize for ChannelId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ChannelId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(raw).map_err(serde::de::Error::custom)
    }
}

impl Serialize for EventId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for EventId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        parse_id(raw, "event id", "evt_")
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for LeaseToken {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for LeaseToken {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(raw).map_err(serde::de::Error::custom)
    }
}

pub fn create_session(
    paths: &ManagerPaths,
    request: CreateSessionRequest,
) -> Result<CreateSessionResult> {
    let now = unix_now_secs()?;
    let session_id = match request.session_id {
        Some(session_id) => session_id,
        None => SessionId::generated(paths)?,
    };
    let session = SessionRecord {
        schema_version: SESSION_SCHEMA_VERSION,
        session_id,
        primary_channel_id: request.channel_id.clone(),
        current_channel_id: request.channel_id.clone(),
        status: SessionStatus::Active,
        lease_epoch: 0,
        active_lease: None,
        created_at_unix: now,
        updated_at_unix: now,
    };
    let event = JournalEvent {
        schema_version: JOURNAL_SCHEMA_VERSION,
        event_id: EventId::generated(paths)?,
        event_kind: JournalEventKind::SessionCreated,
        session_id: session.session_id.clone(),
        channel_id: request.channel_id,
        occurred_at_unix: now,
    };

    write_session_new(paths, &session)?;
    append_event(paths, &event)?;
    Ok(CreateSessionResult { session, event })
}

pub fn acquire_lease(paths: &ManagerPaths, request: AcquireLeaseRequest) -> Result<LeaseResult> {
    let now = unix_now_secs()?;
    let mut session = show_session(paths, &request.session_id)?;
    if session.active_lease.is_some() && !request.steal {
        anyhow::bail!(
            "session already has an active lease: {}",
            session.session_id
        );
    }
    let next_epoch = session
        .lease_epoch
        .checked_add(1)
        .context("lease epoch overflow")?;
    let lease = ChannelLease {
        channel_id: request.channel_id.clone(),
        lease_token: LeaseToken::generated(paths)?,
        epoch: next_epoch,
        acquired_at_unix: now,
    };
    session.current_channel_id = request.channel_id.clone();
    session.lease_epoch = next_epoch;
    session.active_lease = Some(lease);
    session.updated_at_unix = now;

    let event = JournalEvent {
        schema_version: JOURNAL_SCHEMA_VERSION,
        event_id: EventId::generated(paths)?,
        event_kind: JournalEventKind::LeaseAcquired,
        session_id: session.session_id.clone(),
        channel_id: request.channel_id,
        occurred_at_unix: now,
    };
    write_session_replace(paths, &session)?;
    append_event(paths, &event)?;
    Ok(LeaseResult { session, event })
}

pub fn release_lease(paths: &ManagerPaths, request: ReleaseLeaseRequest) -> Result<LeaseResult> {
    let now = unix_now_secs()?;
    let mut session = show_session(paths, &request.session_id)?;
    let Some(active_lease) = session.active_lease.clone() else {
        anyhow::bail!("session has no active lease: {}", session.session_id);
    };
    if active_lease.lease_token != request.lease_token {
        anyhow::bail!("lease token does not match active lease");
    }
    session.active_lease = None;
    session.updated_at_unix = now;

    let event = JournalEvent {
        schema_version: JOURNAL_SCHEMA_VERSION,
        event_id: EventId::generated(paths)?,
        event_kind: JournalEventKind::LeaseReleased,
        session_id: session.session_id.clone(),
        channel_id: active_lease.channel_id,
        occurred_at_unix: now,
    };
    write_session_replace(paths, &session)?;
    append_event(paths, &event)?;
    Ok(LeaseResult { session, event })
}

pub fn record_channel_message(
    paths: &ManagerPaths,
    request: RecordChannelMessageRequest,
) -> Result<JournalEvent> {
    let now = unix_now_secs()?;
    let _ = show_session(paths, &request.session_id)?;
    let event = JournalEvent {
        schema_version: JOURNAL_SCHEMA_VERSION,
        event_id: EventId::generated(paths)?,
        event_kind: JournalEventKind::ChannelMessageReceived,
        session_id: request.session_id,
        channel_id: request.channel_id,
        occurred_at_unix: now,
    };
    append_event(paths, &event)?;
    Ok(event)
}

pub fn list_sessions(paths: &ManagerPaths) -> Result<Vec<SessionRecord>> {
    let sessions_dir = paths.serve_sessions_dir();
    let entries = match fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("read {}", sessions_dir.display())),
    };

    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", sessions_dir.display()))?;
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        sessions.push(read_session_file(&entry.path())?);
    }
    sessions.sort_by(|left, right| {
        left.created_at_unix
            .cmp(&right.created_at_unix)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    Ok(sessions)
}

pub fn show_session(paths: &ManagerPaths, session_id: &SessionId) -> Result<SessionRecord> {
    read_session_file(&paths.serve_session_file(session_id.as_str()))
}

pub fn list_events(
    paths: &ManagerPaths,
    session_id: Option<&SessionId>,
) -> Result<Vec<JournalEvent>> {
    let journal_file = paths.serve_event_journal_file();
    let content = match fs::read_to_string(&journal_file) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("read {}", journal_file.display())),
    };

    let mut events = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str::<JournalEvent>(line)
            .with_context(|| format!("parse {} line {}", journal_file.display(), index + 1))?;
        if event.schema_version != JOURNAL_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported journal schema version: {}",
                event.schema_version
            );
        }
        if session_id.is_some_and(|session_id| &event.session_id != session_id) {
            continue;
        }
        events.push(event);
    }
    Ok(events)
}

fn write_session_new(paths: &ManagerPaths, session: &SessionRecord) -> Result<()> {
    fs::create_dir_all(paths.serve_sessions_dir())
        .with_context(|| format!("create {}", paths.serve_sessions_dir().display()))?;
    set_private_dir_permissions(&paths.serve_dir())?;
    set_private_dir_permissions(&paths.serve_sessions_dir())?;

    let session_file = paths.serve_session_file(session.session_id.as_str());
    if session_file.exists() {
        anyhow::bail!("session already exists: {}", session.session_id);
    }
    let tmp_file = paths
        .serve_sessions_dir()
        .join(format!("{}.tmp", session.session_id));
    let content = serde_json::to_vec_pretty(session).context("serialize session")?;
    let mut file = private_open_for_write(&tmp_file)?;
    file.write_all(&content)
        .with_context(|| format!("write {}", tmp_file.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("write {}", tmp_file.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", tmp_file.display()))?;
    fs::rename(&tmp_file, &session_file).with_context(|| {
        format!(
            "rename {} to {}",
            tmp_file.display(),
            session_file.display()
        )
    })?;
    set_private_file_permissions(&session_file)?;
    Ok(())
}

fn write_session_replace(paths: &ManagerPaths, session: &SessionRecord) -> Result<()> {
    fs::create_dir_all(paths.serve_sessions_dir())
        .with_context(|| format!("create {}", paths.serve_sessions_dir().display()))?;
    set_private_dir_permissions(&paths.serve_dir())?;
    set_private_dir_permissions(&paths.serve_sessions_dir())?;

    let session_file = paths.serve_session_file(session.session_id.as_str());
    let tmp_file = paths
        .serve_sessions_dir()
        .join(format!("{}.tmp", session.session_id));
    let content = serde_json::to_vec_pretty(session).context("serialize session")?;
    let mut file = private_open_for_write(&tmp_file)?;
    file.write_all(&content)
        .with_context(|| format!("write {}", tmp_file.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("write {}", tmp_file.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", tmp_file.display()))?;
    fs::rename(&tmp_file, &session_file).with_context(|| {
        format!(
            "rename {} to {}",
            tmp_file.display(),
            session_file.display()
        )
    })?;
    set_private_file_permissions(&session_file)?;
    Ok(())
}

fn read_session_file(path: &Path) -> Result<SessionRecord> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let session = serde_json::from_str::<SessionRecord>(&content)
        .with_context(|| format!("parse {}", path.display()))?;
    if session.schema_version != SESSION_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported session schema version: {}",
            session.schema_version
        );
    }
    Ok(session)
}

fn append_event(paths: &ManagerPaths, event: &JournalEvent) -> Result<()> {
    fs::create_dir_all(paths.serve_dir())
        .with_context(|| format!("create {}", paths.serve_dir().display()))?;
    set_private_dir_permissions(&paths.serve_dir())?;

    let journal_file = paths.serve_event_journal_file();
    let mut file = private_open_for_append(&journal_file)?;
    serde_json::to_writer(&mut file, event).context("serialize journal event")?;
    file.write_all(b"\n")
        .with_context(|| format!("write {}", journal_file.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", journal_file.display()))?;
    set_private_file_permissions(&journal_file)?;
    Ok(())
}

fn parse_id(raw: String, label: &str, prefix: &str) -> Result<String> {
    if raw.len() <= prefix.len() || !raw.starts_with(prefix) {
        anyhow::bail!("{label} must start with {prefix}");
    }
    if raw.len() > 96 {
        anyhow::bail!("{label} is too long");
    }
    let suffix = &raw[prefix.len()..];
    if !suffix
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        anyhow::bail!("{label} contains unsupported characters");
    }
    Ok(raw)
}

fn stable_digest(kind: &str, paths: &ManagerPaths, now_nanos: u128) -> String {
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha1::new();
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(paths.manager_dir.display().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(std::process::id().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(counter.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(now_nanos.to_string().as_bytes());
    let digest = hasher.finalize();
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn unix_now_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?
        .as_secs())
}

fn unix_now_nanos() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?
        .as_nanos())
}

fn private_open_for_write(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn private_open_for_append(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn set_private_dir_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", path.display()))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    Ok(())
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
            "cx-session-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
    }

    #[test]
    fn session_id_rejects_path_like_values() {
        let err = SessionId::parse("sess_bad/value").unwrap_err();

        assert!(format!("{err:#}").contains("unsupported"));
    }

    #[test]
    fn channel_id_accepts_adapter_prefix() {
        let channel = ChannelId::parse("telegram:12345").unwrap();

        assert_eq!(channel.to_string(), "telegram:12345");
    }

    #[test]
    fn create_session_writes_registry_and_journal() {
        let paths = temp_paths("create");

        let result = create_session(
            &paths,
            CreateSessionRequest {
                session_id: Some(SessionId::parse("sess_manual").unwrap()),
                channel_id: ChannelId::parse("terminal").unwrap(),
            },
        )
        .unwrap();

        assert_eq!(result.session.session_id.as_str(), "sess_manual");
        assert!(paths.serve_session_file("sess_manual").exists());
        let sessions = list_sessions(&paths).unwrap();
        assert_eq!(sessions.len(), 1);
        let journal = fs::read_to_string(paths.serve_event_journal_file()).unwrap();
        assert!(journal.contains("\"eventKind\":\"session-created\""));
        assert!(!journal.contains("auth"));
        assert!(!journal.contains("env"));

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn duplicate_session_id_is_rejected() {
        let paths = temp_paths("duplicate");
        let request = CreateSessionRequest {
            session_id: Some(SessionId::parse("sess_manual").unwrap()),
            channel_id: ChannelId::parse("terminal").unwrap(),
        };

        create_session(&paths, request.clone()).unwrap();
        let err = create_session(&paths, request).unwrap_err();

        assert!(format!("{err:#}").contains("already exists"));
        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn acquire_and_release_lease_updates_session_and_journal() {
        let paths = temp_paths("lease");
        create_session(
            &paths,
            CreateSessionRequest {
                session_id: Some(SessionId::parse("sess_manual").unwrap()),
                channel_id: ChannelId::parse("terminal").unwrap(),
            },
        )
        .unwrap();

        let acquired = acquire_lease(
            &paths,
            AcquireLeaseRequest {
                session_id: SessionId::parse("sess_manual").unwrap(),
                channel_id: ChannelId::parse("telegram:12345").unwrap(),
                steal: false,
            },
        )
        .unwrap();

        assert_eq!(acquired.session.lease_epoch, 1);
        assert_eq!(
            acquired.session.current_channel_id,
            ChannelId::parse("telegram:12345").unwrap()
        );
        let token = acquired
            .session
            .active_lease
            .as_ref()
            .unwrap()
            .lease_token
            .clone();

        let released = release_lease(
            &paths,
            ReleaseLeaseRequest {
                session_id: SessionId::parse("sess_manual").unwrap(),
                lease_token: token,
            },
        )
        .unwrap();

        assert_eq!(released.session.lease_epoch, 1);
        assert!(released.session.active_lease.is_none());
        let journal = fs::read_to_string(paths.serve_event_journal_file()).unwrap();
        assert!(journal.contains("\"eventKind\":\"lease-acquired\""));
        assert!(journal.contains("\"eventKind\":\"lease-released\""));

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn release_rejects_wrong_lease_token() {
        let paths = temp_paths("wrong-token");
        create_session(
            &paths,
            CreateSessionRequest {
                session_id: Some(SessionId::parse("sess_manual").unwrap()),
                channel_id: ChannelId::parse("terminal").unwrap(),
            },
        )
        .unwrap();
        acquire_lease(
            &paths,
            AcquireLeaseRequest {
                session_id: SessionId::parse("sess_manual").unwrap(),
                channel_id: ChannelId::parse("terminal").unwrap(),
                steal: false,
            },
        )
        .unwrap();

        let err = release_lease(
            &paths,
            ReleaseLeaseRequest {
                session_id: SessionId::parse("sess_manual").unwrap(),
                lease_token: LeaseToken::parse("lease_0000000000000000000000000000000000000000")
                    .unwrap(),
            },
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("does not match"));
        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn acquire_requires_steal_when_lease_is_active() {
        let paths = temp_paths("steal");
        create_session(
            &paths,
            CreateSessionRequest {
                session_id: Some(SessionId::parse("sess_manual").unwrap()),
                channel_id: ChannelId::parse("terminal").unwrap(),
            },
        )
        .unwrap();
        acquire_lease(
            &paths,
            AcquireLeaseRequest {
                session_id: SessionId::parse("sess_manual").unwrap(),
                channel_id: ChannelId::parse("terminal").unwrap(),
                steal: false,
            },
        )
        .unwrap();

        let err = acquire_lease(
            &paths,
            AcquireLeaseRequest {
                session_id: SessionId::parse("sess_manual").unwrap(),
                channel_id: ChannelId::parse("telegram:12345").unwrap(),
                steal: false,
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("active lease"));

        let stolen = acquire_lease(
            &paths,
            AcquireLeaseRequest {
                session_id: SessionId::parse("sess_manual").unwrap(),
                channel_id: ChannelId::parse("telegram:12345").unwrap(),
                steal: true,
            },
        )
        .unwrap();
        assert_eq!(stolen.session.lease_epoch, 2);
        assert_eq!(
            stolen.session.current_channel_id,
            ChannelId::parse("telegram:12345").unwrap()
        );

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn record_channel_message_appends_metadata_only_event() {
        let paths = temp_paths("message");
        create_session(
            &paths,
            CreateSessionRequest {
                session_id: Some(SessionId::parse("sess_manual").unwrap()),
                channel_id: ChannelId::parse("telegram:42").unwrap(),
            },
        )
        .unwrap();

        let event = record_channel_message(
            &paths,
            RecordChannelMessageRequest {
                session_id: SessionId::parse("sess_manual").unwrap(),
                channel_id: ChannelId::parse("telegram:42").unwrap(),
            },
        )
        .unwrap();

        assert_eq!(event.event_kind, JournalEventKind::ChannelMessageReceived);
        let journal = fs::read_to_string(paths.serve_event_journal_file()).unwrap();
        assert!(journal.contains("\"eventKind\":\"channel-message-received\""));
        assert!(!journal.contains("message text"));

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn list_events_filters_by_session() {
        let paths = temp_paths("event-filter");
        create_session(
            &paths,
            CreateSessionRequest {
                session_id: Some(SessionId::parse("sess_one").unwrap()),
                channel_id: ChannelId::parse("terminal").unwrap(),
            },
        )
        .unwrap();
        create_session(
            &paths,
            CreateSessionRequest {
                session_id: Some(SessionId::parse("sess_two").unwrap()),
                channel_id: ChannelId::parse("telegram:12345").unwrap(),
            },
        )
        .unwrap();

        let events = list_events(&paths, Some(&SessionId::parse("sess_two").unwrap())).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, SessionId::parse("sess_two").unwrap());
        let _ = fs::remove_dir_all(&paths.manager_dir);
    }
}
