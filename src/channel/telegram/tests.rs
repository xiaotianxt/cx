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

#[test]
fn telegram_state_round_trips_without_token() {
    let paths = temp_paths("state");
    let mut state = TelegramState::empty();
    state.last_update_id = Some(123);
    state.bindings.push(TelegramBinding {
        chat_id: 42,
        message_thread_id: None,
        alias: None,
        channel_id: ChannelId::parse("telegram:42").unwrap(),
        session_id: SessionId::parse("sess_manual").unwrap(),
        app_thread_id: None,
        app_thread_title: None,
        app_thread_cwd: None,
        topic_title: None,
        panel_message_id: None,
        telegram_paused: false,
        topic_created_by_adapter: false,
        watch_proxy_offset: None,
        watch_rollout_offset: None,
        watch_source: None,
        watch_last_agent_message: None,
        watch_activity: None,
        watch_thinking: None,
        watch_status: None,
        watch_pending_approvals: Vec::new(),
    });

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

    assert_eq!(
                reply.text,
                "No cx session is bound to route 42. Send /start to create the default session or /new <name> to create a named one."
            );

    let _ = fs::remove_dir_all(paths.serve_dir());
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
fn bind_command_uses_chat_scope_for_forum_messages() {
    let message = TelegramMessage {
        chat: TelegramChat {
            id: -10042,
            is_forum: true,
        },
        message_id: 1,
        date: None,
        message_thread_id: Some(99),
        text: Some("/bind secret-123".to_string()),
        forum_topic_edited: None,
    };

    assert_eq!(
        trusted_route_for_message(&message),
        TelegramRoute {
            chat_id: -10042,
            message_thread_id: None
        }
    );
}

#[test]
fn bind_command_does_not_create_session_binding() {
    let paths = temp_paths("bind-trust-only");
    let mut state = TelegramState::empty();
    let mut options = handle_options();
    options.authorized_by_bind = true;
    let message = text_message(42, None, "/bind secret-123");

    let reply = handle_message(&paths, &mut state, message, options)
        .unwrap()
        .unwrap();

    assert_eq!(reply.text, "Trusted Telegram route 42.");
    assert!(state.bindings.is_empty());
    assert!(session::list_sessions(&paths).unwrap().is_empty());

    let _ = fs::remove_dir_all(paths.serve_dir());
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
    state.bindings.push(TelegramBinding {
        chat_id: 42,
        message_thread_id: None,
        alias: None,
        channel_id: ChannelId::parse("telegram:42").unwrap(),
        session_id: SessionId::parse("sess_manual").unwrap(),
        app_thread_id: None,
        app_thread_title: None,
        app_thread_cwd: None,
        topic_title: None,
        panel_message_id: None,
        telegram_paused: false,
        topic_created_by_adapter: false,
        watch_proxy_offset: None,
        watch_rollout_offset: None,
        watch_source: None,
        watch_last_agent_message: None,
        watch_activity: None,
        watch_thinking: None,
        watch_status: None,
        watch_pending_approvals: Vec::new(),
    });
    let trusted = trusted_chats(&state, Vec::new());

    assert!(trusted.contains(&TelegramRoute {
        chat_id: 42,
        message_thread_id: None
    }));
}

#[test]
fn chat_level_binding_trusts_forum_topic_routes() {
    let mut state = TelegramState::empty();
    state.bindings.push(TelegramBinding {
        chat_id: -10042,
        message_thread_id: None,
        alias: None,
        channel_id: ChannelId::parse("telegram:-10042").unwrap(),
        session_id: SessionId::parse("sess_manual").unwrap(),
        app_thread_id: None,
        app_thread_title: None,
        app_thread_cwd: None,
        topic_title: None,
        panel_message_id: None,
        telegram_paused: false,
        topic_created_by_adapter: false,
        watch_proxy_offset: None,
        watch_rollout_offset: None,
        watch_source: None,
        watch_last_agent_message: None,
        watch_activity: None,
        watch_thinking: None,
        watch_status: None,
        watch_pending_approvals: Vec::new(),
    });
    let trusted = trusted_chats(&state, Vec::new());
    let message = text_message(-10042, Some(99), "/new build");

    let access = message_access(&message, &state, &trusted, None, true);

    assert_eq!(access, MessageAccess::Allowed);
}

#[test]
fn allow_chat_trusts_forum_topic_routes() {
    let state = TelegramState::empty();
    let trusted = trusted_chats(&state, vec![-10042]);
    let message = text_message(-10042, Some(99), "/new build");

    let access = message_access(&message, &state, &trusted, None, true);

    assert_eq!(access, MessageAccess::Allowed);
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
fn topic_reply_preserves_message_thread_id() {
    let paths = temp_paths("topic-reply");
    let mut state = TelegramState::empty();
    let message = text_message(-10042, Some(99), "/new Build!");

    let reply = handle_message(&paths, &mut state, message, handle_options())
        .unwrap()
        .unwrap();

    assert_eq!(reply.chat_id, -10042);
    assert_eq!(reply.message_thread_id, Some(99));

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn new_command_creates_named_route_session_and_marks_active() {
    let paths = temp_paths("new");
    let mut state = TelegramState::empty();
    let message = text_message(42, None, "/new Build!");

    let reply = handle_message(&paths, &mut state, message, handle_options())
        .unwrap()
        .unwrap();

    assert!(reply.text.contains("Created session `build`"));
    assert!(state
        .binding_for_route(
            &TelegramRoute {
                chat_id: 42,
                message_thread_id: None
            },
            Some("build")
        )
        .is_some());
    assert_eq!(state.active_routes[0].alias.as_deref(), Some("build"));

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn use_command_switches_active_route_session() {
    let paths = temp_paths("use");
    let mut state = TelegramState::empty();
    let route = TelegramRoute {
        chat_id: 42,
        message_thread_id: None,
    };
    state.bind_route(&paths, &route, Some("alpha")).unwrap();
    state.bind_route(&paths, &route, Some("beta")).unwrap();

    let reply = handle_message(
        &paths,
        &mut state,
        text_message(42, None, "/use alpha"),
        handle_options(),
    )
    .unwrap()
    .unwrap();

    assert_eq!(reply.text, "Using session `alpha`.");
    assert_eq!(state.active_routes[0].alias.as_deref(), Some("alpha"));

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn close_command_unbinds_named_route_session() {
    let paths = temp_paths("close");
    let mut state = TelegramState::empty();
    let route = TelegramRoute {
        chat_id: 42,
        message_thread_id: None,
    };
    state.bind_route(&paths, &route, Some("alpha")).unwrap();

    let reply = handle_message(
        &paths,
        &mut state,
        text_message(42, None, "/close alpha"),
        handle_options(),
    )
    .unwrap()
    .unwrap();

    assert_eq!(reply.text, "Unbound session `alpha`.");
    assert!(state.binding_for_route(&route, Some("alpha")).is_none());

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn update_view_accepts_channel_posts() {
    let update = TelegramUpdate {
        update_id: 7,
        message: None,
        edited_message: None,
        channel_post: Some(TelegramMessage {
            chat: TelegramChat {
                id: -10042,
                is_forum: false,
            },
            message_id: 1,
            date: None,
            message_thread_id: Some(7),
            text: Some(String::from("/start")),
            forum_topic_edited: None,
        }),
        edited_channel_post: None,
        callback_query: None,
        my_chat_member: None,
    };

    let view = update.view();

    assert_eq!(view.source, TelegramUpdateSource::ChannelPost);
    assert_eq!(view.chat_id, Some(-10042));
    assert_eq!(
        view.message.and_then(|message| message.text),
        Some(String::from("/start"))
    );
}

#[test]
fn update_view_reports_membership_updates_without_message() {
    let update = TelegramUpdate {
        update_id: 8,
        message: None,
        edited_message: None,
        channel_post: None,
        edited_channel_post: None,
        callback_query: None,
        my_chat_member: Some(TelegramChatMemberUpdated {
            chat: TelegramChat {
                id: 42,
                is_forum: false,
            },
        }),
    };

    let view = update.view();

    assert_eq!(view.source, TelegramUpdateSource::MyChatMember);
    assert_eq!(view.chat_id, Some(42));
    assert!(view.message.is_none());
}

#[test]
fn telegram_chat_deserializes_with_is_forum() {
    let json = r#"{"id": -1003586916929, "is_forum": true}"#;
    let chat: TelegramChat = serde_json::from_str(json).unwrap();

    assert_eq!(chat.id, -1003586916929);
    assert!(chat.is_forum);
}

#[test]
fn telegram_chat_defaults_is_forum_to_false() {
    let json = r#"{"id": 42}"#;
    let chat: TelegramChat = serde_json::from_str(json).unwrap();

    assert_eq!(chat.id, 42);
    assert!(!chat.is_forum);
}

#[test]
fn new_in_forum_creates_session_without_api_when_no_notifier() {
    let paths = temp_paths("new-forum-nonotif");
    let mut state = TelegramState::empty();
    let message = TelegramMessage {
        chat: TelegramChat {
            id: -10042,
            is_forum: true,
        },
        message_id: 1,
        date: None,
        message_thread_id: None,
        text: Some("/new Build!".to_string()),
        forum_topic_edited: None,
    };

    let reply = handle_message(&paths, &mut state, message, handle_options())
        .unwrap()
        .unwrap();

    assert!(reply.text.contains("Created session `build`"));
    assert!(reply.text.contains("route"));
    assert_eq!(state.bindings.len(), 1);
    assert_eq!(state.bindings[0].chat_id, -10042);
    assert_eq!(state.bindings[0].message_thread_id, None);
    assert_eq!(state.bindings[0].alias.as_deref(), Some("build"));
    assert!(!state.bindings[0].topic_created_by_adapter);

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn new_inside_forum_topic_creates_session_within_topic() {
    let paths = temp_paths("new-topic");
    let mut state = TelegramState::empty();
    let message = TelegramMessage {
        chat: TelegramChat {
            id: -10042,
            is_forum: true,
        },
        message_id: 1,
        date: None,
        message_thread_id: Some(99),
        text: Some("/new config".to_string()),
        forum_topic_edited: None,
    };

    let reply = handle_message(&paths, &mut state, message, handle_options())
        .unwrap()
        .unwrap();

    assert!(reply.text.contains("Created session `config`"));
    assert_eq!(state.bindings.len(), 1);
    assert_eq!(state.bindings[0].chat_id, -10042);
    assert_eq!(state.bindings[0].message_thread_id, Some(99));
    assert_eq!(state.bindings[0].alias.as_deref(), Some("config"));

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn close_topic_binding_removes_session() {
    let paths = temp_paths("close-topic");
    let mut state = TelegramState::empty();
    let route = TelegramRoute {
        chat_id: -10042,
        message_thread_id: Some(99),
    };
    state.bind_route(&paths, &route, Some("smoke")).unwrap();
    assert!(state.binding_for_route(&route, Some("smoke")).is_some());

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
            text: Some("/close smoke".to_string()),
            forum_topic_edited: None,
        },
        handle_options(),
    )
    .unwrap()
    .unwrap();

    assert_eq!(reply.text, "Unbound session `smoke`.");
    assert!(state.binding_for_route(&route, Some("smoke")).is_none());

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn close_topic_binding_does_not_panic_without_notifier() {
    let paths = temp_paths("close-topic-nonotif");
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
            text: Some("/close".to_string()),
            forum_topic_edited: None,
        },
        handle_options(),
    )
    .unwrap()
    .unwrap();

    assert_eq!(reply.text, "Unbound session `default`.");
    assert!(state.binding_for_route(&route, None).is_none());

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn close_without_alias_unbinds_active_named_session() {
    let paths = temp_paths("close-active-named");
    let mut state = TelegramState::empty();
    let route = TelegramRoute {
        chat_id: -10042,
        message_thread_id: Some(31),
    };
    state.bind_route(&paths, &route, Some("session-2")).unwrap();

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
            message_thread_id: Some(31),
            text: Some("/close".to_string()),
            forum_topic_edited: None,
        },
        handle_options(),
    )
    .unwrap()
    .unwrap();

    assert_eq!(reply.text, "Unbound session `session-2`.");
    assert!(state.binding_for_route(&route, Some("session-2")).is_none());

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn close_unbinds_local_route_without_archiving_app_thread() {
    let paths = temp_paths("close-preserve-app-thread");
    let mut state = TelegramState::empty();
    let route = TelegramRoute {
        chat_id: -10042,
        message_thread_id: Some(31),
    };
    state.bind_route(&paths, &route, None).unwrap();
    state
        .binding_for_route_mut(&route, None)
        .unwrap()
        .app_thread_id = Some("thread-missing".to_string());

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
            message_thread_id: Some(31),
            text: Some("/close".to_string()),
            forum_topic_edited: None,
        },
        handle_options(),
    )
    .unwrap()
    .unwrap();

    assert_eq!(reply.text, "Unbound session `default`.");
    assert!(state.binding_for_route(&route, None).is_none());

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn deleting_topic_route_removes_all_local_bindings_for_that_topic() {
    let paths = temp_paths("remove-topic-route");
    let mut state = TelegramState::empty();
    let route = TelegramRoute {
        chat_id: -10042,
        message_thread_id: Some(31),
    };
    state.bind_route(&paths, &route, None).unwrap();
    state.bind_route(&paths, &route, Some("alpha")).unwrap();
    state.set_active_route(&route, Some("alpha"));

    assert!(state.remove_all_route_bindings(&route));

    assert!(state.bindings.is_empty());
    assert!(state.active_routes.is_empty());

    let _ = fs::remove_dir_all(paths.serve_dir());
}

#[test]
fn portal_callback_data_parses_watch() {
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
        TelegramCallbackCommand::parse("cx:a:thread-123"),
        Some(TelegramCallbackCommand::Observe {
            thread_id: "thread-123".to_string()
        })
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
        title: None,
        preview: "Idle desktop thread".to_string(),
        cwd: "/Users/yupeit/dev/cx".to_string(),
        source: "cli".to_string(),
        active: false,
        status: "notLoaded".to_string(),
        created_at_unix: 1,
        updated_at_unix: 2,
    });

    assert!(entry.watchable);
    assert!(!entry.active);
    assert_eq!(entry.status, "notLoaded");
}

#[test]
fn portal_keyboard_uses_watch_callbacks() {
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
fn portal_keyboard_offers_watch_for_inactive_threads() {
    let entries = vec![AppThreadPortalEntry {
        thread_id: "thread-idle".to_string(),
        title: Some("Idle thread".to_string()),
        preview: String::new(),
        cwd: "/Users/yupeit/dev/cx".to_string(),
        active: false,
        watchable: false,
        status: "idle".to_string(),
    }];

    let keyboard = portal_keyboard(&entries);

    assert_eq!(keyboard.inline_keyboard[0].len(), 1);
    assert_eq!(
        keyboard.inline_keyboard[0][0].callback_data,
        "cx:o:thread-idle"
    );
    assert!(keyboard.inline_keyboard[0][0].text.starts_with("Watch:"));
}

#[test]
fn portal_keyboard_offers_watch_for_watchable_tui_threads() {
    let entries = vec![AppThreadPortalEntry {
        thread_id: "thread-tui-active".to_string(),
        title: Some("Running in TUI".to_string()),
        preview: String::new(),
        cwd: "/Users/yupeit/dev/cx".to_string(),
        active: false,
        watchable: true,
        status: "active-tui".to_string(),
    }];

    let keyboard = portal_keyboard(&entries);

    assert_eq!(keyboard.inline_keyboard[0].len(), 1);
    assert!(keyboard.inline_keyboard[0][0].text.starts_with("Watch:"));
    assert_eq!(
        keyboard.inline_keyboard[0][0].callback_data,
        "cx:o:thread-tui-active"
    );
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
fn filter_portal_entries_filters_local_watch_regression_threads() {
    let entries = vec![
        AppThreadPortalEntry {
            thread_id: "thread-test".to_string(),
            title: Some("For Telegram watch E2E marker CXE2E".to_string()),
            preview: String::new(),
            cwd: "/Users/yupeit/dev/cx".to_string(),
            active: false,
            watchable: true,
            status: "open-tui".to_string(),
        },
        AppThreadPortalEntry {
            thread_id: "thread-real".to_string(),
            title: Some("Fix Telegram close semantics".to_string()),
            preview: String::new(),
            cwd: "/Users/yupeit/dev/cx".to_string(),
            active: true,
            watchable: true,
            status: "active".to_string(),
        },
    ];

    let filtered = filter_portal_entries(entries, 10);

    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].thread_id, "thread-real");
}

#[test]
fn list_portal_entries_reads_threads_from_all_slot_state_dbs() {
    let paths = temp_paths("portal-all-slots");
    let deepseek_home = paths.slot_home("deepseek");
    fs::create_dir_all(&deepseek_home).unwrap();
    let rollout = deepseek_home.join("sessions/rollout-deepseek.jsonl");
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    fs::write(&rollout, "").unwrap();
    let conn = Connection::open(deepseek_home.join("state_5.sqlite")).unwrap();
    conn.execute(
        "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    title TEXT,
                    first_user_message TEXT,
                    cwd TEXT,
                    updated_at INTEGER NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0
                )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO threads (
                    id,
                    rollout_path,
                    title,
                    first_user_message,
                    cwd,
                    updated_at,
                    archived
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
        params![
            "thread-deepseek",
            rollout.to_string_lossy().as_ref(),
            "DeepSeek slot thread",
            "DeepSeek slot thread",
            "/Users/yupeit",
            42_i64
        ],
    )
    .unwrap();

    let entries = list_portal_entries(&paths, 8, false, "test").unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].thread_id, "thread-deepseek");
    assert_eq!(entries[0].status, "notLoaded");
    assert!(entries[0].watchable);

    let _ = fs::remove_dir_all(paths.manager_dir);
    let _ = fs::remove_dir_all(paths.base_codex_home);
}

