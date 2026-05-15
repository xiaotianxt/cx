use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::BufRead;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration as StdDuration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::Duration;
use time::OffsetDateTime;

use crate::paths::ManagerPaths;

pub(crate) const WATCH_SESSION_ARG: &str = "__cx-watch-session";

const STATE_VERSION: u32 = 1;
const WATCH_REQUEST_VERSION: u32 = 1;
const WATCH_POLL_INTERVAL: StdDuration = StdDuration::from_millis(750);
const WATCH_MAX_RUNTIME: StdDuration = StdDuration::from_secs(8 * 60 * 60);
const LAUNCH_MATCH_TOLERANCE_SECS: i64 = 2;
const STATE_DB_FILENAME: &str = "state_5.sqlite";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TerminalKey {
    source: String,
    value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ResumeState {
    version: u32,
    terminal_key: TerminalKey,
    pub(crate) slot: String,
    pub(crate) cwd: PathBuf,
    pub(crate) session_id: String,
    pub(crate) rollout_path: Option<PathBuf>,
    recorded_at_unix_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumeCandidate {
    pub(crate) slot: String,
    pub(crate) cwd: PathBuf,
    pub(crate) session_id: String,
    pub(crate) rollout_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WatchRequest {
    version: u32,
    terminal_key: TerminalKey,
    manager_dir: PathBuf,
    codex_home: PathBuf,
    #[serde(default)]
    sqlite_home: Option<PathBuf>,
    slot: String,
    cwd: PathBuf,
    launch_started_unix_ms: u128,
    launch_pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CandidateSession {
    session_id: String,
    rollout_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DbResumeCandidate {
    candidate: ResumeCandidate,
    updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CandidateScan {
    None,
    One(CandidateSession),
    Ambiguous,
}

pub(crate) fn run_internal_watcher(args: Vec<OsString>) -> Result<()> {
    let request_path = args
        .first()
        .map(PathBuf::from)
        .context("missing terminal resume watch request path")?;
    let request = read_watch_request(&request_path)?;
    let result = watch_for_session(&request);
    let _ = fs::remove_file(request_path);
    result
}

pub(crate) fn current_terminal_key() -> Option<TerminalKey> {
    env_terminal_key()
        .or_else(tty_terminal_key)
        .or_else(gpg_tty_terminal_key)
}

pub(crate) fn load_resume_state(
    paths: &ManagerPaths,
    key: &TerminalKey,
    cwd: &Path,
) -> Result<Option<ResumeState>> {
    let path = state_path(paths, key);
    let Some(state) = read_resume_state(&path)? else {
        return Ok(None);
    };
    if state.terminal_key != *key || !resume_state_matches_cwd(&state, cwd) {
        return Ok(None);
    }
    Ok(Some(state))
}

pub(crate) fn load_resume_state_for_cwd(
    paths: &ManagerPaths,
    cwd: &Path,
) -> Result<Option<ResumeState>> {
    let path = cwd_state_path(paths, cwd);
    let Some(state) = read_resume_state(&path)? else {
        return Ok(None);
    };
    if !resume_state_matches_cwd(&state, cwd) {
        return Ok(None);
    }
    Ok(Some(state))
}

pub(crate) fn has_active_session_in_cwd(paths: &ManagerPaths, cwd: &Path) -> bool {
    let Ok(entries) = fs::read_dir(watch_request_dir(paths)) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            return false;
        }
        read_watch_request(&path)
            .is_ok_and(|request| request.cwd == cwd && watch_request_is_active(&request))
    })
}

pub(crate) fn latest_cwd_resume_candidate(
    paths: &ManagerPaths,
    cwd: &Path,
    hint: Option<&ResumeState>,
) -> Result<Option<ResumeCandidate>> {
    let mut latest = latest_cwd_db_candidate(paths, cwd)?;
    if let Some(state) = load_resume_state_for_cwd(paths, cwd)? {
        if let Some(candidate) = resume_candidate_from_state_with_time(&state, cwd) {
            keep_newest_candidate(&mut latest, candidate);
        }
    }
    if let Some(state) = hint {
        if let Some(candidate) = resume_candidate_from_state_with_time(state, cwd) {
            keep_newest_candidate(&mut latest, candidate);
        }
    }
    Ok(latest.map(|candidate| candidate.candidate))
}

pub(crate) fn record_resume_state(
    paths: &ManagerPaths,
    key: &TerminalKey,
    slot: &str,
    cwd: &Path,
    session_id: &str,
    rollout_path: Option<PathBuf>,
) -> Result<()> {
    let state = ResumeState {
        version: STATE_VERSION,
        terminal_key: key.clone(),
        slot: slot.to_string(),
        cwd: cwd.to_path_buf(),
        session_id: session_id.to_string(),
        rollout_path,
        recorded_at_unix_ms: now_unix_ms(),
    };
    write_resume_state(paths, &state)
}

pub(crate) fn spawn_session_watcher(
    paths: &ManagerPaths,
    codex_home: &Path,
    slot: &str,
    cwd: &Path,
    key: &TerminalKey,
    launch_started_unix_ms: u128,
    launch_pid: u32,
) -> Result<()> {
    let request = WatchRequest {
        version: WATCH_REQUEST_VERSION,
        terminal_key: key.clone(),
        manager_dir: paths.manager_dir.clone(),
        codex_home: codex_home.to_path_buf(),
        sqlite_home: Some(paths.slot_sqlite_home(slot)),
        slot: slot.to_string(),
        cwd: cwd.to_path_buf(),
        launch_started_unix_ms,
        launch_pid,
    };
    let request_path = write_watch_request(paths, &request)?;
    let mut command = Command::new(std::env::current_exe().context("resolve cx executable")?);
    command
        .arg(WATCH_SESSION_ARG)
        .arg(&request_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_command(&mut command);
    command
        .spawn()
        .with_context(|| format!("spawn terminal resume watcher {}", request_path.display()))?;
    Ok(())
}

pub(crate) fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
pub(crate) fn test_resume_state(slot: &str, cwd: PathBuf) -> ResumeState {
    ResumeState {
        version: STATE_VERSION,
        terminal_key: TerminalKey {
            source: "test".to_string(),
            value: "tty-1".to_string(),
        },
        slot: slot.to_string(),
        cwd,
        session_id: "session-1".to_string(),
        rollout_path: None,
        recorded_at_unix_ms: 1,
    }
}

#[cfg(test)]
pub(crate) fn test_write_watch_request(
    paths: &ManagerPaths,
    cwd: &Path,
    launch_pid: u32,
) -> Result<PathBuf> {
    let request = WatchRequest {
        version: WATCH_REQUEST_VERSION,
        terminal_key: TerminalKey {
            source: "test".to_string(),
            value: "tty-1".to_string(),
        },
        manager_dir: paths.manager_dir.clone(),
        codex_home: paths.base_codex_home.clone(),
        sqlite_home: None,
        slot: "dia1".to_string(),
        cwd: cwd.to_path_buf(),
        launch_started_unix_ms: now_unix_ms(),
        launch_pid,
    };
    write_watch_request(paths, &request)
}

fn watch_for_session(request: &WatchRequest) -> Result<()> {
    if request.version != WATCH_REQUEST_VERSION {
        return Ok(());
    }

    let paths = ManagerPaths::new(Some(request.manager_dir.clone())).context("load paths")?;
    let started = SystemTime::now();
    let mut last_recorded: Option<CandidateSession> = None;
    loop {
        match scan_for_current_session(request) {
            CandidateScan::One(candidate) => {
                if last_recorded.as_ref() != Some(&candidate) {
                    record_resume_state(
                        &paths,
                        &request.terminal_key,
                        &request.slot,
                        &request.cwd,
                        &candidate.session_id,
                        Some(candidate.rollout_path.clone()),
                    )?;
                    last_recorded = Some(candidate);
                }
            }
            CandidateScan::Ambiguous => {}
            CandidateScan::None => {}
        }

        if !process_alive(request.launch_pid) {
            return Ok(());
        }
        if started.elapsed().unwrap_or_default() >= WATCH_MAX_RUNTIME {
            return Ok(());
        }
        thread::sleep(WATCH_POLL_INTERVAL);
    }
}

fn latest_cwd_db_candidate(paths: &ManagerPaths, cwd: &Path) -> Result<Option<DbResumeCandidate>> {
    let mut latest = None;
    for slot in state_db_slot_names(paths)? {
        let Some(candidate) = latest_cwd_db_candidate_for_slot(paths, &slot, cwd)? else {
            continue;
        };
        if latest
            .as_ref()
            .is_none_or(|current| db_candidate_is_newer(&candidate, current))
        {
            latest = Some(candidate);
        }
    }
    Ok(latest)
}

fn state_db_slot_names(paths: &ManagerPaths) -> Result<Vec<String>> {
    let entries = match fs::read_dir(&paths.slots_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("read {}", paths.slots_dir.display())),
    };
    let mut slots = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().join("home").is_dir() {
            continue;
        }
        slots.push(entry.file_name().to_string_lossy().to_string());
    }
    slots.sort();
    Ok(slots)
}

fn latest_cwd_db_candidate_for_slot(
    paths: &ManagerPaths,
    slot: &str,
    cwd: &Path,
) -> Result<Option<DbResumeCandidate>> {
    let path = paths.slot_sqlite_home(slot).join(STATE_DB_FILENAME);
    if !path.exists() {
        return Ok(None);
    }
    let Ok(conn) = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Ok(None);
    };
    let candidate = query_latest_cwd_db_candidate(&conn, slot, cwd, UpdatedAtColumn::Millis)?;
    if candidate.is_some() {
        return Ok(candidate);
    }
    query_latest_cwd_db_candidate(&conn, slot, cwd, UpdatedAtColumn::Seconds)
}

#[derive(Debug, Clone, Copy)]
enum UpdatedAtColumn {
    Millis,
    Seconds,
}

fn query_latest_cwd_db_candidate(
    conn: &Connection,
    slot: &str,
    cwd: &Path,
    updated_at_column: UpdatedAtColumn,
) -> Result<Option<DbResumeCandidate>> {
    let sql = match updated_at_column {
        UpdatedAtColumn::Millis => {
            "SELECT id, rollout_path, updated_at_ms
             FROM threads
             WHERE archived = 0
               AND cwd = ?1
               AND rollout_path <> ''
             ORDER BY updated_at_ms DESC, id DESC
             LIMIT 16"
        }
        UpdatedAtColumn::Seconds => {
            "SELECT id, rollout_path, updated_at
             FROM threads
             WHERE archived = 0
               AND cwd = ?1
               AND rollout_path <> ''
             ORDER BY updated_at DESC, id DESC
             LIMIT 16"
        }
    };
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Ok(None);
    };
    let Ok(rows) = stmt.query_map(params![cwd.display().to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            PathBuf::from(row.get::<_, String>(1)?),
            row.get::<_, i64>(2)?,
        ))
    }) else {
        return Ok(None);
    };

    for row in rows {
        let Ok((session_id, rollout_path, raw_updated_at)) = row else {
            continue;
        };
        if !rollout_path.exists() {
            continue;
        }
        let updated_at = match updated_at_column {
            UpdatedAtColumn::Millis => raw_updated_at,
            UpdatedAtColumn::Seconds => raw_updated_at.saturating_mul(1000),
        };
        return Ok(Some(DbResumeCandidate {
            candidate: ResumeCandidate {
                slot: slot.to_string(),
                cwd: cwd.to_path_buf(),
                session_id,
                rollout_path: Some(rollout_path),
            },
            updated_at,
        }));
    }
    Ok(None)
}

