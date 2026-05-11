use std::cell::RefCell;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use super::*;

#[derive(Default)]
struct CapturingTranscriptTarget {
    sent: RefCell<Vec<String>>,
    edited: RefCell<Vec<(i64, String)>>,
}

impl TelegramTranscriptTarget for CapturingTranscriptTarget {
    fn send_one(&self, text: &str) -> Result<i64> {
        self.sent.borrow_mut().push(text.to_string());
        Ok(self.sent.borrow().len() as i64)
    }

    fn edit_one(&self, message_id: i64, text: &str) -> Result<()> {
        self.edited
            .borrow_mut()
            .push((message_id, text.to_string()));
        Ok(())
    }
}

fn temp_paths(name: &str) -> ManagerPaths {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "cx-telegram-test-{name}-{}-{unique}",
        std::process::id()
    ));
    ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
}

fn text_message(chat_id: i64, message_thread_id: Option<i64>, text: &str) -> TelegramMessage {
    TelegramMessage {
        chat: TelegramChat {
            id: chat_id,
            is_forum: false,
        },
        message_id: 1,
        date: None,
        message_thread_id,
        text: Some(text.to_string()),
        forum_topic_edited: None,
    }
}

fn handle_options() -> HandleOptions<'static> {
    HandleOptions {
        notifier: None,
        acquire_lease: false,
        steal: false,
        authorized_by_bind: false,
        app_server_timeout: 600.0,
        trace_timings: false,
        callback_data: None,
        callback_message_id: None,
    }
}

fn manual_binding(chat_id: i64, message_thread_id: Option<i64>) -> TelegramBinding {
    TelegramBinding {
        chat_id,
        message_thread_id,
        alias: None,
        channel_id: ChannelId::parse(format!(
            "telegram:{chat_id}{}",
            message_thread_id
                .map(|thread_id| format!(":topic:{thread_id}"))
                .unwrap_or_default()
        ))
        .unwrap(),
        session_id: SessionId::parse("sess_manual").unwrap(),
        app_thread_id: None,
        app_thread_title: None,
        app_thread_cwd: None,
        topic_title: None,
        panel_message_id: None,
        telegram_paused: false,
        topic_created_by_adapter: false,
        watch_app_last_turn_id: None,
        watch_last_agent_message: None,
        watch_activity: None,
        watch_thinking: None,
        watch_status: None,
        watch_pending_approvals: Vec::new(),
    }
}