#[test]
fn loopback_listen_urls_accepts_only_loopback_addresses() {
    let urls = loopback_listen_urls(
                "p1\nn127.0.0.1:1234\nnlocalhost:4567\nn[::1]:2345\nn0.0.0.0:9999\nn127.0.0.1:not-a-port\nn127.0.0.1:1234\n",
            );

    assert_eq!(
        urls,
        vec![
            "ws://127.0.0.1:1234",
            "ws://127.0.0.1:2345",
            "ws://127.0.0.1:4567",
        ]
    );
}

#[test]
fn rollout_task_events_track_active_turn_lifecycle() {
    let mut active_turns = Vec::<(String, u64)>::new();
    apply_rollout_task_event(
        &mut active_turns,
        rollout_task_event(
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
        ),
        42,
    );
    apply_rollout_task_event(
        &mut active_turns,
        rollout_task_event(
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"working"}}"#,
        ),
        84,
    );
    assert_eq!(active_turns, vec![("turn-1".to_string(), 42)]);

    apply_rollout_task_event(
        &mut active_turns,
        rollout_task_event(
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
        ),
        126,
    );
    assert!(active_turns.is_empty());
}

#[test]
fn rollout_task_events_accept_turn_aliases_and_abort() {
    let mut active_turns = Vec::<(String, u64)>::new();
    apply_rollout_task_event(
        &mut active_turns,
        rollout_task_event(
            r#"{"type":"event_msg","payload":{"type":"turn_started","turn_id":"turn-2"}}"#,
        ),
        7,
    );
    assert_eq!(active_turns, vec![("turn-2".to_string(), 7)]);

    apply_rollout_task_event(
        &mut active_turns,
        rollout_task_event(
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"turn-2","reason":"interrupted"}}"#,
        ),
        8,
    );
    assert!(active_turns.is_empty());
}