fn keep_newest_candidate(latest: &mut Option<DbResumeCandidate>, candidate: DbResumeCandidate) {
    if latest
        .as_ref()
        .is_none_or(|current| db_candidate_is_newer(&candidate, current))
    {
        *latest = Some(candidate);
    }
}

fn db_candidate_is_newer(candidate: &DbResumeCandidate, current: &DbResumeCandidate) -> bool {
    candidate.updated_at > current.updated_at
        || (candidate.updated_at == current.updated_at
            && candidate.candidate.session_id > current.candidate.session_id)
}

fn resume_candidate_from_state_with_time(
    state: &ResumeState,
    cwd: &Path,
) -> Option<DbResumeCandidate> {
    let updated_at = state.recorded_at_unix_ms.min(i64::MAX as u128) as i64;
    resume_candidate_from_state(state, cwd).map(|candidate| DbResumeCandidate {
        candidate,
        updated_at,
    })
}

fn resume_candidate_from_state(state: &ResumeState, cwd: &Path) -> Option<ResumeCandidate> {
    if state.cwd != cwd {
        return None;
    }
    if state
        .rollout_path
        .as_deref()
        .is_some_and(|path| !path.exists())
    {
        return None;
    }
    Some(ResumeCandidate {
        slot: state.slot.clone(),
        cwd: state.cwd.clone(),
        session_id: state.session_id.clone(),
        rollout_path: state.rollout_path.clone(),
    })
}