#[test]
fn telegram_state_round_trips_without_token() {
    let paths = temp_paths("state");
    let mut state = TelegramState::empty();
    state.last_update_id = Some(123);
    state.bindings.push(manual_binding(42, None));

    write_state(&paths, &state).unwrap();
    let read_back = read_state(&paths).unwrap();

    assert_eq!(read_back, state);
    let content = fs::read_to_string(paths.telegram_channel_state_file()).unwrap();
    assert!(!content.contains("bot"));
    assert!(!content.contains("token"));

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn start_message_creates_session_binding() {
    let paths = temp_paths("start");
    let mut state = TelegramState::empty();
    let message = text_message(42, None, "/start");

    let reply = handle_message(&paths, &mut state, message, handle_options())
        .unwrap()
        .unwrap();

    assert_eq!(reply.chat_id, 42);
    assert!(reply.text.contains("Bound route 42 to cx session"));
    assert_eq!(state.bindings.len(), 1);
    assert_eq!(session::list_sessions(&paths).unwrap().len(), 1);

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn release_reports_missing_binding() {
    let paths = temp_paths("release-missing");
    let mut state = TelegramState::empty();
    let message = text_message(42, None, "/release");

    let reply = handle_message(&paths, &mut state, message, handle_options())
        .unwrap()
        .unwrap();

    assert!(reply.text.contains("No cx session is bound to route 42"));

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn resolved_resume_cwd_uses_rebound_session_cwd_before_snapshot_cwd() {
    let paths = temp_paths("resolved-resume-cwd");
    let channel_id = ChannelId::parse("telegram:42").unwrap();
    let session = session::create_session(
        &paths,
        session::CreateSessionRequest {
            session_id: Some(SessionId::parse("sess_manual").unwrap()),
            channel_id: channel_id.clone(),
        },
    )
    .unwrap()
    .session;
    session::bind_app_thread(
        &paths,
        session::BindAppThreadRequest {
            session_id: session.session_id.clone(),
            app_thread: session::AppThreadBinding {
                thread_id: "thread-new".to_string(),
                codex_session_id: None,
                cwd: "/repo/new".to_string(),
                title: None,
                slot: None,
                generation: 0,
                path: None,
                updated_at_unix: 10,
            },
        },
    )
    .unwrap();
    let mut binding = manual_binding(42, None);
    binding.channel_id = channel_id;
    binding.app_thread_id = Some("thread-old".to_string());
    binding.app_thread_cwd = Some("/repo/old".to_string());

    assert_eq!(
        resolved_resume_cwd(&paths, &binding, "thread-new").as_deref(),
        Some("/repo/new")
    );
    assert_eq!(
        resolved_resume_cwd(&paths, &binding, "thread-other").as_deref(),
        Some("/repo/old")
    );
}

#[test]
fn bind_command_trusts_matching_secret_for_unknown_chat() {
    let state = TelegramState::empty();
    let trusted = BTreeSet::new();
    let message = text_message(42, None, "/bind secret-123");

    let access = message_access(&message, &state, &trusted, Some("secret-123"), true);

    assert_eq!(access, MessageAccess::AuthorizedByBind);
}

#[test]
fn bind_command_rejects_wrong_secret_for_unknown_chat() {
    let state = TelegramState::empty();
    let trusted = BTreeSet::new();
    let message = text_message(42, None, "/bind wrong");

    let access = message_access(&message, &state, &trusted, Some("secret-123"), true);

    assert_eq!(access, MessageAccess::DeniedBind);
}

#[test]
fn trusted_route_trusts_forum_topic_routes() {
    let mut state = TelegramState::empty();
    state.trust_route(&TelegramRoute {
        chat_id: -10042,
        message_thread_id: None,
    });
    let trusted = trusted_chats(&state, Vec::new());
    let message = text_message(-10042, Some(99), "/portal");

    let access = message_access(&message, &state, &trusted, None, true);

    assert_eq!(access, MessageAccess::Allowed);
}

#[test]
fn existing_binding_is_trusted_without_allow_chat() {
    let mut state = TelegramState::empty();
    state.bindings.push(manual_binding(42, None));
    let trusted = trusted_chats(&state, Vec::new());

    assert!(trusted.contains(&TelegramRoute {
        chat_id: 42,
        message_thread_id: None
    }));
}

#[test]
fn bot_commands_cover_supported_telegram_commands() {
    let commands = bot_commands()
        .iter()
        .map(|command| command.command)
        .collect::<Vec<_>>();

    assert_eq!(commands, vec!["start", "bind", "portal", "status"]);
}

#[test]
fn topic_route_channel_id_includes_thread_id() {
    let route = TelegramRoute {
        chat_id: -10042,
        message_thread_id: Some(99),
    };

    assert_eq!(
        route.channel_id(Some("build")).unwrap().to_string(),
        "telegram:-10042:topic:99:session:build"
    );
}

#[test]
fn portal_callback_data_parses_observe() {
    assert_eq!(
        TelegramCallbackCommand::parse("cx:p"),
        Some(TelegramCallbackCommand::RefreshPortal)
    );
    assert_eq!(
        TelegramCallbackCommand::parse("cx:w"),
        Some(TelegramCallbackCommand::RefreshWork)
    );
    assert_eq!(
        TelegramCallbackCommand::parse("cx:t"),
        Some(TelegramCallbackCommand::Takeover)
    );
    assert_eq!(
        TelegramCallbackCommand::parse("cx:rel"),
        Some(TelegramCallbackCommand::Release)
    );
    assert_eq!(
        TelegramCallbackCommand::parse("cx:c"),
        Some(TelegramCallbackCommand::Close)
    );
    assert_eq!(
        TelegramCallbackCommand::parse("cx:o:thread-456"),
        Some(TelegramCallbackCommand::Observe {
            thread_id: "thread-456".to_string()
        })
    );
    assert_eq!(TelegramCallbackCommand::parse("noop"), None);
}

#[test]
fn app_server_threads_are_watchable_even_when_not_loaded() {
    let entry = AppThreadPortalEntry::from(AppThreadSummary {
        upstream_thread_id: "thread-not-loaded".to_string(),
        session_id: None,
        title: None,
        preview: "Idle desktop thread".to_string(),
        cwd: "/Users/yupeit/dev/cx".to_string(),
        path: None,
        source: "cli".to_string(),
        active_turn_id: None,
        active: false,
        status: "notLoaded".to_string(),
        created_at_unix: 1,
        updated_at_unix: 2,
        broker_subscriber_count: None,
    });

    assert!(entry.watchable);
    assert!(!entry.active);
    assert_eq!(entry.status, "notLoaded");
}

#[test]
fn portal_keyboard_uses_observe_callbacks() {
    let entries = vec![AppThreadPortalEntry {
        thread_id: "019dfeec-78ca-7cb0-a497-cd3a79f1329a".to_string(),
        title: Some("Fix Telegram handoff".to_string()),
        preview: String::new(),
        cwd: "/Users/yupeit/dev/cx".to_string(),
        active: true,
        watchable: true,
        status: "active".to_string(),
    }];

    let keyboard = portal_keyboard(&entries);

    assert_eq!(keyboard.inline_keyboard.len(), 2);
    assert_eq!(keyboard.inline_keyboard[0].len(), 1);
    assert_eq!(
        keyboard.inline_keyboard[0][0].callback_data,
        "cx:o:019dfeec-78ca-7cb0-a497-cd3a79f1329a"
    );
    assert!(keyboard.inline_keyboard[0][0].text.starts_with("Watch:"));
    assert_eq!(keyboard.inline_keyboard[1][0].text, "Refresh");
    assert_eq!(keyboard.inline_keyboard[1][0].callback_data, "cx:p");
}

#[test]
fn app_thread_binding_lookup_reuses_existing_topic() {
    let mut state = TelegramState::empty();
    let mut portal_binding = manual_binding(-10042, None);
    portal_binding.app_thread_id = Some("thread-1".to_string());
    let mut topic_binding = manual_binding(-10042, Some(77));
    topic_binding.app_thread_id = Some("thread-1".to_string());
    topic_binding.topic_created_by_adapter = true;
    state.bindings.push(portal_binding);
    state.bindings.push(topic_binding);

    let binding = state
        .app_thread_binding_for_chat(-10042, "thread-1")
        .unwrap();

    assert_eq!(binding.message_thread_id, Some(77));
    assert!(binding.topic_created_by_adapter);
}

#[test]
fn filter_portal_entries_keeps_registry_order_and_limits() {
    let entries = vec![
        AppThreadPortalEntry {
            thread_id: "thread-first".to_string(),
            title: Some("First".to_string()),
            preview: String::new(),
            cwd: "/Users/yupeit/dev/cx".to_string(),
            active: false,
            watchable: false,
            status: "notLoaded".to_string(),
        },
        AppThreadPortalEntry {
            thread_id: "thread-second".to_string(),
            title: Some("Second".to_string()),
            preview: String::new(),
            cwd: "/Users/yupeit/dev/cx".to_string(),
            active: true,
            watchable: true,
            status: "active".to_string(),
        },
        AppThreadPortalEntry {
            thread_id: "thread-third".to_string(),
            title: Some("Third".to_string()),
            preview: String::new(),
            cwd: "/Users/yupeit/dev/cx".to_string(),
            active: false,
            watchable: false,
            status: "notLoaded".to_string(),
        },
    ];

    let filtered = filter_portal_entries(entries, 2);

    assert_eq!(filtered.len(), 2);
    assert_eq!(filtered[0].thread_id, "thread-first");
    assert_eq!(filtered[1].thread_id, "thread-second");
}

#[test]
fn activity_watch_text_renders_concise_running_command() {
    let mut panel = TelegramActivityPanel::new();
    panel.apply_execution(CommandExecution {
        item_id: "cmd-1".to_string(),
        command: "sed -n '1,20p' src/channel.rs".to_string(),
        cwd: "/tmp/project".to_string(),
        activity: Some(CommandActivity {
            verb: "Read".to_string(),
            target: "channel.rs".to_string(),
        }),
        status: CommandExecutionStatus::InProgress,
        exit_code: None,
        duration_ms: None,
        aggregated_output: Some("long command output that should not be sent".to_string()),
    });

    let text = activity_watch_text(&panel);

    assert!(text.contains("• **Exploring**"));
    assert!(text.contains("Read `channel.rs`"));
    assert!(!text.contains("Running:"));
    assert!(!text.contains("cwd:"));
    assert!(!text.contains("output"));
}

#[test]
fn activity_watch_text_groups_transcript_sections() {
    let mut panel = TelegramActivityPanel::new();
    panel.apply_execution(CommandExecution {
        item_id: "read-1".to_string(),
        command: "sed -n '1,80p' wiley.py".to_string(),
        cwd: "/tmp/project".to_string(),
        activity: Some(CommandActivity {
            verb: "Read".to_string(),
            target: "wiley.py, science.py, pnas.py".to_string(),
        }),
        status: CommandExecutionStatus::Completed,
        exit_code: Some(0),
        duration_ms: Some(0),
        aggregated_output: None,
    });
    panel.apply_execution(CommandExecution {
        item_id: "test-1".to_string(),
        command: "cargo test".to_string(),
        cwd: "/tmp/project".to_string(),
        activity: None,
        status: CommandExecutionStatus::Completed,
        exit_code: Some(0),
        duration_ms: Some(1240),
        aggregated_output: None,
    });

    assert_eq!(
        activity_watch_text(&panel),
        "• **Explored**\n  └ Read `wiley.py`, `science.py`, `pnas.py`\n• **Ran** `cargo test` (1.2s)"
    );
}

#[test]
fn activity_panel_state_preserves_edit_target_across_drains() {
    let mut panel = TelegramActivityPanel::new();
    panel.apply_execution(CommandExecution {
        item_id: "read-1".to_string(),
        command: "sed -n '1,80p' telegram.rs".to_string(),
        cwd: "/tmp/project".to_string(),
        activity: Some(CommandActivity {
            verb: "Read".to_string(),
            target: "telegram.rs".to_string(),
        }),
        status: CommandExecutionStatus::Completed,
        exit_code: Some(0),
        duration_ms: Some(0),
        aggregated_output: None,
    });
    panel.message_id = Some(42);
    panel.last_sent_text = Some(activity_watch_text(&panel));

    let restored = TelegramActivityPanel::from_state(panel.to_state());

    assert_eq!(restored.message_id, Some(42));
    assert_eq!(
        activity_watch_text(&restored),
        "• **Explored**\n  └ Read `telegram.rs`"
    );
}

#[test]
fn watch_sink_seals_activity_cell_before_next_history_cell() {
    let target = CapturingTranscriptTarget::default();
    let mut panel = TelegramActivityPanel::new();
    panel.apply_execution(CommandExecution {
        item_id: "read-1".to_string(),
        command: "sed -n '1,80p' telegram.rs".to_string(),
        cwd: "/tmp/project".to_string(),
        activity: Some(CommandActivity {
            verb: "Read".to_string(),
            target: "telegram.rs".to_string(),
        }),
        status: CommandExecutionStatus::Completed,
        exit_code: Some(0),
        duration_ms: Some(0),
        aggregated_output: None,
    });
    assert!(panel.flush(&target, true).unwrap());

    let client = Client::new();
    let notifier = TelegramNotifier {
        client: &client,
        token: "test-token",
    };
    let route = TelegramRoute {
        chat_id: 1,
        message_thread_id: None,
    };
    let mut sink =
        TelegramWatchSink::new_best_effort(&notifier, route, panel.to_state(), None, None);

    sink.seal_activity_cell().unwrap();
    sink.activity.apply_execution(CommandExecution {
        item_id: "read-2".to_string(),
        command: "sed -n '1,80p' client.rs".to_string(),
        cwd: "/tmp/project".to_string(),
        activity: Some(CommandActivity {
            verb: "Read".to_string(),
            target: "client.rs".to_string(),
        }),
        status: CommandExecutionStatus::Completed,
        exit_code: Some(0),
        duration_ms: Some(0),
        aggregated_output: None,
    });

    assert_eq!(
        activity_watch_text(&sink.activity),
        "• **Explored**\n  └ Read `client.rs`"
    );
}

#[test]
fn status_panel_state_throttles_active_edit_across_drains() {
    let target = CapturingTranscriptTarget::default();
    let mut panel = TelegramStatusPanel::from_state(None);
    panel.start_new_turn();

    assert!(panel.flush(&target, false).unwrap());
    let mut restored = TelegramStatusPanel::from_state(panel.to_state());

    assert!(!restored.flush(&target, false).unwrap());
    assert_eq!(target.sent.borrow().len(), 1);
    assert!(target.sent.borrow()[0].contains("Working"));
    assert!(target.edited.borrow().is_empty());
}

#[test]
fn retry_after_delay_reads_telegram_rate_limit() {
    let err = anyhow::anyhow!("Telegram editMessageText failed: Too Many Requests: retry after 27");

    assert_eq!(
        telegram_retry_after_delay(&err),
        Some(Duration::from_secs(28))
    );
}

#[test]
fn watched_file_changes_use_binding_cwd_when_event_cwd_is_missing() {
    let events = events_with_default_cwd(
        vec![WatchEvent::Stream(AppStreamEvent::CommandCompleted(
            CommandExecution {
                item_id: "patch-1".to_string(),
                command: "apply_patch".to_string(),
                cwd: "<unknown>".to_string(),
                activity: Some(CommandActivity {
                    verb: "Edited".to_string(),
                    target: "/Users/yupeit/Develop.localized/ra/copyright/scripts/export-qualtrics-metrics.cjs (+1 -0)"
                        .to_string(),
                }),
                status: CommandExecutionStatus::Completed,
                exit_code: None,
                duration_ms: None,
                aggregated_output: None,
            },
        ))],
        Some("/Users/yupeit/Develop.localized/ra/copyright"),
    );
    let WatchEvent::Stream(AppStreamEvent::CommandCompleted(command)) =
        events.into_iter().next().unwrap()
    else {
        panic!("expected completed command");
    };

    let mut panel = TelegramActivityPanel::new();
    panel.apply_execution(command);

    assert_eq!(
        activity_watch_text(&panel),
        "• **Edited** `scripts/export-qualtrics-metrics.cjs` (+1 -0)"
    );
}

#[test]
fn work_panel_keyboard_switches_between_takeover_and_release() {
    let paths = temp_paths("work-panel-keyboard");
    let mut state = TelegramState::empty();
    let route = TelegramRoute {
        chat_id: -10042,
        message_thread_id: Some(99),
    };
    state.bind_route(&paths, &route, None).unwrap();
    let active = state.active_binding_for_route(&route).unwrap().clone();

    let active_keyboard = work_panel_keyboard(&active, &route);

    assert_eq!(
        active_keyboard.inline_keyboard[0][0].text,
        "Release to desktop"
    );
    assert_eq!(
        active_keyboard.inline_keyboard[0][0].callback_data,
        "cx:rel"
    );

    state
        .binding_for_route_mut(&route, None)
        .unwrap()
        .telegram_paused = true;
    let paused = state.active_binding_for_route(&route).unwrap();
    let paused_keyboard = work_panel_keyboard(paused, &route);

    assert_eq!(
        paused_keyboard.inline_keyboard[0][0].text,
        "Take over from desktop"
    );
    assert_eq!(paused_keyboard.inline_keyboard[0][0].callback_data, "cx:t");

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn telegram_noop_errors_are_success_conditions() {
    assert!(is_telegram_noop_error(
        "Bad Request: message is not modified: specified new message content and reply markup are exactly the same as a current content and reply markup of the message"
    ));
    assert!(is_telegram_noop_error("Bad Request: TOPIC_NOT_MODIFIED"));
    assert!(!is_telegram_noop_error(
        "Bad Request: message to edit not found"
    ));
    assert!(is_telegram_delete_noop_error(
        "Bad Request: message to delete not found"
    ));
}

#[test]
fn telegram_html_text_renders_code_and_escapes_plain_text() {
    assert_eq!(
        telegram_html_text("cwd: `/Users/yupeit/dev/cx` & <ok>"),
        "cwd: <code>/Users/yupeit/dev/cx</code> &amp; &lt;ok&gt;"
    );
    assert_eq!(
        telegram_html_text("• **Explored**\n  └ Read `src/main.rs`"),
        "• <b>Explored</b>\n  └ Read <code>src/main.rs</code>"
    );
    assert_eq!(
        telegram_html_text("```rust\nlet x = 1 < 2;\n```"),
        "<pre>let x = 1 &lt; 2;\n</pre>"
    );
    assert_eq!(telegram_html_text("unmatched `code"), "unmatched `code");
}

#[test]
fn approval_callback_data_round_trips() {
    let nonce = "abc123";

    let data = approval_callback_data(nonce, TelegramApprovalDecision::AcceptForSession);

    assert_eq!(
        parse_approval_callback_data(&data, nonce),
        Some(TelegramApprovalDecision::AcceptForSession)
    );
    assert_eq!(parse_approval_callback_data(&data, "other"), None);

    let cancel = approval_callback_data(nonce, TelegramApprovalDecision::Cancel);
    assert_eq!(
        parse_approval_callback_data(&cancel, nonce),
        Some(TelegramApprovalDecision::Cancel)
    );
}

#[test]
fn approval_response_maps_command_and_permissions_requests() {
    let command = ApprovalRequest {
        id: json!(1),
        method: "item/commandExecution/requestApproval".to_string(),
        params: json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "item-1",
            "command": "touch /tmp/x"
        }),
    };
    let command_result =
        approval_response_result(&command, TelegramApprovalDecision::AcceptForSession).unwrap();
    assert_eq!(
        command_result,
        json!({
            "decision": "acceptForSession"
        })
    );
    let cancel_result =
        approval_response_result(&command, TelegramApprovalDecision::Cancel).unwrap();
    assert_eq!(cancel_result["decision"], json!("cancel"));

    let permissions = ApprovalRequest {
        id: json!("request-1"),
        method: "item/permissions/requestApproval".to_string(),
        params: json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "item-1",
            "cwd": "/tmp",
            "permissions": {
                "network": { "enabled": true },
                "fileSystem": null
            }
        }),
    };
    let permissions_result =
        approval_response_result(&permissions, TelegramApprovalDecision::AcceptOnce).unwrap();
    assert_eq!(permissions_result["scope"], json!("turn"));
    assert_eq!(
        permissions_result["permissions"],
        permissions.params["permissions"]
    );
}