#[test]
fn rollout_task_events_accept_turn_context_start() {
    assert_eq!(
        rollout_task_event(
            r#"{"type":"turn_context","payload":{"turn_id":"turn-context-1","cwd":"/tmp"}}"#,
        ),
        Some(RolloutTaskEvent::Started("turn-context-1".to_string()))
    );
}

#[test]
fn latest_active_rollout_turn_reads_recent_unclosed_turn_from_tail() {
    let paths = temp_paths("latest-active-tail");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let path = paths.base_codex_home.join("rollout.jsonl");
    let mut content = Vec::new();

    push_rollout_line(
        &mut content,
        r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
    );
    push_rollout_line(
        &mut content,
        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
    );
    let turn_2_offset = push_rollout_line(
        &mut content,
        r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}"#,
    );
    push_rollout_line(
        &mut content,
        r#"{"type":"event_msg","payload":{"type":"agent_message","message":"still working"}}"#,
    );
    fs::write(&path, content).unwrap();

    assert_eq!(
        latest_active_rollout_turn(&path).unwrap(),
        Some(("turn-2".to_string(), turn_2_offset))
    );
}

#[test]
fn latest_active_rollout_turn_ignores_closed_turn_from_tail() {
    let paths = temp_paths("latest-active-closed-tail");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let path = paths.base_codex_home.join("rollout.jsonl");
    let mut content = Vec::new();

    push_rollout_line(
        &mut content,
        r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
    );
    push_rollout_line(
        &mut content,
        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
    );
    fs::write(&path, content).unwrap();

    assert_eq!(latest_active_rollout_turn(&path).unwrap(), None);
}