fn scan_for_current_session(request: &WatchRequest) -> CandidateScan {
    match scan_state_db_candidate(request) {
        CandidateScan::None => scan_new_rollout_candidate(request),
        scan => scan,
    }
}

fn scan_state_db_candidate(request: &WatchRequest) -> CandidateScan {
    let Some(sqlite_home) = request.sqlite_home.as_ref() else {
        return CandidateScan::None;
    };
    let path = sqlite_home.join(STATE_DB_FILENAME);
    if !path.exists() {
        return CandidateScan::None;
    }
    let Ok(conn) = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return CandidateScan::None;
    };
    let min_updated_at_ms = request
        .launch_started_unix_ms
        .saturating_sub((LAUNCH_MATCH_TOLERANCE_SECS as u128) * 1000)
        .min(i64::MAX as u128) as i64;
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, rollout_path, updated_at_ms
         FROM threads
         WHERE archived = 0
           AND cwd = ?1
           AND rollout_path <> ''
           AND updated_at_ms >= ?2
         ORDER BY updated_at_ms DESC, id DESC
         LIMIT 2",
    ) else {
        return CandidateScan::None;
    };
    let Ok(rows) = stmt.query_map(
        params![request.cwd.display().to_string(), min_updated_at_ms],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                PathBuf::from(row.get::<_, String>(1)?),
                row.get::<_, i64>(2)?,
            ))
        },
    ) else {
        return CandidateScan::None;
    };

    let mut candidates = Vec::new();
    for row in rows {
        let Ok((session_id, rollout_path, updated_at_ms)) = row else {
            continue;
        };
        if !rollout_path.exists() {
            continue;
        }
        candidates.push((session_id, rollout_path, updated_at_ms));
    }
    let Some((session_id, rollout_path, updated_at_ms)) = candidates.first() else {
        return CandidateScan::None;
    };
    if candidates
        .get(1)
        .is_some_and(|(_, _, other_updated_at_ms)| other_updated_at_ms == updated_at_ms)
    {
        return CandidateScan::Ambiguous;
    }
    CandidateScan::One(CandidateSession {
        session_id: session_id.clone(),
        rollout_path: rollout_path.clone(),
    })
}

