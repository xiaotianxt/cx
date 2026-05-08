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
    pub(crate) explicit_thread_id: Option<String>,
    pub(crate) slot: Option<String>,
    pub(crate) generation: u64,
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
    let channel_id = match scope.channel_id {
        Some(ref channel_id) => channel_id.clone(),
        None => ChannelId::parse("terminal")?,
    };

    if let Some(thread_id) = scope.explicit_thread_id.as_deref() {
        return resolve_explicit_thread(paths, client, &scope, &channel_id, &cwd, thread_id);
    }

    if let Some(session) = session::find_session_by_channel(paths, &channel_id)? {
        if let Some(outcome) = attach_session_thread(paths, client, &scope, &session, &cwd, None)? {
            return Ok(outcome);
        }
    }

    if let Some(session) = session::find_session_by_app_thread_cwd(paths, &cwd)? {
        if let Some(outcome) = attach_session_thread(paths, client, &scope, &session, &cwd, None)? {
            return Ok(outcome);
        }
    }

    let page = client.thread_list_filtered(ThreadListFilter {
        limit: 50,
        cwd: Some(&cwd),
        archived: Some(false),
        source_kinds: Some(INTERACTIVE_SOURCE_KINDS),
        use_state_db_only: false,
    })?;
    if let Some(summary) = select_best_thread(page.threads) {
        let session = ensure_session(paths, &channel_id)?;
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

    let started = client.thread_start(Some(&cwd))?;
    let read = client
        .thread_read(&started.upstream_thread_id, false)
        .with_context(|| {
            format!(
                "read started app-server thread {}",
                started.upstream_thread_id
            )
        })?;
    let session = ensure_session(paths, &channel_id)?;
    let _session = bind_summary(
        paths,
        session.session_id.clone(),
        &read.summary,
        &scope,
        Some(&cwd),
    )?;
    Ok(ThreadResolverOutcome {
        decision: ThreadResolverDecision::StartNew { cwd },
        thread_id: Some(read.summary.upstream_thread_id),
    })
}

fn resolve_explicit_thread(
    paths: &ManagerPaths,
    client: &mut impl ThreadResolverClient,
    scope: &ThreadResolverScope,
    channel_id: &ChannelId,
    cwd: &str,
    thread_id: &str,
) -> Result<ThreadResolverOutcome> {
    match client.thread_read(thread_id, false) {
        Ok(read) => {
            let session = ensure_session(paths, channel_id)?;
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
            let session = session::find_session_by_app_thread_id(paths, thread_id)?
                .or_else(|| {
                    session::find_session_by_channel(paths, channel_id)
                        .ok()
                        .flatten()
                })
                .or_else(|| {
                    session::find_session_by_app_thread_cwd(paths, cwd)
                        .ok()
                        .flatten()
                });
            if let Some(session) = session {
                if let Some(outcome) =
                    attach_session_thread(paths, client, scope, &session, cwd, Some(thread_id))?
                {
                    return Ok(outcome);
                }
            }
            Ok(ThreadResolverOutcome {
                decision: ThreadResolverDecision::Refuse {
                    reason: format!("explicit thread {thread_id} is not readable and no resumable path is bound: {read_err:#}"),
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

fn ensure_session(paths: &ManagerPaths, channel_id: &ChannelId) -> Result<session::SessionRecord> {
    if let Some(session) = session::find_session_by_channel(paths, channel_id)? {
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
                cwd: cwd_override.unwrap_or(&summary.cwd).to_string(),
                title: summary.title.clone(),
                slot: scope.slot.clone(),
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
        }
    }

    #[derive(Default)]
    struct FakeResolverClient {
        readable: BTreeMap<String, AppThreadSummary>,
        listed: Vec<AppThreadSummary>,
        started: Option<StartedThread>,
        resume_result: Option<AppThreadSummary>,
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
                explicit_thread_id: Some("old-thread".to_string()),
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
                explicit_thread_id: Some("thread-1".to_string()),
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