#[test]
fn latest_active_rollout_turn_skips_large_non_lifecycle_lines() {
    let paths = temp_paths("latest-active-large-line");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let path = paths.base_codex_home.join("rollout.jsonl");
    let mut content = Vec::new();

    let turn_offset = push_rollout_line(
        &mut content,
        r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-large"}}"#,
    );
    content.extend(std::iter::repeat_n(b'x', 2_000_000));
    content.push(b'\n');
    fs::write(&path, content).unwrap();

    assert_eq!(
        latest_active_rollout_turn(&path).unwrap(),
        Some(("turn-large".to_string(), turn_offset))
    );
}

fn push_rollout_line(content: &mut Vec<u8>, line: &str) -> u64 {
    let offset = content.len() as u64;
    content.extend_from_slice(line.as_bytes());
    content.push(b'\n');
    offset
}

#[test]
fn rollout_path_for_thread_keeps_archived_active_rollouts_visible() {
    let paths = temp_paths("rollout-archived-path");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let rollout = paths.base_codex_home.join("archived-rollout.jsonl");
    let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
    conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0)",
                [],
            )
            .unwrap();
    conn.execute(
        "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 1)",
        params!["thread-archived-active", rollout.to_string_lossy().as_ref()],
    )
    .unwrap();

    assert_eq!(
        rollout_path_for_thread(&paths, "thread-archived-active"),
        Some(rollout)
    );
}

#[test]
fn watch_source_prefers_registered_rollout_thread() {
    let paths = temp_paths("watch-source-rollout");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let rollout = paths.base_codex_home.join("rollout.jsonl");
    let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
    conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0)",
                [],
            )
            .unwrap();
    conn.execute(
        "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 0)",
        params!["thread-rollout", rollout.to_string_lossy().as_ref()],
    )
    .unwrap();

    assert_eq!(
        select_watch_source(&paths, "thread-rollout", None),
        TelegramWatchSource::Rollout
    );
}

#[test]
fn watch_source_upgrades_proxy_when_rollout_registration_appears() {
    let paths = temp_paths("watch-source-upgrade");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let rollout = paths.base_codex_home.join("rollout.jsonl");
    let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
    conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0)",
                [],
            )
            .unwrap();
    conn.execute(
        "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 0)",
        params!["thread-upgrade", rollout.to_string_lossy().as_ref()],
    )
    .unwrap();

    assert_eq!(
        select_watch_source(&paths, "thread-upgrade", Some(TelegramWatchSource::Proxy)),
        TelegramWatchSource::Rollout
    );
}

#[test]
fn watch_source_uses_proxy_without_rollout_transcript() {
    let paths = temp_paths("watch-source-proxy");

    assert_eq!(
        select_watch_source(&paths, "thread-proxy", None),
        TelegramWatchSource::Proxy
    );
}

#[test]
fn watch_source_keeps_persisted_rollout_when_registration_is_temporarily_missing() {
    let paths = temp_paths("watch-source-sticky-rollout");

    assert_eq!(
        select_watch_source(&paths, "thread-rollout", Some(TelegramWatchSource::Rollout)),
        TelegramWatchSource::Rollout
    );
}

#[test]
fn open_tui_portal_entries_keeps_recent_archived_rollouts_visible() {
    let paths = temp_paths("rollout-archived-idle");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let rollout = paths.base_codex_home.join("archived-idle-rollout.jsonl");
    fs::write(
        &rollout,
        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
    )
    .unwrap();
    let _held = fs::File::open(&rollout).unwrap();
    let Some(pid) = pid_holding_file(&rollout) else {
        let _ = fs::remove_dir_all(paths.base_codex_home);
        return;
    };
    if !pid_listening_urls(pid).is_empty() {
        let _ = fs::remove_dir_all(paths.base_codex_home);
        return;
    }
    let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
    conn.execute(
        "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    title TEXT NOT NULL,
                    first_user_message TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    updated_at INTEGER NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0
                )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO threads (
                    id,
                    rollout_path,
                    title,
                    first_user_message,
                    cwd,
                    updated_at,
                    archived
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            "thread-archived-idle",
            rollout.to_string_lossy().as_ref(),
            "Archived idle",
            "Archived idle",
            "/tmp",
            1_i64,
            1_i64
        ],
    )
    .unwrap();

    let entries = open_tui_portal_entries(&paths, 10).unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].thread_id, "thread-archived-idle");
    assert_eq!(entries[0].status, "notLoaded");
    let _ = fs::remove_dir_all(paths.base_codex_home);
}

#[test]
fn active_rollout_turn_requires_active_turn_proof_from_listener_holder() {
    let paths = temp_paths("rollout-listener-holder");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let rollout = paths.base_codex_home.join("listener-holder-rollout.jsonl");
    fs::write(
        &rollout,
        r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
    )
    .unwrap();
    let _held = fs::File::open(&rollout).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let accept_thread = std::thread::spawn(move || {
        let _ = listener.accept();
    });
    let Some(pid) = pid_holding_file(&rollout) else {
        let _ = fs::remove_dir_all(paths.base_codex_home);
        return;
    };
    if pid_listening_urls(pid).is_empty() {
        let _ = fs::remove_dir_all(paths.base_codex_home);
        return;
    }
    let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
    conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0)",
                [],
            )
            .unwrap();
    conn.execute(
        "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 0)",
        params!["thread-listener-holder", rollout.to_string_lossy().as_ref()],
    )
    .unwrap();

    let active = active_rollout_turn(&paths, "thread-listener-holder").unwrap();

    assert!(active.is_none());
    let _ = accept_thread.join();
    let _ = fs::remove_dir_all(paths.base_codex_home);
}