fn scan_new_rollout_candidate(request: &WatchRequest) -> CandidateScan {
    let mut found = Vec::new();
    for dir in session_dirs_for_launch(&request.codex_home, request.launch_started_unix_ms) {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_rollout_path(&path) {
                continue;
            }
            if let Some(candidate) = rollout_candidate(&path, request) {
                found.push(candidate);
                if found.len() > 1 {
                    return CandidateScan::Ambiguous;
                }
            }
        }
    }
    match found.pop() {
        Some(candidate) => CandidateScan::One(candidate),
        None => CandidateScan::None,
    }
}

fn rollout_candidate(path: &Path, request: &WatchRequest) -> Option<CandidateSession> {
    let meta = read_rollout_meta(path)?;
    if meta.cwd != request.cwd {
        return None;
    }
    if meta.source.as_deref() != Some("cli") {
        return None;
    }
    let timestamp = OffsetDateTime::parse(meta.timestamp.as_str(), &Rfc3339).ok()?;
    let launch_secs = (request.launch_started_unix_ms / 1000) as i64;
    if timestamp.unix_timestamp() + LAUNCH_MATCH_TOLERANCE_SECS < launch_secs {
        return None;
    }
    Some(CandidateSession {
        session_id: meta.id,
        rollout_path: path.to_path_buf(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RolloutMeta {
    id: String,
    timestamp: String,
    cwd: PathBuf,
    source: Option<String>,
}

fn read_rollout_meta(path: &Path) -> Option<RolloutMeta> {
    let file = fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let value = serde_json::from_str::<Value>(&line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?;
    Some(RolloutMeta {
        id: payload.get("id")?.as_str()?.to_string(),
        timestamp: payload.get("timestamp")?.as_str()?.to_string(),
        cwd: PathBuf::from(payload.get("cwd")?.as_str()?),
        source: payload
            .get("source")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

fn session_dirs_for_launch(codex_home: &Path, launch_unix_ms: u128) -> Vec<PathBuf> {
    let launch_secs = (launch_unix_ms / 1000).min(i64::MAX as u128) as i64;
    let launch_time = OffsetDateTime::from_unix_timestamp(launch_secs)
        .unwrap_or_else(|_| OffsetDateTime::now_utc());
    let mut dirs = BTreeSet::new();
    for offset in [-1, 0, 1] {
        let date = (launch_time + Duration::days(offset)).date();
        dirs.insert(
            codex_home
                .join("sessions")
                .join(date.year().to_string())
                .join(format!("{:02}", u8::from(date.month())))
                .join(format!("{:02}", date.day())),
        );
    }
    dirs.into_iter().collect()
}

fn is_rollout_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
}

fn env_terminal_key() -> Option<TerminalKey> {
    const ENV_KEYS: &[&str] = &[
        "CX_TERMINAL_SESSION_ID",
        "TMUX_PANE",
        "WEZTERM_PANE",
        "KITTY_WINDOW_ID",
        "ITERM_SESSION_ID",
        "TERM_SESSION_ID",
        "WT_SESSION",
        "ZELLIJ",
        "STY",
    ];
    ENV_KEYS.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| TerminalKey {
                source: (*name).to_string(),
                value,
            })
    })
}

#[cfg(unix)]
fn tty_terminal_key() -> Option<TerminalKey> {
    let mut buffer = [0_i8; 256];
    // SAFETY: `buffer` is a valid writable C buffer and stdin is a process-owned fd.
    let result = unsafe { libc::ttyname_r(libc::STDIN_FILENO, buffer.as_mut_ptr(), buffer.len()) };
    if result != 0 {
        return None;
    }
    // SAFETY: a successful ttyname_r call writes a NUL-terminated string into `buffer`.
    let tty = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    if tty.is_empty() {
        return None;
    }
    // SAFETY: getsid(0) asks the OS for this process' session id.
    let session_id = unsafe { libc::getsid(0) };
    Some(TerminalKey {
        source: "tty".to_string(),
        value: format!("{tty}:{session_id}"),
    })
}

#[cfg(not(unix))]
fn tty_terminal_key() -> Option<TerminalKey> {
    None
}

fn gpg_tty_terminal_key() -> Option<TerminalKey> {
    std::env::var("GPG_TTY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| TerminalKey {
            source: "GPG_TTY".to_string(),
            value,
        })
}

fn state_path(paths: &ManagerPaths, key: &TerminalKey) -> PathBuf {
    paths
        .manager_dir
        .join("terminal-resume")
        .join(format!("{}.json", key.fingerprint()))
}

fn cwd_state_path(paths: &ManagerPaths, cwd: &Path) -> PathBuf {
    paths
        .manager_dir
        .join("terminal-resume")
        .join("by-cwd")
        .join(format!(
            "{}.json",
            fingerprint_text(&cwd.display().to_string())
        ))
}

fn read_resume_state(path: &Path) -> Result<Option<ResumeState>> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let Ok(state) = serde_json::from_str::<ResumeState>(&text) else {
        return Ok(None);
    };
    Ok(Some(state))
}

fn resume_state_matches_cwd(state: &ResumeState, cwd: &Path) -> bool {
    state.version == STATE_VERSION
        && state.cwd == cwd
        && state
            .rollout_path
            .as_deref()
            .is_none_or(|path| path.exists())
}

fn watch_request_dir(paths: &ManagerPaths) -> PathBuf {
    paths.manager_dir.join(".tmp").join("session-watch")
}

fn watch_request_is_active(request: &WatchRequest) -> bool {
    if request.version != WATCH_REQUEST_VERSION {
        return false;
    }
    if now_unix_ms().saturating_sub(request.launch_started_unix_ms) > WATCH_MAX_RUNTIME.as_millis()
    {
        return false;
    }
    // Occupation is represented by cx watcher requests. Direct `codex` launches
    // do not write these requests, so they are not visible to this guard.
    process_alive(request.launch_pid)
}

fn write_resume_state(paths: &ManagerPaths, state: &ResumeState) -> Result<()> {
    write_resume_state_path(&state_path(paths, &state.terminal_key), state)?;
    write_resume_state_path(&cwd_state_path(paths, &state.cwd), state)
}

fn write_resume_state_path(path: &Path, state: &ResumeState) -> Result<()> {
    let parent = path
        .parent()
        .context("terminal resume state path has no parent")?;
    fs::create_dir_all(parent)?;
    atomic_write_json(path, state)
}

fn write_watch_request(paths: &ManagerPaths, request: &WatchRequest) -> Result<PathBuf> {
    let dir = watch_request_dir(paths);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "{}-{}.json",
        std::process::id(),
        request.launch_started_unix_ms
    ));
    atomic_write_json(&path, request)?;
    Ok(path)
}

