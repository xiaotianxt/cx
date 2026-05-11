use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;

use crate::app_server::AppServerClient;
use crate::app_server::AppThreadRead;
use crate::app_server::AppThreadSummary;
use crate::app_server::StartedThread;
use crate::app_server::ThreadListFilter;
use crate::app_server::ThreadListPage;
use crate::paths::ManagerPaths;
use crate::session;
use crate::session::AppThreadBinding;
use crate::session::BindAppThreadRequest;
use crate::session::ChannelId;
use crate::session::CreateSessionRequest;
use crate::session::SessionId;

const INTERACTIVE_SOURCE_KINDS: &[&str] = &["cli", "vscode"];

#[derive(Debug, Clone)]
pub(crate) struct ThreadResolverScope {
    pub(crate) cwd: PathBuf,
    pub(crate) channel_id: Option<ChannelId>,
    pub(crate) explicit_resume_id: Option<ExplicitResumeId>,
    pub(crate) slot: Option<String>,
    pub(crate) generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExplicitResumeId {
    CxSession(SessionId),
    AppThreadOrCodexSession(String),
}

impl ExplicitResumeId {
    pub(crate) fn parse(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        match SessionId::parse(raw.clone()) {
            Ok(session_id) => Self::CxSession(session_id),
            Err(_) => Self::AppThreadOrCodexSession(raw),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::CxSession(session_id) => session_id.as_str(),
            Self::AppThreadOrCodexSession(id) => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThreadResolverDecision {
    AttachExisting { thread_id: String },
    StartNew { cwd: String },
    Refuse { reason: String },
}

#[derive(Debug, Clone)]
pub(crate) struct ThreadResolverOutcome {
    pub(crate) decision: ThreadResolverDecision,
    pub(crate) thread_id: Option<String>,
}

pub(crate) trait ThreadResolverClient {
    fn thread_list_filtered(&mut self, filter: ThreadListFilter<'_>) -> Result<ThreadListPage>;
    fn thread_start(&mut self, cwd: Option<&str>) -> Result<StartedThread>;
    fn thread_read(&mut self, thread_id: &str, include_turns: bool) -> Result<AppThreadRead>;
    fn thread_resume_with_path(
        &mut self,
        thread_id: &str,
        path: Option<&str>,
        cwd: Option<&str>,
        exclude_turns: bool,
    ) -> Result<AppThreadRead>;
}

impl ThreadResolverClient for AppServerClient {
    fn thread_list_filtered(&mut self, filter: ThreadListFilter<'_>) -> Result<ThreadListPage> {
        AppServerClient::thread_list_filtered(self, filter)
    }

    fn thread_start(&mut self, cwd: Option<&str>) -> Result<StartedThread> {
        AppServerClient::thread_start(self, cwd)
    }

    fn thread_read(&mut self, thread_id: &str, include_turns: bool) -> Result<AppThreadRead> {
        AppServerClient::thread_read(self, thread_id, include_turns)
    }

    fn thread_resume_with_path(
        &mut self,
        thread_id: &str,
        path: Option<&str>,
        cwd: Option<&str>,
        exclude_turns: bool,
    ) -> Result<AppThreadRead> {
        AppServerClient::thread_resume_with_path(self, thread_id, path, cwd, exclude_turns)
    }
}

pub(crate) fn resolve_app_thread<C: ThreadResolverClient>(
    paths: &ManagerPaths,
    client: &mut C,
    scope: ThreadResolverScope,
) -> Result<ThreadResolverOutcome> {
    let cwd = workspace_string(&scope.cwd);
    let default_terminal_channel = scope.channel_id.is_none();
    let channel_id = match scope.channel_id {
        Some(ref channel_id) => channel_id.clone(),
        None => ChannelId::parse("terminal")?,
    };

    if let Some(explicit_resume_id) = scope.explicit_resume_id.as_ref() {
        return resolve_explicit_resume(
            paths,
            client,
            &scope,
            &channel_id,
            &cwd,
            explicit_resume_id,
            default_terminal_channel,
        );
    }

    let mut cwd_threads = None;
    if let Some(session) = find_channel_session(paths, &channel_id, &cwd, default_terminal_channel)?
    {
        if !session_allows_auto_attach(client, &mut cwd_threads, &cwd, &session)? {
            return start_new_thread(
                paths,
                client,
                &scope,
                &channel_id,
                &cwd,
                default_terminal_channel,
            );
        }
        if let Some(outcome) = attach_session_thread(paths, client, &scope, &session, &cwd, None)? {
            return Ok(outcome);
        }
    }

    if let Some(session) = session::find_session_by_app_thread_cwd(paths, &cwd)? {
        if !session_allows_auto_attach(client, &mut cwd_threads, &cwd, &session)? {
            return start_new_thread(
                paths,
                client,
                &scope,
                &channel_id,
                &cwd,
                default_terminal_channel,
            );
        }
        if let Some(outcome) = attach_session_thread(paths, client, &scope, &session, &cwd, None)? {
            return Ok(outcome);
        }
    }

    let threads = take_or_fetch_cwd_threads(client, &mut cwd_threads, &cwd)?;
    if let Some(summary) = select_best_thread(threads) {
        if !summary_allows_auto_attach(&summary) {
            return start_new_thread(
                paths,
                client,
                &scope,
                &channel_id,
                &cwd,
                default_terminal_channel,
            );
        }
        let session = ensure_session(paths, &channel_id, &cwd, default_terminal_channel)?;
        let _session = bind_summary(
            paths,
            session.session_id.clone(),
            &summary,
            &scope,
            Some(&cwd),
        )?;
        return Ok(ThreadResolverOutcome {
            decision: ThreadResolverDecision::AttachExisting {
                thread_id: summary.upstream_thread_id.clone(),
            },
            thread_id: Some(summary.upstream_thread_id),
        });
    }

    start_new_thread(
        paths,
        client,
        &scope,
        &channel_id,
        &cwd,
        default_terminal_channel,
    )
}

fn start_new_thread(
    paths: &ManagerPaths,
    client: &mut impl ThreadResolverClient,
    scope: &ThreadResolverScope,
    channel_id: &ChannelId,
    cwd: &str,
    default_terminal_channel: bool,
) -> Result<ThreadResolverOutcome> {
    let started = client.thread_start(Some(cwd))?;
    let read = client
        .thread_read(&started.upstream_thread_id, false)
        .with_context(|| {
            format!(
                "read started app-server thread {}",
                started.upstream_thread_id
            )
        })?;
    let session = ensure_session(paths, channel_id, cwd, default_terminal_channel)?;
    let _session = bind_summary(
        paths,
        session.session_id.clone(),
        &read.summary,
        scope,
        Some(cwd),
    )?;
    Ok(ThreadResolverOutcome {
        decision: ThreadResolverDecision::StartNew {
            cwd: cwd.to_string(),
        },
        thread_id: Some(read.summary.upstream_thread_id),
    })
}

fn session_allows_auto_attach(
    client: &mut impl ThreadResolverClient,
    cwd_threads: &mut Option<Vec<AppThreadSummary>>,
    cwd: &str,
    session: &session::SessionRecord,
) -> Result<bool> {
    let Some(app_thread) = session.app_thread.as_ref() else {
        return Ok(true);
    };
    auto_attach_allowed_for_thread(client, cwd_threads, cwd, &app_thread.thread_id)
}

fn auto_attach_allowed_for_thread(
    client: &mut impl ThreadResolverClient,
    cwd_threads: &mut Option<Vec<AppThreadSummary>>,
    cwd: &str,
    thread_id: &str,
) -> Result<bool> {
    let threads = fetch_cwd_threads(client, cwd_threads, cwd)?;
    Ok(threads
        .iter()
        .find(|summary| summary.upstream_thread_id == thread_id)
        .is_none_or(summary_allows_auto_attach))
}

fn take_or_fetch_cwd_threads(
    client: &mut impl ThreadResolverClient,
    cwd_threads: &mut Option<Vec<AppThreadSummary>>,
    cwd: &str,
) -> Result<Vec<AppThreadSummary>> {
    if cwd_threads.is_none() {
        *cwd_threads = Some(list_cwd_threads(client, cwd)?);
    }
    Ok(cwd_threads.take().unwrap_or_default())
}

fn fetch_cwd_threads<'a>(
    client: &mut impl ThreadResolverClient,
    cwd_threads: &'a mut Option<Vec<AppThreadSummary>>,
    cwd: &str,
) -> Result<&'a [AppThreadSummary]> {
    if cwd_threads.is_none() {
        *cwd_threads = Some(list_cwd_threads(client, cwd)?);
    }
    Ok(cwd_threads.as_deref().unwrap_or(&[]))
}

fn list_cwd_threads(
    client: &mut impl ThreadResolverClient,
    cwd: &str,
) -> Result<Vec<AppThreadSummary>> {
    Ok(client
        .thread_list_filtered(ThreadListFilter {
            limit: 50,
            cwd: Some(cwd),
            archived: Some(false),
            source_kinds: Some(INTERACTIVE_SOURCE_KINDS),
            use_state_db_only: false,
        })?
        .threads)
}

fn summary_allows_auto_attach(summary: &AppThreadSummary) -> bool {
    summary.broker_subscriber_count.unwrap_or(0) == 0
}

fn resolve_explicit_resume(
    paths: &ManagerPaths,
    client: &mut impl ThreadResolverClient,
    scope: &ThreadResolverScope,
    channel_id: &ChannelId,
    cwd: &str,
    explicit_resume_id: &ExplicitResumeId,
    default_terminal_channel: bool,
) -> Result<ThreadResolverOutcome> {
    match explicit_resume_id {
        ExplicitResumeId::CxSession(session_id) => {
            if !paths.serve_session_file(session_id.as_str()).exists() {
                return resolve_explicit_app_or_codex_id(
                    paths,
                    client,
                    scope,
                    channel_id,
                    cwd,
                    session_id.as_str(),
                    default_terminal_channel,
                );
            }
            let session = session::show_session(paths, session_id)?;
            if let Some(outcome) = attach_session_thread(paths, client, scope, &session, cwd, None)?
            {
                return Ok(outcome);
            }
            Ok(ThreadResolverOutcome {
                decision: ThreadResolverDecision::Refuse {
                    reason: format!(
                        "cx session {session_id} has no readable or resumable app-server thread"
                    ),
                },
                thread_id: None,
            })
        }
        ExplicitResumeId::AppThreadOrCodexSession(thread_or_session_id) => {
            resolve_explicit_app_or_codex_id(
                paths,
                client,
                scope,
                channel_id,
                cwd,
                thread_or_session_id,
                default_terminal_channel,
            )
        }
    }
}

fn resolve_explicit_app_or_codex_id(
    paths: &ManagerPaths,
    client: &mut impl ThreadResolverClient,
    scope: &ThreadResolverScope,
    channel_id: &ChannelId,
    cwd: &str,
    thread_or_session_id: &str,
    default_terminal_channel: bool,
) -> Result<ThreadResolverOutcome> {
    match client.thread_read(thread_or_session_id, false) {
        Ok(read) => {
            let session = ensure_session(paths, channel_id, cwd, default_terminal_channel)?;
            let _session = bind_summary(
                paths,
                session.session_id.clone(),
                &read.summary,
                scope,
                Some(cwd),
            )?;
            Ok(ThreadResolverOutcome {
                decision: ThreadResolverDecision::AttachExisting {
                    thread_id: read.summary.upstream_thread_id.clone(),
                },
                thread_id: Some(read.summary.upstream_thread_id),
            })
        }
        Err(read_err) => {
            let (session, requested_thread_id) = if let Some(session) =
                session::find_session_by_app_thread_id(paths, thread_or_session_id)?
            {
                (Some(session), Some(thread_or_session_id))
            } else if let Some(session) =
                session::find_session_by_app_thread_codex_session_id(paths, thread_or_session_id)?
            {
                (Some(session), None)
            } else if let Some(session) =
                find_channel_session(paths, channel_id, cwd, default_terminal_channel)?
            {
                (Some(session), Some(thread_or_session_id))
            } else {
                (
                    session::find_session_by_app_thread_cwd(paths, cwd)?,
                    Some(thread_or_session_id),
                )
            };
            if let Some(session) = session {
                if let Some(outcome) =
                    attach_session_thread(paths, client, scope, &session, cwd, requested_thread_id)?
                {
                    return Ok(outcome);
                }
            }
            Ok(ThreadResolverOutcome {
                decision: ThreadResolverDecision::Refuse {
                    reason: format!("explicit app thread or Codex session {thread_or_session_id} is not readable and no resumable path is bound: {read_err:#}"),
                },
                thread_id: None,
            })
        }
    }
}

fn attach_session_thread(
    paths: &ManagerPaths,
    client: &mut impl ThreadResolverClient,
    scope: &ThreadResolverScope,
    session: &session::SessionRecord,
    cwd: &str,
    requested_thread_id: Option<&str>,
) -> Result<Option<ThreadResolverOutcome>> {
    let Some(app_thread) = session.app_thread.as_ref() else {
        return Ok(None);
    };
    let thread_id = requested_thread_id.unwrap_or(&app_thread.thread_id);
    let read = client.thread_read(thread_id, false).or_else(|read_err| {
        let Some(path) = app_thread.path.as_deref() else {
            return Err(read_err);
        };
        client.thread_resume_with_path(thread_id, Some(path), Some(cwd), false)
    });
    let Ok(read) = read else {
        return Ok(None);
    };
    let _session = bind_summary(
        paths,
        session.session_id.clone(),
        &read.summary,
        scope,
        Some(cwd),
    )?;
    Ok(Some(ThreadResolverOutcome {
        decision: ThreadResolverDecision::AttachExisting {
            thread_id: read.summary.upstream_thread_id.clone(),
        },
        thread_id: Some(read.summary.upstream_thread_id),
    }))
}

fn find_channel_session(
    paths: &ManagerPaths,
    channel_id: &ChannelId,
    cwd: &str,
    default_terminal_channel: bool,
) -> Result<Option<session::SessionRecord>> {
    if default_terminal_channel {
        return session::find_session_by_channel_and_app_thread_cwd(paths, channel_id, cwd);
    }
    session::find_session_by_channel(paths, channel_id)
}

fn ensure_session(
    paths: &ManagerPaths,
    channel_id: &ChannelId,
    cwd: &str,
    default_terminal_channel: bool,
) -> Result<session::SessionRecord> {
    if let Some(session) = find_channel_session(paths, channel_id, cwd, default_terminal_channel)? {
        return Ok(session);
    }
    Ok(session::create_session(
        paths,
        CreateSessionRequest {
            session_id: None,
            channel_id: channel_id.clone(),
        },
    )?
    .session)
}

fn bind_summary(
    paths: &ManagerPaths,
    session_id: SessionId,
    summary: &AppThreadSummary,
    scope: &ThreadResolverScope,
    cwd_override: Option<&str>,
) -> Result<session::SessionRecord> {
    session::bind_app_thread(
        paths,
        BindAppThreadRequest {
            session_id,
            app_thread: AppThreadBinding {
                thread_id: summary.upstream_thread_id.clone(),
                codex_session_id: summary.session_id.clone(),
                cwd: cwd_override.unwrap_or(&summary.cwd).to_string(),
                title: summary.title.clone(),
                slot: slot_from_thread_path(paths, summary.path.as_deref())
                    .or_else(|| real_slot(scope.slot.clone())),
                generation: scope.generation,
                path: summary.path.clone(),
                updated_at_unix: summary.updated_at_unix.max(0) as u64,
            },
        },
    )
}

fn workspace_string(cwd: &std::path::Path) -> String {
    cwd.to_string_lossy().to_string()
}

fn real_slot(slot: Option<String>) -> Option<String> {
    slot.filter(|slot| slot != "broker")
}

fn slot_from_thread_path(paths: &ManagerPaths, path: Option<&str>) -> Option<String> {
    let path = Path::new(path?);
    let relative = path.strip_prefix(&paths.slots_dir).ok()?;
    let mut components = relative.components();
    let slot = components.next()?.as_os_str().to_str()?;
    let home = components.next()?.as_os_str().to_str()?;
    (home == "home")
        .then(|| slot.to_string())
        .and_then(|slot| real_slot(Some(slot)))
}

pub(crate) fn select_best_thread(threads: Vec<AppThreadSummary>) -> Option<AppThreadSummary> {
    threads.into_iter().max_by(|left, right| {
        thread_priority(left)
            .cmp(&thread_priority(right))
            .then_with(|| left.updated_at_unix.cmp(&right.updated_at_unix))
            .then_with(|| left.upstream_thread_id.cmp(&right.upstream_thread_id))
    })
}

fn thread_priority(thread: &AppThreadSummary) -> u8 {
    match thread.status.as_str() {
        "active" => 3,
        "idle" => 2,
        "notLoaded" => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use anyhow::anyhow;

    use super::*;

    fn summary(id: &str, cwd: &str, status: &str, updated_at_unix: i64) -> AppThreadSummary {
        summary_with_path(id, cwd, status, updated_at_unix, None)
    }

    fn summary_with_subscribers(
        id: &str,
        cwd: &str,
        status: &str,
        updated_at_unix: i64,
        broker_subscriber_count: usize,
    ) -> AppThreadSummary {
        let mut summary = summary(id, cwd, status, updated_at_unix);
        summary.broker_subscriber_count = Some(broker_subscriber_count);
        summary
    }

    fn summary_with_path(
        id: &str,
        cwd: &str,
        status: &str,
        updated_at_unix: i64,
        path: Option<&str>,
    ) -> AppThreadSummary {
        AppThreadSummary {
            upstream_thread_id: id.to_string(),
            session_id: None,
            title: None,
            preview: String::new(),
            cwd: cwd.to_string(),
            path: path.map(str::to_string),
            source: "cli".to_string(),
            status: status.to_string(),
            active_turn_id: None,
            active: status == "active",
            created_at_unix: 0,
            updated_at_unix,
            broker_subscriber_count: None,
        }
    }

    #[derive(Default)]
    struct FakeResolverClient {
        readable: BTreeMap<String, AppThreadSummary>,
        listed: Vec<AppThreadSummary>,
        started: Option<StartedThread>,
        resume_result: Option<AppThreadSummary>,
        read_calls: Vec<String>,
        resume_calls: Vec<(String, Option<String>, Option<String>, bool)>,
    }

    impl ThreadResolverClient for FakeResolverClient {
        fn thread_list_filtered(
            &mut self,
            _filter: ThreadListFilter<'_>,
        ) -> Result<ThreadListPage> {
            Ok(ThreadListPage {
                threads: self.listed.clone(),
                next_cursor: None,
                backwards_cursor: None,
            })
        }

        fn thread_start(&mut self, cwd: Option<&str>) -> Result<StartedThread> {
            Ok(self.started.clone().unwrap_or_else(|| StartedThread {
                upstream_thread_id: "started".to_string(),
                path: Some("/tmp/started.jsonl".to_string()),
                cwd: cwd.unwrap_or("").to_string(),
            }))
        }

        fn thread_read(&mut self, thread_id: &str, _include_turns: bool) -> Result<AppThreadRead> {
            self.read_calls.push(thread_id.to_string());
            self.readable
                .get(thread_id)
                .cloned()
                .map(|summary| AppThreadRead {
                    summary,
                    turns: Vec::new(),
                })
                .ok_or_else(|| anyhow!("thread {thread_id} is not loaded"))
        }

        fn thread_resume_with_path(
            &mut self,
            thread_id: &str,
            path: Option<&str>,
            cwd: Option<&str>,
            exclude_turns: bool,
        ) -> Result<AppThreadRead> {
            self.resume_calls.push((
                thread_id.to_string(),
                path.map(str::to_string),
                cwd.map(str::to_string),
                exclude_turns,
            ));
            self.resume_result
                .clone()
                .map(|summary| AppThreadRead {
                    summary,
                    turns: Vec::new(),
                })
                .ok_or_else(|| anyhow!("thread {thread_id} cannot resume"))
        }
    }

    fn temp_paths(name: &str) -> ManagerPaths {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cx-thread-resolver-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
    }

    #[test]
    fn resolver_prefers_active_thread_over_newer_idle() {
        let selected = select_best_thread(vec![
            summary("idle-new", "/tmp/project", "idle", 30),
            summary("active-old", "/tmp/project", "active", 10),
        ])
        .unwrap();

        assert_eq!(selected.upstream_thread_id, "active-old");
    }

    #[test]
    fn resolver_prefers_recent_idle_over_not_loaded() {
        let selected = select_best_thread(vec![
            summary("not-loaded-new", "/tmp/project", "notLoaded", 30),
            summary("idle-old", "/tmp/project", "idle", 10),
        ])
        .unwrap();

        assert_eq!(selected.upstream_thread_id, "idle-old");
    }

    #[test]
    fn resolver_uses_updated_time_within_same_status() {
        let selected = select_best_thread(vec![
            summary("idle-old", "/tmp/project", "idle", 10),
            summary("idle-new", "/tmp/project", "idle", 30),
        ])
        .unwrap();

        assert_eq!(selected.upstream_thread_id, "idle-new");
    }

    #[test]
    fn auto_resume_skips_bound_thread_with_subscribers() {
        let paths = temp_paths("skip-subscribed-bound");
        let channel_id = ChannelId::parse("terminal").unwrap();
        let session = session::create_session(
            &paths,
            CreateSessionRequest {
                session_id: None,
                channel_id: channel_id.clone(),
            },
        )
        .unwrap()
        .session;
        session::bind_app_thread(
            &paths,
            BindAppThreadRequest {
                session_id: session.session_id,
                app_thread: AppThreadBinding {
                    thread_id: "thread-1".to_string(),
                    codex_session_id: None,
                    cwd: "/tmp/project".to_string(),
                    title: None,
                    slot: Some("slot-a".to_string()),
                    generation: 1,
                    path: Some("/tmp/thread-1.jsonl".to_string()),
                    updated_at_unix: 10,
                },
            },
        )
        .unwrap();
        let mut readable = BTreeMap::new();
        readable.insert(
            "started".to_string(),
            summary("started", "/tmp/project", "idle", 20),
        );
        let mut client = FakeResolverClient {
            readable,
            listed: vec![summary_with_subscribers(
                "thread-1",
                "/tmp/project",
                "active",
                10,
                1,
            )],
            ..FakeResolverClient::default()
        };

        let outcome = resolve_app_thread(
            &paths,
            &mut client,
            ThreadResolverScope {
                cwd: PathBuf::from("/tmp/project"),
                channel_id: Some(channel_id),
                explicit_resume_id: None,
                slot: Some("slot-b".to_string()),
                generation: 2,
            },
        )
        .unwrap();

        assert_eq!(
            outcome.decision,
            ThreadResolverDecision::StartNew {
                cwd: "/tmp/project".to_string()
            }
        );
        assert_eq!(client.read_calls, vec!["started"]);
    }

    #[test]
    fn auto_resume_allows_bound_thread_without_subscribers() {
        let paths = temp_paths("allow-unsubscribed-bound");
        let channel_id = ChannelId::parse("terminal").unwrap();
        let session = session::create_session(
            &paths,
            CreateSessionRequest {
                session_id: None,
                channel_id: channel_id.clone(),
            },
        )
        .unwrap()
        .session;
        session::bind_app_thread(
            &paths,
            BindAppThreadRequest {
                session_id: session.session_id,
                app_thread: AppThreadBinding {
                    thread_id: "thread-1".to_string(),
                    codex_session_id: None,
                    cwd: "/tmp/project".to_string(),
                    title: None,
                    slot: Some("slot-a".to_string()),
                    generation: 1,
                    path: Some("/tmp/thread-1.jsonl".to_string()),
                    updated_at_unix: 10,
                },
            },
        )
        .unwrap();
        let mut readable = BTreeMap::new();
        readable.insert(
            "thread-1".to_string(),
            summary("thread-1", "/tmp/project", "idle", 10),
        );
        let mut client = FakeResolverClient {
            readable,
            listed: vec![summary_with_subscribers(
                "thread-1",
                "/tmp/project",
                "idle",
                10,
                0,
            )],
            ..FakeResolverClient::default()
        };

        let outcome = resolve_app_thread(
            &paths,
            &mut client,
            ThreadResolverScope {
                cwd: PathBuf::from("/tmp/project"),
                channel_id: Some(channel_id),
                explicit_resume_id: None,
                slot: Some("slot-b".to_string()),
                generation: 2,
            },
        )
        .unwrap();

        assert_eq!(
            outcome.decision,
            ThreadResolverDecision::AttachExisting {
                thread_id: "thread-1".to_string()
            }
        );
        assert_eq!(client.read_calls, vec!["thread-1"]);
    }

    #[test]
    fn auto_resume_skips_best_thread_with_subscribers() {
        let paths = temp_paths("skip-subscribed-list");
        let mut readable = BTreeMap::new();
        readable.insert(
            "started".to_string(),
            summary("started", "/tmp/project", "idle", 20),
        );
        let mut client = FakeResolverClient {
            readable,
            listed: vec![summary_with_subscribers(
                "thread-1",
                "/tmp/project",
                "active",
                10,
                1,
            )],
            ..FakeResolverClient::default()
        };

        let outcome = resolve_app_thread(
            &paths,
            &mut client,
            ThreadResolverScope {
                cwd: PathBuf::from("/tmp/project"),
                channel_id: None,
                explicit_resume_id: None,
                slot: Some("slot-b".to_string()),
                generation: 2,
            },
        )
        .unwrap();

        assert_eq!(
            outcome.decision,
            ThreadResolverDecision::StartNew {
                cwd: "/tmp/project".to_string()
            }
        );
        assert_eq!(client.read_calls, vec!["started"]);
    }

    #[test]
    fn broker_registration_is_not_persisted_as_slot() {
        assert_eq!(real_slot(Some("broker".to_string())), None);
        assert_eq!(
            real_slot(Some("dia4".to_string())),
            Some("dia4".to_string())
        );
    }

    #[test]
    fn slot_from_thread_path_prefers_rollout_path_slot() {
        let paths = temp_paths("slot-from-thread-path");
        let rollout_path = paths
            .slot_home("dia1")
            .join("sessions/2026/05/08/rollout-thread.jsonl");
        let unrelated_path = paths
            .base_codex_home
            .join("sessions/2026/05/08/rollout-thread.jsonl");

        assert_eq!(
            slot_from_thread_path(&paths, rollout_path.to_str()),
            Some(String::from("dia1"))
        );
        assert_eq!(slot_from_thread_path(&paths, unrelated_path.to_str()), None);
        assert_eq!(slot_from_thread_path(&paths, None), None);
    }

    #[test]
    fn explicit_thread_falls_back_to_bound_path_resume_and_rebinds_session() {
        let paths = temp_paths("explicit-path-resume");
        let channel_id = ChannelId::parse("terminal").unwrap();
        let session = session::create_session(
            &paths,
            CreateSessionRequest {
                session_id: None,
                channel_id: channel_id.clone(),
            },
        )
        .unwrap()
        .session;
        session::bind_app_thread(
            &paths,
            BindAppThreadRequest {
                session_id: session.session_id.clone(),
                app_thread: AppThreadBinding {
                    thread_id: "old-thread".to_string(),
                    codex_session_id: None,
                    cwd: "/tmp/project".to_string(),
                    title: Some("old".to_string()),
                    slot: Some("slot-a".to_string()),
                    generation: 1,
                    path: Some("/tmp/threads/old.jsonl".to_string()),
                    updated_at_unix: 10,
                },
            },
        )
        .unwrap();
        let mut client = FakeResolverClient {
            resume_result: Some(summary_with_path(
                "current-thread",
                "/tmp/project",
                "idle",
                40,
                Some("/tmp/threads/old.jsonl"),
            )),
            ..FakeResolverClient::default()
        };

        let outcome = resolve_app_thread(
            &paths,
            &mut client,
            ThreadResolverScope {
                cwd: PathBuf::from("/tmp/project"),
                channel_id: Some(channel_id),
                explicit_resume_id: Some(ExplicitResumeId::AppThreadOrCodexSession(
                    "old-thread".to_string(),
                )),
                slot: Some("slot-b".to_string()),
                generation: 2,
            },
        )
        .unwrap();

        assert_eq!(
            outcome.decision,
            ThreadResolverDecision::AttachExisting {
                thread_id: "current-thread".to_string()
            }
        );
        assert_eq!(
            client.resume_calls,
            vec![(
                "old-thread".to_string(),
                Some("/tmp/threads/old.jsonl".to_string()),
                Some("/tmp/project".to_string()),
                false,
            )]
        );
        let rebound = session::show_session(&paths, &session.session_id).unwrap();
        let app_thread = rebound.app_thread.unwrap();
        assert_eq!(app_thread.thread_id, "current-thread");
        assert_eq!(app_thread.slot.as_deref(), Some("slot-b"));
        assert_eq!(app_thread.generation, 2);
        assert_eq!(app_thread.path.as_deref(), Some("/tmp/threads/old.jsonl"));
    }

    #[test]
    fn explicit_cx_session_id_uses_bound_thread_not_session_id_as_thread() {
        let paths = temp_paths("explicit-cx-session");
        let channel_id = ChannelId::parse("terminal").unwrap();
        let session = session::create_session(
            &paths,
            CreateSessionRequest {
                session_id: None,
                channel_id: channel_id.clone(),
            },
        )
        .unwrap()
        .session;
        session::bind_app_thread(
            &paths,
            BindAppThreadRequest {
                session_id: session.session_id.clone(),
                app_thread: AppThreadBinding {
                    thread_id: "thread-1".to_string(),
                    codex_session_id: Some("codex-session-1".to_string()),
                    cwd: "/tmp/project".to_string(),
                    title: Some("bound".to_string()),
                    slot: Some("slot-a".to_string()),
                    generation: 1,
                    path: Some("/tmp/threads/thread-1.jsonl".to_string()),
                    updated_at_unix: 10,
                },
            },
        )
        .unwrap();
        let mut readable = BTreeMap::new();
        readable.insert(
            "thread-1".to_string(),
            summary("thread-1", "/tmp/project", "idle", 20),
        );
        let mut client = FakeResolverClient {
            readable,
            ..FakeResolverClient::default()
        };

        let outcome = resolve_app_thread(
            &paths,
            &mut client,
            ThreadResolverScope {
                cwd: PathBuf::from("/tmp/project"),
                channel_id: Some(channel_id),
                explicit_resume_id: Some(ExplicitResumeId::CxSession(session.session_id.clone())),
                slot: Some("slot-b".to_string()),
                generation: 2,
            },
        )
        .unwrap();

        assert_eq!(
            outcome.decision,
            ThreadResolverDecision::AttachExisting {
                thread_id: "thread-1".to_string()
            }
        );
        assert_eq!(client.read_calls, vec!["thread-1"]);
    }

    #[test]
    fn explicit_codex_session_id_uses_bound_app_thread_id_for_resume() {
        let paths = temp_paths("explicit-codex-session");
        let channel_id = ChannelId::parse("terminal").unwrap();
        let session = session::create_session(
            &paths,
            CreateSessionRequest {
                session_id: None,
                channel_id: channel_id.clone(),
            },
        )
        .unwrap()
        .session;
        session::bind_app_thread(
            &paths,
            BindAppThreadRequest {
                session_id: session.session_id.clone(),
                app_thread: AppThreadBinding {
                    thread_id: "thread-1".to_string(),
                    codex_session_id: Some("codex-session-1".to_string()),
                    cwd: "/tmp/project".to_string(),
                    title: Some("bound".to_string()),
                    slot: Some("slot-a".to_string()),
                    generation: 1,
                    path: Some("/tmp/threads/thread-1.jsonl".to_string()),
                    updated_at_unix: 10,
                },
            },
        )
        .unwrap();
        let mut readable = BTreeMap::new();
        readable.insert(
            "thread-1".to_string(),
            summary("thread-1", "/tmp/project", "idle", 20),
        );
        let mut client = FakeResolverClient {
            readable,
            ..FakeResolverClient::default()
        };

        let outcome = resolve_app_thread(
            &paths,
            &mut client,
            ThreadResolverScope {
                cwd: PathBuf::from("/tmp/project"),
                channel_id: Some(channel_id),
                explicit_resume_id: Some(ExplicitResumeId::AppThreadOrCodexSession(
                    "codex-session-1".to_string(),
                )),
                slot: Some("slot-b".to_string()),
                generation: 2,
            },
        )
        .unwrap();

        assert_eq!(
            outcome.decision,
            ThreadResolverDecision::AttachExisting {
                thread_id: "thread-1".to_string()
            }
        );
        assert_eq!(client.read_calls, vec!["codex-session-1", "thread-1"]);
        assert!(client.resume_calls.is_empty());
    }

    #[test]
    fn explicit_sess_like_codex_session_id_falls_back_when_no_cx_session_exists() {
        let paths = temp_paths("explicit-sess-like-codex-session");
        let channel_id = ChannelId::parse("terminal").unwrap();
        let session = session::create_session(
            &paths,
            CreateSessionRequest {
                session_id: None,
                channel_id: channel_id.clone(),
            },
        )
        .unwrap()
        .session;
        session::bind_app_thread(
            &paths,
            BindAppThreadRequest {
                session_id: session.session_id.clone(),
                app_thread: AppThreadBinding {
                    thread_id: "thread-1".to_string(),
                    codex_session_id: Some("sess_upstream".to_string()),
                    cwd: "/tmp/project".to_string(),
                    title: Some("bound".to_string()),
                    slot: Some("slot-a".to_string()),
                    generation: 1,
                    path: Some("/tmp/threads/thread-1.jsonl".to_string()),
                    updated_at_unix: 10,
                },
            },
        )
        .unwrap();
        let mut readable = BTreeMap::new();
        readable.insert(
            "thread-1".to_string(),
            summary("thread-1", "/tmp/project", "idle", 20),
        );
        let mut client = FakeResolverClient {
            readable,
            ..FakeResolverClient::default()
        };

        let outcome = resolve_app_thread(
            &paths,
            &mut client,
            ThreadResolverScope {
                cwd: PathBuf::from("/tmp/project"),
                channel_id: Some(channel_id),
                explicit_resume_id: Some(ExplicitResumeId::parse("sess_upstream")),
                slot: Some("slot-b".to_string()),
                generation: 2,
            },
        )
        .unwrap();

        assert_eq!(
            outcome.decision,
            ThreadResolverDecision::AttachExisting {
                thread_id: "thread-1".to_string()
            }
        );
        assert_eq!(client.read_calls, vec!["sess_upstream", "thread-1"]);
    }

    #[test]
    fn default_terminal_channel_does_not_attach_other_cwd_session() {
        let paths = temp_paths("terminal-cwd");
        let channel_id = ChannelId::parse("terminal").unwrap();
        let session = session::create_session(
            &paths,
            CreateSessionRequest {
                session_id: None,
                channel_id,
            },
        )
        .unwrap()
        .session;
        session::bind_app_thread(
            &paths,
            BindAppThreadRequest {
                session_id: session.session_id,
                app_thread: AppThreadBinding {
                    thread_id: "repo-a-thread".to_string(),
                    codex_session_id: None,
                    cwd: "/tmp/repo-a".to_string(),
                    title: Some("repo-a".to_string()),
                    slot: Some("slot-a".to_string()),
                    generation: 1,
                    path: Some("/tmp/repo-a.jsonl".to_string()),
                    updated_at_unix: 10,
                },
            },
        )
        .unwrap();
        let mut readable = BTreeMap::new();
        readable.insert(
            "started".to_string(),
            summary_with_path(
                "started",
                "/tmp/repo-b",
                "idle",
                20,
                Some("/tmp/repo-b.jsonl"),
            ),
        );
        let mut client = FakeResolverClient {
            readable,
            ..FakeResolverClient::default()
        };

        let outcome = resolve_app_thread(
            &paths,
            &mut client,
            ThreadResolverScope {
                cwd: PathBuf::from("/tmp/repo-b"),
                channel_id: None,
                explicit_resume_id: None,
                slot: Some("slot-b".to_string()),
                generation: 2,
            },
        )
        .unwrap();

        assert_eq!(
            outcome.decision,
            ThreadResolverDecision::StartNew {
                cwd: "/tmp/repo-b".to_string()
            }
        );
        assert_eq!(client.read_calls, vec!["started"]);
    }

    #[test]
    fn explicit_readable_thread_rebinds_current_path_without_path_resume() {
        let paths = temp_paths("explicit-readable-rebind");
        let channel_id = ChannelId::parse("terminal").unwrap();
        let session = session::create_session(
            &paths,
            CreateSessionRequest {
                session_id: None,
                channel_id: channel_id.clone(),
            },
        )
        .unwrap()
        .session;
        session::bind_app_thread(
            &paths,
            BindAppThreadRequest {
                session_id: session.session_id.clone(),
                app_thread: AppThreadBinding {
                    thread_id: "thread-1".to_string(),
                    codex_session_id: None,
                    cwd: "/tmp/project".to_string(),
                    title: Some("old".to_string()),
                    slot: Some("slot-a".to_string()),
                    generation: 1,
                    path: Some("/tmp/stale.jsonl".to_string()),
                    updated_at_unix: 10,
                },
            },
        )
        .unwrap();
        let mut readable = BTreeMap::new();
        readable.insert(
            "thread-1".to_string(),
            summary_with_path(
                "thread-1",
                "/tmp/project",
                "idle",
                50,
                Some("/tmp/current.jsonl"),
            ),
        );
        let mut client = FakeResolverClient {
            readable,
            ..FakeResolverClient::default()
        };

        let outcome = resolve_app_thread(
            &paths,
            &mut client,
            ThreadResolverScope {
                cwd: PathBuf::from("/tmp/project"),
                channel_id: Some(channel_id),
                explicit_resume_id: Some(ExplicitResumeId::AppThreadOrCodexSession(
                    "thread-1".to_string(),
                )),
                slot: Some("slot-b".to_string()),
                generation: 2,
            },
        )
        .unwrap();

        assert_eq!(
            outcome.decision,
            ThreadResolverDecision::AttachExisting {
                thread_id: "thread-1".to_string()
            }
        );
        assert!(client.resume_calls.is_empty());
        let rebound = session::show_session(&paths, &session.session_id).unwrap();
        let app_thread = rebound.app_thread.unwrap();
        assert_eq!(app_thread.path.as_deref(), Some("/tmp/current.jsonl"));
        assert_eq!(app_thread.slot.as_deref(), Some("slot-b"));
        assert_eq!(app_thread.generation, 2);
    }
}