#[test]
fn open_tui_portal_entries_keeps_active_archived_rollouts_visible_without_probe() {
    let paths = temp_paths("rollout-archived-active");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let rollout = paths.base_codex_home.join("archived-active-rollout.jsonl");
    fs::write(
        &rollout,
        r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
    )
    .unwrap();
    let _held = fs::File::open(&rollout).unwrap();
    let Some(pid) = pid_holding_file(&rollout) else {
        let _ = fs::remove_dir_all(paths.base_codex_home);
        return;
    };
    if !pid_listening_urls(pid).is_empty() {
        let _ = fs::remove_dir_all(paths.base_codex_home);
        return;
    }
    let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
    conn.execute(
        "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    title TEXT NOT NULL,
                    first_user_message TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    updated_at INTEGER NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0
                )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO threads (
                    id,
                    rollout_path,
                    title,
                    first_user_message,
                    cwd,
                    updated_at,
                    archived
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            "thread-archived-active",
            rollout.to_string_lossy().as_ref(),
            "Archived active",
            "Archived active",
            "/tmp",
            1_i64,
            1_i64
        ],
    )
    .unwrap();

    let entries = open_tui_portal_entries(&paths, 10).unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].thread_id, "thread-archived-active");
    assert_eq!(entries[0].status, "notLoaded");
    let _ = fs::remove_dir_all(paths.base_codex_home);
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
    assert!(!text.contains("long command output"));
}

#[test]
fn activity_watch_text_renders_completion_metadata() {
    let mut panel = TelegramActivityPanel::new();
    panel.apply_execution(CommandExecution {
        item_id: "cmd-1".to_string(),
        command: "cargo test".to_string(),
        cwd: "/tmp/project".to_string(),
        activity: None,
        status: CommandExecutionStatus::Completed,
        exit_code: Some(0),
        duration_ms: Some(1234),
        aggregated_output: Some("ok\n".to_string()),
    });

    let text = activity_watch_text(&panel);

    assert!(text.contains("• **Ran** `cargo test` (1.2s)"));
    assert!(text.contains("  └ `ok`"));
    assert!(!text.contains("Done:"));
    assert!(!text.contains("exit: 0"));
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
fn activity_watch_text_merges_adjacent_explore_items_like_tui() {
    let mut panel = TelegramActivityPanel::new();
    for (item_id, verb, target) in [
        ("read-skill", "Read", "SKILL.md (rust-systems-style skill)"),
        ("read-telegram", "Read", "telegram.rs"),
        (
            "search-telegram",
            "Search",
            "paused|telegram_paused|AcquireLease in telegram.rs",
        ),
        ("read-client", "Read", "telegram.rs, client.rs"),
    ] {
        panel.apply_execution(CommandExecution {
            item_id: item_id.to_string(),
            command: "tool call".to_string(),
            cwd: "/tmp/project".to_string(),
            activity: Some(CommandActivity {
                verb: verb.to_string(),
                target: target.to_string(),
            }),
            status: CommandExecutionStatus::Completed,
            exit_code: Some(0),
            duration_ms: Some(0),
            aggregated_output: None,
        });
    }

    let text = activity_watch_text(&panel);

    assert_eq!(text.matches("• **Explored**").count(), 1);
    assert_eq!(
            text,
            "• **Explored**\n  └ Read `SKILL.md` (rust-systems-style skill), `telegram.rs`\n    Search `paused|telegram_paused|AcquireLease` in `telegram.rs`\n    Read `telegram.rs`, `client.rs`"
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
fn watch_sink_does_not_move_persisted_status_without_new_tail_content() {
    let target = CapturingTranscriptTarget::default();
    let mut status = TelegramStatusPanel::from_state(None);
    status.start_new_turn();
    assert!(status.flush(&target, true).unwrap());

    let client = Client::new();
    let notifier = TelegramNotifier {
        client: &client,
        token: "test-token",
    };
    let route = TelegramRoute {
        chat_id: 1,
        message_thread_id: None,
    };
    let sink = TelegramWatchSink::new_best_effort(&notifier, route, None, None, status.to_state());

    assert!(!sink.status_needs_tail);
}

#[test]
fn rollout_start_offset_does_not_rewind_after_cursor_exists() {
    assert_eq!(watch_rollout_start_offset(Some(100), 200, Some(10)), 100);
}

#[test]
fn rollout_start_offset_uses_active_turn_without_cursor() {
    assert_eq!(watch_rollout_start_offset(None, 200, Some(10)), 10);
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
fn activity_watch_text_renders_file_changes_like_tui() {
    let mut panel = TelegramActivityPanel::new();
    panel.apply_execution(CommandExecution {
        item_id: "patch-1".to_string(),
        command: "apply patch".to_string(),
        cwd: "/tmp/project".to_string(),
        activity: Some(CommandActivity {
            verb: "Edited".to_string(),
            target: "/tmp/project/src/channel/telegram.rs (+12 -3)".to_string(),
        }),
        status: CommandExecutionStatus::Completed,
        exit_code: None,
        duration_ms: None,
        aggregated_output: None,
    });

    assert_eq!(
        activity_watch_text(&panel),
        "• **Edited** `src/channel/telegram.rs` (+12 -3)"
    );
}

#[test]
fn rollout_observe_event_reads_update_plan_as_activity() {
    let event = rollout_observe_event(
        r#"{"type":"response_item","payload":{"type":"function_call","name":"update_plan","arguments":"{\"explanation\":\"Converging on the TUI transcript model.\",\"plan\":[{\"step\":\"Mirror activity cells\",\"status\":\"completed\"},{\"step\":\"Ship release\",\"status\":\"in_progress\"}]}","call_id":"plan-1"}}"#,
    );

    assert_eq!(
            event,
            Some(RolloutObserveEvent::Stream(AppStreamEvent::CommandCompleted(
                CommandExecution {
                    item_id: "plan-1".to_string(),
                    command: "update_plan".to_string(),
                    cwd: String::new(),
                    activity: Some(CommandActivity {
                        verb: "Plan".to_string(),
                        target: "Converging on the TUI transcript model.\n✔ ~~Mirror activity cells~~\n□ Ship release".to_string(),
                    }),
                    status: CommandExecutionStatus::Completed,
                    exit_code: None,
                    duration_ms: None,
                    aggregated_output: None,
                }
            )))
        );
}

#[test]
fn thinking_watch_text_marks_active_and_done_states() {
    let mut panel = TelegramThinkingPanel::new();
    panel.ensure_active();

    assert_eq!(thinking_watch_text(&panel), "Codex is working\n");

    panel.push("Checking current state.");
    panel.finish();

    assert_eq!(
        thinking_watch_text(&panel),
        "Codex\nChecking current state."
    );
}

#[test]
fn thinking_panel_does_not_send_empty_working_placeholder() {
    let target = CapturingTranscriptTarget::default();
    let mut panel = TelegramThinkingPanel::new();
    panel.ensure_active();
    panel.finish();

    assert!(!panel.flush(&target, true).unwrap());
    assert!(target.sent.borrow().is_empty());
    assert!(target.edited.borrow().is_empty());
}

#[test]
fn rollout_observe_event_reads_context_compaction_as_info_cell() {
    assert_eq!(
        rollout_observe_event(r#"{"type":"compacted","payload":{"message":""}}"#),
        Some(RolloutObserveEvent::Stream(AppStreamEvent::Info(
            "Context compacted".to_string()
        )))
    );
    assert_eq!(
        rollout_observe_event(r#"{"type":"event_msg","payload":{"type":"context_compacted"}}"#,),
        Some(RolloutObserveEvent::Stream(AppStreamEvent::Info(
            "Context compacted".to_string()
        )))
    );
}

#[test]
fn rollout_observe_event_reads_agent_and_terminal_messages() {
    assert_eq!(
        rollout_observe_event(
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"please inspect this"}}"#,
        ),
        Some(RolloutObserveEvent::Stream(AppStreamEvent::UserMessage(
            "please inspect this".to_string()
        )))
    );
    assert_eq!(
        rollout_observe_event(
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"hello","phase":"commentary"}}"#,
        ),
        Some(RolloutObserveEvent::Stream(AppStreamEvent::AgentDelta(
            "hello".to_string()
        )))
    );
    assert_eq!(
        rollout_observe_event(
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"done","phase":"final_answer"}}"#,
        ),
        Some(RolloutObserveEvent::Stream(AppStreamEvent::AgentDelta(
            "done".to_string()
        )))
    );
    assert_eq!(
        rollout_observe_event(
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","duration":{"secs":2,"nanos":500000000},"last_agent_message":"done"}}"#,
        ),
        Some(RolloutObserveEvent::Terminal {
            turn_id: Some("turn-1".to_string()),
            terminal: ObserveTerminal::Completed,
            duration_ms: Some(2500),
            last_agent_message: Some("done".to_string()),
        })
    );
}