fn read_watch_request(path: &Path) -> Result<WatchRequest> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("read terminal resume watch request {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("parse terminal resume watch request {}", path.display()))
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), now_unix_ms()));
    let data = serde_json::to_vec_pretty(value)?;
    fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))
}

impl TerminalKey {
    fn fingerprint(&self) -> String {
        fingerprint_bytes(self.source.bytes().chain([0]).chain(self.value.bytes()))
    }
}

fn fingerprint_text(value: &str) -> String {
    fingerprint_bytes(value.bytes())
}

fn fingerprint_bytes(bytes: impl IntoIterator<Item = u8>) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(unix)]
fn detach_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the pre-exec closure only calls async-signal-safe setsid and returns.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach_command(_command: &mut Command) {}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: kill(pid, 0) performs permission/existence checking without sending a signal.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn test_paths(name: &str) -> ManagerPaths {
        let root = std::env::temp_dir().join(format!(
            "cx-terminal-resume-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
    }

    fn terminal_key() -> TerminalKey {
        TerminalKey {
            source: "test".to_string(),
            value: "tty-1".to_string(),
        }
    }

    fn write_rollout(codex_home: &Path, timestamp: &str, id: &str, cwd: &Path) -> PathBuf {
        let path = codex_home
            .join("sessions")
            .join(&timestamp[0..4])
            .join(&timestamp[5..7])
            .join(&timestamp[8..10])
            .join(format!("rollout-{}-{id}.jsonl", &timestamp[0..19]));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"timestamp\":\"{timestamp}\",\"cwd\":\"{}\",\"source\":\"cli\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();
        path
    }

    fn write_state_thread(
        paths: &ManagerPaths,
        slot: &str,
        id: &str,
        rollout_path: &Path,
        cwd: &Path,
        updated_at_ms: i64,
    ) {
        write_state_thread_with_source(paths, slot, id, rollout_path, cwd, updated_at_ms, "cli");
    }

    fn write_state_thread_with_source(
        paths: &ManagerPaths,
        slot: &str,
        id: &str,
        rollout_path: &Path,
        cwd: &Path,
        updated_at_ms: i64,
        source: &str,
    ) {
        let db_path = paths.slot_sqlite_home(slot).join(STATE_DB_FILENAME);
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                cwd TEXT NOT NULL,
                source TEXT NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0,
                updated_at_ms INTEGER NOT NULL
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO threads
             (id, rollout_path, cwd, source, archived, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            params![
                id,
                rollout_path.display().to_string(),
                cwd.display().to_string(),
                source,
                updated_at_ms,
            ],
        )
        .unwrap();
    }

    fn write_state_thread_seconds(
        paths: &ManagerPaths,
        slot: &str,
        id: &str,
        rollout_path: &Path,
        cwd: &Path,
        updated_at: i64,
    ) {
        let db_path = paths.slot_sqlite_home(slot).join(STATE_DB_FILENAME);
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                cwd TEXT NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO threads
             (id, rollout_path, cwd, archived, updated_at)
             VALUES (?1, ?2, ?3, 0, ?4)",
            params![
                id,
                rollout_path.display().to_string(),
                cwd.display().to_string(),
                updated_at,
            ],
        )
        .unwrap();
    }

    #[test]
    fn terminal_key_fingerprint_is_stable() {
        assert_eq!(terminal_key().fingerprint(), terminal_key().fingerprint());
    }

    #[test]
    fn resume_state_round_trips() {
        let paths = test_paths("state-round-trip");
        let key = terminal_key();
        let cwd = paths.base_codex_home.join("project");
        record_resume_state(
            &paths,
            &key,
            "dia1",
            &cwd,
            "session-1",
            Some(paths.base_codex_home.join("rollout.jsonl")),
        )
        .unwrap();
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        fs::write(paths.base_codex_home.join("rollout.jsonl"), "").unwrap();

        let state = load_resume_state(&paths, &key, &cwd).unwrap().unwrap();
        let cwd_state = load_resume_state_for_cwd(&paths, &cwd).unwrap().unwrap();

        assert_eq!(state.slot, "dia1");
        assert_eq!(state.session_id, "session-1");
        assert_eq!(cwd_state.slot, "dia1");
        assert_eq!(cwd_state.session_id, "session-1");
    }

    #[test]
    fn latest_cwd_resume_candidate_prefers_newest_thread_across_slots() {
        let paths = test_paths("latest-cwd");
        let cwd = paths.base_codex_home.join("project");
        let other_cwd = paths.base_codex_home.join("other-project");
        let older = write_rollout(
            &paths.slot_home("dia1"),
            "2026-05-13T21:40:06.000Z",
            "session-old",
            &cwd,
        );
        let newer = write_rollout(
            &paths.slot_home("bus1"),
            "2026-05-13T21:41:06.000Z",
            "session-new",
            &cwd,
        );
        let other = write_rollout(
            &paths.slot_home("bus2"),
            "2026-05-13T21:42:06.000Z",
            "session-other-cwd",
            &other_cwd,
        );
        write_state_thread(
            &paths,
            "dia1",
            "session-old",
            &older,
            &cwd,
            1_778_704_801_000,
        );
        write_state_thread(
            &paths,
            "bus1",
            "session-new",
            &newer,
            &cwd,
            1_778_704_802_000,
        );
        write_state_thread(
            &paths,
            "bus2",
            "session-other-cwd",
            &other,
            &other_cwd,
            1_778_704_803_000,
        );

        let candidate = latest_cwd_resume_candidate(&paths, &cwd, None)
            .unwrap()
            .unwrap();

        assert_eq!(candidate.slot, "bus1");
        assert_eq!(candidate.session_id, "session-new");
        assert_eq!(candidate.rollout_path, Some(newer));
    }

    #[test]
    fn latest_cwd_resume_candidate_accepts_updated_at_seconds() {
        let paths = test_paths("latest-cwd-seconds");
        let cwd = paths.base_codex_home.join("project");
        let rollout = write_rollout(
            &paths.slot_home("dia1"),
            "2026-05-13T21:40:06.000Z",
            "session-seconds",
            &cwd,
        );
        write_state_thread_seconds(&paths, "dia1", "session-seconds", &rollout, &cwd, 1_778_704);

        let candidate = latest_cwd_resume_candidate(&paths, &cwd, None)
            .unwrap()
            .unwrap();

        assert_eq!(candidate.slot, "dia1");
        assert_eq!(candidate.session_id, "session-seconds");
        assert_eq!(candidate.rollout_path, Some(rollout));
    }

    #[test]
    fn latest_cwd_resume_candidate_uses_terminal_state_as_fallback() {
        let paths = test_paths("latest-cwd-fallback");
        let cwd = paths.base_codex_home.join("project");
        let state = test_resume_state("dia1", cwd.clone());

        let candidate = latest_cwd_resume_candidate(&paths, &cwd, Some(&state))
            .unwrap()
            .unwrap();

        assert_eq!(candidate.slot, "dia1");
        assert_eq!(candidate.session_id, "session-1");
        assert_eq!(candidate.rollout_path, None);
    }

    #[test]
    fn active_session_detects_live_watch_request_in_cwd() {
        let paths = test_paths("active-watch");
        let cwd = paths.base_codex_home.join("project");
        let path = test_write_watch_request(&paths, &cwd, std::process::id()).unwrap();

        assert!(has_active_session_in_cwd(&paths, &cwd));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn active_session_ignores_live_watch_request_in_other_cwd() {
        let paths = test_paths("active-watch-other-cwd");
        let cwd = paths.base_codex_home.join("project");
        let other_cwd = paths.base_codex_home.join("other-project");
        let path = test_write_watch_request(&paths, &other_cwd, std::process::id()).unwrap();

        assert!(!has_active_session_in_cwd(&paths, &cwd));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn active_session_ignores_dead_watch_request() {
        let paths = test_paths("dead-watch");
        let cwd = paths.base_codex_home.join("project");
        let _path = test_write_watch_request(&paths, &cwd, 0).unwrap();

        assert!(!has_active_session_in_cwd(&paths, &cwd));
    }

    #[test]
    fn scan_matches_single_rollout_by_cwd_and_timestamp() {
        let paths = test_paths("scan-single");
        let cwd = paths.base_codex_home.join("project");
        let timestamp = "2026-05-13T21:40:06.000Z";
        let rollout = write_rollout(&paths.base_codex_home, timestamp, "session-1", &cwd);
        let request = WatchRequest {
            version: WATCH_REQUEST_VERSION,
            terminal_key: terminal_key(),
            manager_dir: paths.manager_dir.clone(),
            codex_home: paths.base_codex_home.clone(),
            sqlite_home: None,
            slot: "dia1".to_string(),
            cwd,
            launch_started_unix_ms: 1_778_704_800_000,
            launch_pid: std::process::id(),
        };

        assert_eq!(
            scan_for_current_session(&request),
            CandidateScan::One(CandidateSession {
                session_id: "session-1".to_string(),
                rollout_path: rollout
            })
        );
    }

    #[test]
    fn scan_rejects_ambiguous_parallel_rollouts() {
        let paths = test_paths("scan-ambiguous");
        let cwd = paths.base_codex_home.join("project");
        let timestamp = "2026-05-13T21:40:06.000Z";
        write_rollout(&paths.base_codex_home, timestamp, "session-1", &cwd);
        write_rollout(&paths.base_codex_home, timestamp, "session-2", &cwd);
        let request = WatchRequest {
            version: WATCH_REQUEST_VERSION,
            terminal_key: terminal_key(),
            manager_dir: paths.manager_dir.clone(),
            codex_home: paths.base_codex_home.clone(),
            sqlite_home: None,
            slot: "dia1".to_string(),
            cwd,
            launch_started_unix_ms: 1_778_704_800_000,
            launch_pid: std::process::id(),
        };

        assert_eq!(scan_for_current_session(&request), CandidateScan::Ambiguous);
    }

    #[test]
    fn scan_state_db_matches_old_rollout_updated_after_launch() {
        let paths = test_paths("scan-state-old-rollout");
        let cwd = paths.base_codex_home.join("project");
        let rollout = write_rollout(
            &paths.base_codex_home,
            "2026-04-01T10:00:00.000Z",
            "session-2",
            &cwd,
        );
        write_state_thread(
            &paths,
            "dia1",
            "session-2",
            &rollout,
            &cwd,
            1_778_704_801_000,
        );
        let request = WatchRequest {
            version: WATCH_REQUEST_VERSION,
            terminal_key: terminal_key(),
            manager_dir: paths.manager_dir.clone(),
            codex_home: paths.base_codex_home.clone(),
            sqlite_home: Some(paths.slot_sqlite_home("dia1")),
            slot: "dia1".to_string(),
            cwd,
            launch_started_unix_ms: 1_778_704_800_000,
            launch_pid: std::process::id(),
        };

        assert_eq!(
            scan_for_current_session(&request),
            CandidateScan::One(CandidateSession {
                session_id: "session-2".to_string(),
                rollout_path: rollout
            })
        );
    }

    #[test]
    fn scan_state_db_accepts_non_cli_source_after_launch() {
        let paths = test_paths("scan-state-non-cli");
        let cwd = paths.base_codex_home.join("project");
        let rollout = write_rollout(
            &paths.base_codex_home,
            "2026-04-01T10:00:00.000Z",
            "session-2",
            &cwd,
        );
        write_state_thread_with_source(
            &paths,
            "dia1",
            "session-2",
            &rollout,
            &cwd,
            1_778_704_801_000,
            "vscode",
        );
        let request = WatchRequest {
            version: WATCH_REQUEST_VERSION,
            terminal_key: terminal_key(),
            manager_dir: paths.manager_dir.clone(),
            codex_home: paths.base_codex_home.clone(),
            sqlite_home: Some(paths.slot_sqlite_home("dia1")),
            slot: "dia1".to_string(),
            cwd,
            launch_started_unix_ms: 1_778_704_800_000,
            launch_pid: std::process::id(),
        };

        assert_eq!(
            scan_for_current_session(&request),
            CandidateScan::One(CandidateSession {
                session_id: "session-2".to_string(),
                rollout_path: rollout
            })
        );
    }

    #[test]
    fn scan_state_db_prefers_latest_updated_thread() {
        let paths = test_paths("scan-state-latest");
        let cwd = paths.base_codex_home.join("project");
        let first = write_rollout(
            &paths.base_codex_home,
            "2026-05-13T21:40:06.000Z",
            "session-1",
            &cwd,
        );
        let second = write_rollout(
            &paths.base_codex_home,
            "2026-04-01T10:00:00.000Z",
            "session-2",
            &cwd,
        );
        write_state_thread(&paths, "dia1", "session-1", &first, &cwd, 1_778_704_801_000);
        write_state_thread(
            &paths,
            "dia1",
            "session-2",
            &second,
            &cwd,
            1_778_704_802_000,
        );
        let request = WatchRequest {
            version: WATCH_REQUEST_VERSION,
            terminal_key: terminal_key(),
            manager_dir: paths.manager_dir.clone(),
            codex_home: paths.base_codex_home.clone(),
            sqlite_home: Some(paths.slot_sqlite_home("dia1")),
            slot: "dia1".to_string(),
            cwd,
            launch_started_unix_ms: 1_778_704_800_000,
            launch_pid: std::process::id(),
        };

        assert_eq!(
            scan_for_current_session(&request),
            CandidateScan::One(CandidateSession {
                session_id: "session-2".to_string(),
                rollout_path: second
            })
        );
    }
}