#[test]
fn release_pauses_telegram_handoff_without_active_lease() {
    let paths = temp_paths("release-pauses");
    let mut state = TelegramState::empty();
    let route = TelegramRoute {
        chat_id: 42,
        message_thread_id: None,
    };
    state.bind_route(&paths, &route, None).unwrap();

    let reply = handle_message(
        &paths,
        &mut state,
        text_message(42, None, "/release"),
        handle_options(),
    )
    .unwrap()
    .unwrap();

    assert!(reply.text.contains("Telegram handoff paused."));
    assert!(state
        .active_binding_for_route(&route)
        .is_some_and(|binding| binding.telegram_paused));

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn paused_handoff_blocks_normal_message() {
    let paths = temp_paths("paused-blocks");
    let mut state = TelegramState::empty();
    let route = TelegramRoute {
        chat_id: 42,
        message_thread_id: None,
    };
    state.bind_route(&paths, &route, None).unwrap();
    state
        .binding_for_route_mut(&route, None)
        .unwrap()
        .telegram_paused = true;

    let reply = handle_message(
        &paths,
        &mut state,
        text_message(42, None, "continue this"),
        handle_options(),
    )
    .unwrap()
    .unwrap();

    assert!(reply
        .text
        .starts_with("Telegram handoff is paused for this topic."));
    assert_eq!(
        reply.reply_markup.unwrap().inline_keyboard[0][0].callback_data,
        "cx:t"
    );

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn plain_text_in_forum_router_returns_portal_panel() {
    let paths = temp_paths("router-text-panel");
    let mut state = TelegramState::empty();
    let message = TelegramMessage {
        chat: TelegramChat {
            id: -10042,
            is_forum: true,
        },
        message_id: 1,
        date: None,
        message_thread_id: None,
        text: Some("show me threads".to_string()),
        forum_topic_edited: None,
    };

    let reply = handle_message(&paths, &mut state, message, handle_options())
        .unwrap()
        .unwrap();

    assert!(reply.text.contains("Codex portal is unavailable."));
    assert!(reply.remember_panel_message);
    assert_eq!(
        reply.reply_markup.unwrap().inline_keyboard[0][0].callback_data,
        "cx:p"
    );
    assert!(state.bindings.is_empty());

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn non_text_service_message_is_ignored() {
    let paths = temp_paths("non-text-service-message");
    let mut state = TelegramState::empty();
    let route = TelegramRoute {
        chat_id: -10042,
        message_thread_id: Some(99),
    };
    state.bind_route(&paths, &route, None).unwrap();

    let reply = handle_message(
        &paths,
        &mut state,
        TelegramMessage {
            chat: TelegramChat {
                id: -10042,
                is_forum: true,
            },
            message_id: 1,
            date: None,
            message_thread_id: Some(99),
            text: None,
            forum_topic_edited: None,
        },
        handle_options(),
    )
    .unwrap();

    assert!(reply.is_none());
    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn forum_topic_edit_updates_title_metadata() {
    let paths = temp_paths("topic-title-edit");
    let mut state = TelegramState::empty();
    let route = TelegramRoute {
        chat_id: -10042,
        message_thread_id: Some(99),
    };
    state.bind_route(&paths, &route, None).unwrap();

    let reply = handle_message(
        &paths,
        &mut state,
        TelegramMessage {
            chat: TelegramChat {
                id: -10042,
                is_forum: true,
            },
            message_id: 1,
            date: None,
            message_thread_id: Some(99),
            text: None,
            forum_topic_edited: Some(TelegramForumTopicEdited {
                name: Some("mobile handoff".to_string()),
            }),
        },
        handle_options(),
    )
    .unwrap();

    assert!(reply.is_none());
    assert_eq!(
        state
            .binding_for_route(&route, None)
            .and_then(|binding| binding.topic_title.as_deref()),
        Some("mobile handoff")
    );

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn app_server_history_watch_establishes_cursor_without_replaying_existing_turns() {
    let turns = vec![json!({"id": "turn-1", "items": [], "status": "completed"})];

    let (events, latest) = app_server_history_events_since(&turns, None, false);

    assert!(events.is_empty());
    assert_eq!(latest.as_deref(), Some("turn-1"));
}

#[test]
fn app_server_history_watch_replays_turns_after_last_seen_cursor() {
    let turns = vec![
        json!({"id": "turn-1", "items": [], "status": "completed"}),
        json!({
            "id": "turn-2",
            "status": "completed",
            "durationMs": 12,
            "items": [
                {"userMessage": {"id": "u2", "content": [{"type": "text", "text": "hi"}]}},
                {"agentMessage": {"id": "a2", "text": "hello"}}
            ]
        }),
    ];

    let (events, latest) = app_server_history_events_since(&turns, Some("turn-1"), false);

    assert_eq!(latest.as_deref(), Some("turn-2"));
    assert_eq!(
        events,
        vec![
            WatchEvent::Stream(AppStreamEvent::TurnStarted),
            WatchEvent::Stream(AppStreamEvent::UserMessage("hi".to_string())),
            WatchEvent::Stream(AppStreamEvent::AgentDelta("hello".to_string())),
            WatchEvent::Terminal {
                turn_id: Some("turn-2".to_string()),
                terminal: WatchTerminal::Completed,
                duration_ms: Some(12),
                last_agent_message: Some("hello".to_string()),
            },
        ]
    );
}

#[test]
fn app_server_history_watch_closes_active_cursor_when_terminal_was_missed() {
    let turns = vec![json!({
        "id": "turn-1",
        "status": "interrupted",
        "durationMs": 42,
        "items": [
            {"agentMessage": {"id": "a1", "text": "stopped"}}
        ]
    })];

    let (events, latest) = app_server_history_events_since(&turns, Some("turn-1"), true);

    assert_eq!(latest.as_deref(), Some("turn-1"));
    assert_eq!(
        events,
        vec![WatchEvent::Terminal {
            turn_id: Some("turn-1".to_string()),
            terminal: WatchTerminal::Aborted,
            duration_ms: Some(42),
            last_agent_message: Some("stopped".to_string()),
        }]
    );
}

#[test]
fn app_server_history_watch_does_not_repeat_terminal_for_inactive_cursor() {
    let turns = vec![json!({"id": "turn-1", "items": [], "status": "completed"})];

    let (events, latest) = app_server_history_events_since(&turns, Some("turn-1"), false);

    assert_eq!(latest.as_deref(), Some("turn-1"));
    assert!(events.is_empty());
}

#[test]
fn recent_history_text_reads_user_and_agent_messages_from_app_server_turns() {
    let turns = vec![
        json!({
            "id": "turn-1",
            "items": [
                {"userMessage": {"content": [{"type": "text", "text": "old"}]}},
                {"agentMessage": {"text": "older"}}
            ]
        }),
        json!({
            "id": "turn-2",
            "items": [
                {"userMessage": {"content": [{"type": "text", "text": "why can I not see this chat?"}]}},
                {"agentMessage": {"text": "because the watcher was not replaying app-server history"}}
            ]
        }),
    ];

    let text = recent_history_text_from_turns(&turns, 2).unwrap();

    assert!(!text.contains("old"));
    assert!(text.contains("You: why can I not see this chat?"));
    assert!(text.contains("Codex: because the watcher was not replaying app-server history"));
}