#[test]
fn rollout_observe_event_reads_assistant_response_items_as_visible_messages() {
    let event = rollout_observe_event(
        r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"opening note"}],"phase":"commentary"}}"#,
    );

    assert_eq!(
        event,
        Some(RolloutObserveEvent::Stream(AppStreamEvent::AgentDelta(
            "opening note".to_string()
        )))
    );
}

#[test]
fn rollout_agent_message_dedupes_last_message_across_drains() {
    let mut last_sent_agent_message = Some("done".to_string());
    let mut events = Vec::<AppStreamEvent>::new();

    send_rollout_agent_message("done", &mut last_sent_agent_message, &mut |event| {
        events.push(event);
        Ok(())
    })
    .unwrap();

    assert!(events.is_empty());
    assert_eq!(last_sent_agent_message, Some("done".to_string()));
}

#[test]
fn rollout_observe_event_reads_reasoning_summary() {
    let event = rollout_observe_event(
        r#"{"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"Checking the weather API"}],"content":null,"encrypted_content":null}}"#,
    );

    assert_eq!(
        event,
        Some(RolloutObserveEvent::Stream(AppStreamEvent::ReasoningDelta(
            "Checking the weather API".to_string()
        )))
    );
}

#[test]
fn rollout_observe_event_reads_command_execution_events() {
    let started = rollout_observe_event(
        r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"echo hi\",\"workdir\":\"/tmp\"}","call_id":"call-1"}}"#,
    );
    assert_eq!(
        started,
        Some(RolloutObserveEvent::Stream(AppStreamEvent::CommandStarted(
            CommandExecution {
                item_id: "call-1".to_string(),
                command: "echo hi".to_string(),
                cwd: "/tmp".to_string(),
                activity: None,
                status: CommandExecutionStatus::InProgress,
                exit_code: None,
                duration_ms: None,
                aggregated_output: None,
            }
        )))
    );

    let output = rollout_observe_event(concat!(
        r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"#,
        r#""Chunk ID: abc\nOutput:\nhi\n"}}"#
    ));
    assert_eq!(
        output,
        Some(RolloutObserveEvent::Stream(
            AppStreamEvent::CommandOutputDelta {
                item_id: "call-1".to_string(),
                delta: "hi\n".to_string(),
            }
        ))
    );

    let completed = rollout_observe_event(
        r#"{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"call-1","turn_id":"turn-1","command":["/bin/zsh","-lc","sed -n '1,20p' src/channel.rs"],"cwd":"/tmp","parsed_cmd":[{"type":"read","cmd":"sed -n '1,20p' src/channel.rs","name":"channel.rs","path":"src/channel.rs"}],"exit_code":0,"duration":{"secs":1,"nanos":250000000},"status":"completed"}}"#,
    );
    assert_eq!(
        completed,
        Some(RolloutObserveEvent::Stream(
            AppStreamEvent::CommandCompleted(CommandExecution {
                item_id: "call-1".to_string(),
                command: "sed -n '1,20p' src/channel.rs".to_string(),
                cwd: "/tmp".to_string(),
                activity: Some(CommandActivity {
                    verb: "Read".to_string(),
                    target: "channel.rs".to_string(),
                }),
                status: CommandExecutionStatus::Completed,
                exit_code: Some(0),
                duration_ms: Some(1250),
                aggregated_output: None,
            })
        ))
    );
}

#[test]
fn rollout_observe_event_reads_custom_apply_patch_as_file_change() {
    let started = rollout_observe_event(
        r#"{"type":"response_item","payload":{"type":"custom_tool_call","status":"completed","call_id":"patch-1","name":"apply_patch","input":"*** Begin Patch\n*** Update File: src/channel/telegram.rs\n@@\n-old\n+new\n+another\n*** End Patch"}}"#,
    );

    assert_eq!(
        started,
        Some(RolloutObserveEvent::Stream(AppStreamEvent::CommandStarted(
            CommandExecution {
                item_id: "patch-1".to_string(),
                command: "apply_patch".to_string(),
                cwd: String::new(),
                activity: Some(CommandActivity {
                    verb: "Edited".to_string(),
                    target: "src/channel/telegram.rs (+2 -1)".to_string(),
                }),
                status: CommandExecutionStatus::InProgress,
                exit_code: None,
                duration_ms: None,
                aggregated_output: None,
            }
        )))
    );

    let output = rollout_observe_event(
        r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"patch-1","output":"{\"output\":\"Success. Updated the following files:\\nM src/channel/telegram.rs\\n\",\"metadata\":{\"exit_code\":0}}"}}"#,
    );

    assert_eq!(
        output,
        Some(RolloutObserveEvent::Stream(
            AppStreamEvent::CommandOutputDelta {
                item_id: "patch-1".to_string(),
                delta: "Success. Updated the following files:\nM src/channel/telegram.rs\n"
                    .to_string(),
            }
        ))
    );
}

#[test]
fn rollout_observe_event_reads_patch_apply_end_as_file_change() {
    let completed = rollout_observe_event(
        r#"{"type":"event_msg","payload":{"type":"patch_apply_end","call_id":"patch-1","turn_id":"turn-1","stdout":"Success. Updated the following files:\nM src/channel/telegram.rs\n","stderr":"","success":true,"changes":{"src/channel/telegram.rs":{"type":"update","unified_diff":"@@\n-old\n+new\n+another\n","move_path":null}},"status":"completed"}}"#,
    );

    assert_eq!(
        completed,
        Some(RolloutObserveEvent::Stream(
            AppStreamEvent::CommandCompleted(CommandExecution {
                item_id: "patch-1".to_string(),
                command: "apply_patch".to_string(),
                cwd: String::new(),
                activity: Some(CommandActivity {
                    verb: "Edited".to_string(),
                    target: "src/channel/telegram.rs (+2 -1)".to_string(),
                }),
                status: CommandExecutionStatus::Completed,
                exit_code: None,
                duration_ms: None,
                aggregated_output: Some(
                    "Success. Updated the following files:\nM src/channel/telegram.rs\n"
                        .to_string()
                ),
            })
        ))
    );
}

#[test]
fn rollout_command_activity_summarizes_multiple_parsed_commands() {
    let completed = rollout_observe_event(
        r#"{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"call-1","turn_id":"turn-1","command":["/bin/zsh","-lc","sed -n '1,80p' wiley.py && sed -n '1,60p' science.py && sed -n '1,60p' pnas.py"],"cwd":"/tmp","parsed_cmd":[{"type":"read","cmd":"sed -n '1,80p' wiley.py","name":"wiley.py","path":"/tmp/wiley.py"},{"type":"read","cmd":"sed -n '1,60p' science.py","name":"science.py","path":"/tmp/science.py"},{"type":"read","cmd":"sed -n '1,60p' pnas.py","name":"pnas.py","path":"/tmp/pnas.py"}],"exit_code":0,"duration":{"secs":0,"nanos":0},"status":"completed"}}"#,
    );

    assert_eq!(
        completed,
        Some(RolloutObserveEvent::Stream(
            AppStreamEvent::CommandCompleted(CommandExecution {
                item_id: "call-1".to_string(),
                command:
                    "sed -n '1,80p' wiley.py && sed -n '1,60p' science.py && sed -n '1,60p' pnas.py"
                        .to_string(),
                cwd: "/tmp".to_string(),
                activity: Some(CommandActivity {
                    verb: "Read".to_string(),
                    target: "wiley.py, science.py, pnas.py".to_string(),
                }),
                status: CommandExecutionStatus::Completed,
                exit_code: Some(0),
                duration_ms: Some(0),
                aggregated_output: None,
            })
        ))
    );
}

#[test]
fn rollout_command_activity_preserves_search_query() {
    let completed = rollout_observe_event(
        r#"{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"call-search","turn_id":"turn-1","command":["/bin/zsh","-lc","rg -n \"paused|telegram_paused|AcquireLease\" src/channel/telegram.rs"],"cwd":"/tmp","parsed_cmd":[{"type":"search","cmd":"rg -n \"paused|telegram_paused|AcquireLease\" src/channel/telegram.rs","query":"paused|telegram_paused|AcquireLease","path":"src/channel/telegram.rs"}],"exit_code":0,"duration":{"secs":0,"nanos":0},"status":"completed"}}"#,
    );

    assert_eq!(
        completed,
        Some(RolloutObserveEvent::Stream(
            AppStreamEvent::CommandCompleted(CommandExecution {
                item_id: "call-search".to_string(),
                command: "rg -n \"paused|telegram_paused|AcquireLease\" src/channel/telegram.rs"
                    .to_string(),
                cwd: "/tmp".to_string(),
                activity: Some(CommandActivity {
                    verb: "Search".to_string(),
                    target: "paused|telegram_paused|AcquireLease in telegram.rs".to_string(),
                }),
                status: CommandExecutionStatus::Completed,
                exit_code: Some(0),
                duration_ms: Some(0),
                aggregated_output: None,
            })
        ))
    );
}

#[test]
fn rollout_command_activity_preserves_skill_read_label() {
    let completed = rollout_observe_event(
        r#"{"type":"event_msg","payload":{"type":"exec_command_end","call_id":"call-skill","turn_id":"turn-1","command":["/bin/zsh","-lc","sed -n '1,80p' /Users/yupeit/dev/skills/skills/rust-systems-style/SKILL.md"],"cwd":"/tmp","parsed_cmd":[{"type":"read","cmd":"sed -n '1,80p' /Users/yupeit/dev/skills/skills/rust-systems-style/SKILL.md","name":"SKILL.md","path":"/Users/yupeit/dev/skills/skills/rust-systems-style/SKILL.md"}],"exit_code":0,"duration":{"secs":0,"nanos":0},"status":"completed"}}"#,
    );

    assert_eq!(
        completed,
        Some(RolloutObserveEvent::Stream(
            AppStreamEvent::CommandCompleted(CommandExecution {
                item_id: "call-skill".to_string(),
                command:
                    "sed -n '1,80p' /Users/yupeit/dev/skills/skills/rust-systems-style/SKILL.md"
                        .to_string(),
                cwd: "/tmp".to_string(),
                activity: Some(CommandActivity {
                    verb: "Read".to_string(),
                    target: "SKILL.md (rust-systems-style skill)".to_string(),
                }),
                status: CommandExecutionStatus::Completed,
                exit_code: Some(0),
                duration_ms: Some(0),
                aggregated_output: None,
            })
        ))
    );
}

#[test]
fn proxy_log_observe_event_reads_client_user_message() {
    let event = proxy_log_observe_event(
        r#"{"timestampUnixMs":1,"direction":"client_to_server","method":"turn/start","threadId":"thread-1","turnId":null,"message":{"id":7,"method":"turn/start","params":{"threadId":"thread-1","input":[{"type":"text","text":"please inspect this","text_elements":[]}],"summary":"auto"}}}"#,
        "thread-1",
    );

    assert_eq!(
        event,
        Some(RolloutObserveEvent::Stream(AppStreamEvent::UserMessage(
            "please inspect this".to_string()
        )))
    );
}

#[test]
fn proxy_log_approval_observe_event_renders_tui_decision_history() {
    let mut pending = Vec::<TelegramPendingApproval>::new();
    let request = r#"{"timestampUnixMs":1,"connectionId":3,"direction":"server_to_client","method":"item/commandExecution/requestApproval","threadId":"thread-1","turnId":"turn-1","message":{"id":"approval-1","method":"item/commandExecution/requestApproval","params":{"threadId":"thread-1","turnId":"turn-1","command":["/bin/zsh","-lc","git mv src/channel/telegram.rs src/channel/telegram/mod.rs"]}}}"#;
    let response = r#"{"timestampUnixMs":2,"connectionId":3,"direction":"client_to_server","method":null,"threadId":null,"turnId":null,"message":{"id":"approval-1","result":{"decision":"cancel"}}}"#;

    assert_eq!(
        proxy_log_approval_observe_event(request, "thread-1", &mut pending),
        None
    );
    assert_eq!(pending.len(), 1);
    assert_eq!(
        proxy_log_approval_observe_event(response, "thread-1", &mut pending),
        Some(RolloutObserveEvent::Stream(AppStreamEvent::Info(
            "✗ You canceled the request to run git mv src/channel/telegram.rs src/channel/telegram/mod.rs"
                .to_string()
        )))
    );
    assert!(pending.is_empty());
}

#[test]
fn rollout_history_text_reads_recent_user_and_agent_messages() {
    let paths = temp_paths("rollout-history");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let rollout = paths.base_codex_home.join("rollout.jsonl");
    fs::write(
                &rollout,
                concat!(
                    r#"{"type":"event_msg","payload":{"type":"user_message","message":"first question"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"agent_message","message":"first answer","phase":"final_answer"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"user_message","message":"second question"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"agent_message","message":"second answer","phase":"final_answer"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
                    "\n"
                ),
            )
            .unwrap();
    let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
    conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0)",
                [],
            )
            .unwrap();
    conn.execute(
        "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 0)",
        params!["thread-history", rollout.to_string_lossy().as_ref()],
    )
    .unwrap();

    let text = rollout_history_text(&paths, "thread-history", 3)
        .unwrap()
        .unwrap();

    assert!(text.starts_with("Recent thread history:"));
    assert!(!text.contains("first question"));
    assert!(text.contains("Codex: first answer"));
    assert!(text.contains("User: second question"));
    assert!(text.contains("Codex: second answer"));
}

#[test]
fn rollout_history_text_reads_task_complete_last_agent_message() {
    let paths = temp_paths("rollout-history-terminal");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let rollout = paths.base_codex_home.join("rollout.jsonl");
    fs::write(
                &rollout,
                concat!(
                    r#"{"type":"event_msg","payload":{"type":"user_message","message":"question"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","last_agent_message":"final answer from terminal"}}"#,
                    "\n"
                ),
            )
            .unwrap();
    let conn = Connection::open(paths.base_codex_home.join("state_5.sqlite")).unwrap();
    conn.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL DEFAULT 0)",
                [],
            )
            .unwrap();
    conn.execute(
        "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 0)",
        params![
            "thread-history-terminal",
            rollout.to_string_lossy().as_ref()
        ],
    )
    .unwrap();

    let text = rollout_history_text(&paths, "thread-history-terminal", 3)
        .unwrap()
        .unwrap();

    assert!(text.contains("Codex: final answer from terminal"));
}

#[test]
fn rollout_events_since_reads_only_new_complete_lines() {
    let paths = temp_paths("rollout-drain");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let rollout = paths.base_codex_home.join("rollout.jsonl");
    let first = r#"{"type":"event_msg","payload":{"type":"agent_message","message":"old"}}"#;
    let second = r#"{"type":"event_msg","payload":{"type":"agent_message","message":"new"}}"#;
    let partial = r#"{"type":"event_msg","payload":{"type":"agent_message","message":"partial"}"#;
    fs::write(&rollout, format!("{first}\n{second}\n{partial}")).unwrap();

    let drain = rollout_events_since(&rollout, (first.len() + 1) as u64, 10).unwrap();

    assert_eq!(drain.events.len(), 1);
    assert_eq!(
        drain.events[0],
        RolloutObserveEvent::Stream(AppStreamEvent::AgentDelta("new".to_string()))
    );
    assert_eq!(
        drain.next_offset,
        (first.len() + 1 + second.len() + 1) as u64
    );
}

#[test]
fn rollout_history_item_ignores_empty_and_non_message_events() {
    assert_eq!(
        rollout_history_item(
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"  "}}"#,
        ),
        None
    );
    assert_eq!(
        rollout_history_item(
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
        ),
        None
    );
}

#[test]
fn observe_active_rollout_streams_until_task_complete() {
    use std::io::Write as _;

    let paths = temp_paths("rollout-observe");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let rollout = paths.base_codex_home.join("rollout.jsonl");
    fs::write(
        &rollout,
        concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-live"}}"#,
            "\n"
        ),
    )
    .unwrap();

    let writer_rollout = rollout.clone();
    let writer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        let mut file = OpenOptions::new()
            .append(true)
            .open(writer_rollout)
            .unwrap();
        file.write_all(
                    concat!(
                        r#"{"type":"event_msg","payload":{"type":"agent_message","message":"done","phase":"final_answer"}}"#,
                        "\n",
                        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-live","last_agent_message":"done"}}"#,
                        "\n"
                    )
                    .as_bytes(),
                )
                .unwrap();
        file.flush().unwrap();
    });

    let mut messages = Vec::<String>::new();
    let start_offset = fs::metadata(&rollout).unwrap().len();
    let observed = observe_active_rollout(
        &rollout,
        "turn-live",
        start_offset,
        Duration::from_secs(2),
        |event| {
            if let AppStreamEvent::AgentDelta(message) = event {
                messages.push(message);
            }
            Ok(())
        },
    )
    .unwrap();
    writer.join().unwrap();

    assert_eq!(messages, vec!["done".to_string()]);
    assert_eq!(
        observed,
        ObservedRolloutTerminal {
            turn_id: "turn-live".to_string(),
            terminal: ObserveTerminal::Completed,
        }
    );
}

#[test]
fn observe_active_rollout_does_not_skip_events_written_after_scan() {
    use std::io::Write as _;

    let paths = temp_paths("rollout-observe-race");
    fs::create_dir_all(&paths.base_codex_home).unwrap();
    let rollout = paths.base_codex_home.join("rollout.jsonl");
    fs::write(
        &rollout,
        concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-live"}}"#,
            "\n"
        ),
    )
    .unwrap();
    let start_offset = fs::metadata(&rollout).unwrap().len();
    fs::OpenOptions::new()
                .append(true)
                .open(&rollout)
                .unwrap()
                .write_all(
                    concat!(
                        r#"{"type":"event_msg","payload":{"type":"agent_message","message":"done","phase":"final_answer"}}"#,
                        "\n",
                        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-live","last_agent_message":"done"}}"#,
                        "\n"
                    )
                    .as_bytes(),
                )
                .unwrap();

    let mut messages = Vec::<String>::new();
    let observed = observe_active_rollout(
        &rollout,
        "turn-live",
        start_offset,
        Duration::from_secs(2),
        |event| {
            if let AppStreamEvent::AgentDelta(message) = event {
                messages.push(message);
            }
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(messages, vec!["done".to_string()]);
    assert_eq!(observed.terminal, ObserveTerminal::Completed);
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
fn forum_router_route_creates_work_topic_even_when_bound() {
    let paths = temp_paths("router-create-topic");
    let mut state = TelegramState::empty();
    let route = TelegramRoute {
        chat_id: -10042,
        message_thread_id: None,
    };
    state.bind_route(&paths, &route, None).unwrap();

    assert!(should_create_work_topic(&state, &route, true));

    let _ = fs::remove_dir_all(paths.serve_dir());
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
