//! Telegram channel driver.
//!
//! This file coordinates polling, routing, sessions, and high-level commands.
//! Protocol calls, persistent state, rollout observation, transcript rendering,
//! and watch delivery live in sibling modules.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
#[cfg(test)]
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
#[cfg(test)]
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::blocking::Client;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use serde_json::json;
use serde_json::Value;

use crate::app_server::AppServerClient;
use crate::app_server::AppStreamEvent;
use crate::app_server::AppThreadSummary;
use crate::app_server::ApprovalRequest;
#[cfg(test)]
use crate::app_server::CommandActivity;
#[cfg(test)]
use crate::app_server::CommandExecution;
#[cfg(test)]
use crate::app_server::CommandExecutionStatus;
use crate::cli::TelegramBindArgs;
use crate::cli::TelegramMenuArgs;
use crate::cli::TelegramRunArgs;
use crate::cli::TelegramStatusArgs;
use crate::paths::ManagerPaths;
use crate::serve;
use crate::session;
use crate::session::AcquireLeaseRequest;
#[cfg(test)]
use crate::session::ChannelId;
use crate::session::RecordChannelMessageRequest;
#[cfg(test)]
use crate::session::SessionId;

mod api;
mod delivery;
mod rollout;
mod state;
#[cfg(test)]
mod tests;
mod transcript;
mod watch;

#[cfg(test)]
use self::api::bot_commands;
use self::api::create_forum_topic;
use self::api::delete_forum_topic;
use self::api::edit_general_forum_topic;
use self::api::get_updates;
#[cfg(test)]
use self::api::is_telegram_delete_noop_error;
use self::api::is_telegram_missing_thread_error;
#[cfg(test)]
use self::api::is_telegram_noop_error;
use self::api::sync_bot_commands;
#[cfg(test)]
use self::api::telegram_html_text;
use self::api::unhide_general_forum_topic;
#[cfg(test)]
use self::api::TelegramChat;
#[cfg(test)]
use self::api::TelegramChatMemberUpdated;
#[cfg(test)]
use self::api::TelegramForumTopicEdited;
use self::api::TelegramInlineKeyboardButton;
use self::api::TelegramInlineKeyboardMarkup;
use self::api::TelegramMessage;
#[cfg(test)]
use self::api::TelegramUpdate;
#[cfg(test)]
use self::api::TelegramUpdateSource;
use self::api::TelegramUpdateView;
use self::delivery::deliver_reply;
use self::delivery::panel_reply_with_keyboard;
use self::delivery::reply;
use self::delivery::TelegramNotifier;
use self::delivery::TelegramReply;
#[cfg(test)]
use self::rollout::active_rollout_turn;
#[cfg(test)]
use self::rollout::apply_rollout_task_event;
use self::rollout::codex_state_db_paths;
use self::rollout::latest_active_rollout_turn;
#[cfg(test)]
use self::rollout::loopback_listen_urls;
#[cfg(test)]
use self::rollout::observe_active_rollout;
#[cfg(test)]
use self::rollout::pid_holding_file;
#[cfg(test)]
use self::rollout::pid_listening_urls;
use self::rollout::proxy_events_since;
#[cfg(test)]
use self::rollout::proxy_log_approval_observe_event;
#[cfg(test)]
use self::rollout::proxy_log_observe_event;
use self::rollout::rollout_events_since;
#[cfg(test)]
use self::rollout::rollout_history_item;
use self::rollout::rollout_history_text;
#[cfg(test)]
use self::rollout::rollout_observe_event;
use self::rollout::rollout_path_for_thread;
#[cfg(test)]
use self::rollout::rollout_task_event;
#[cfg(test)]
use self::rollout::send_rollout_agent_message;
#[cfg(test)]
use self::rollout::ObserveTerminal;
#[cfg(test)]
use self::rollout::ObservedRolloutTerminal;
use self::rollout::RolloutObserveEvent;
#[cfg(test)]
use self::rollout::RolloutTaskEvent;
use self::state::read_state;
use self::state::write_state;
use self::state::TelegramBinding;
#[cfg(test)]
use self::state::TelegramPendingApproval;
use self::state::TelegramRoute;
use self::state::TelegramState;
use self::state::TelegramWatchSource;
#[cfg(test)]
use self::transcript::activity_watch_text;
#[cfg(test)]
use self::transcript::thinking_watch_text;
#[cfg(test)]
use self::transcript::TelegramActivityPanel;
#[cfg(test)]
use self::transcript::TelegramStatusPanel;
#[cfg(test)]
use self::transcript::TelegramThinkingPanel;
#[cfg(test)]
use self::transcript::TelegramTranscriptTarget;
use self::transcript::TelegramTurnTerminal;
use self::watch::send_watch_events;
#[cfg(test)]
use self::watch::telegram_retry_after_delay;
use self::watch::TelegramWatchSink;
use self::watch::WatchSendResult;

const PORTAL_TOPIC_TITLE: &str = "cx portal";
const PORTAL_APP_SERVER_TIMEOUT: Duration = Duration::from_millis(750);
#[cfg(test)]
const ROLLOUT_OWNER_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const WATCH_DRAIN_MAX_LINES: usize = 1000;

pub fn run(args: TelegramRunArgs) -> Result<()> {
    validate_timeouts(args.poll_timeout, args.request_timeout)?;
    if args.app_server_timeout <= 0.0 {
        anyhow::bail!("--app-server-timeout must be positive");
    }
    let token = read_token_env(&args.bot_token_env)?;
    let paths = ManagerPaths::new(args.manager_dir)?;
    let mut state = read_state(&paths)?;
    let mut trusted = trusted_chats(&state, args.allow_chats);
    let mut bind_secret = if trusted.is_empty() {
        let secret = generate_bind_secret()?;
        println!("cx telegram onboarding");
        println!("send this message to the Telegram bot:");
        println!("/bind {secret}");
        println!("waiting for first matching chat...");
        Some(secret)
    } else {
        None
    };
    let client = Client::builder()
        .timeout(Duration::from_secs_f32(args.request_timeout))
        .build()
        .context("build Telegram HTTP client")?;
    sync_bot_commands(&client, &token)?;
    refresh_startup_portals(&paths, &client, &token, &mut state, args.app_server_timeout)?;

    loop {
        let outcome = poll_once(
            &paths,
            &client,
            &token,
            &mut state,
            &mut trusted,
            PollOptions {
                bind_secret: bind_secret.as_deref(),
                poll_timeout: args.poll_timeout,
                log_updates: args.log_updates,
                acquire_lease: args.acquire_lease,
                steal: args.steal,
                app_server_timeout: args.app_server_timeout,
                trust_existing: true,
            },
        )?;
        if bind_secret.is_some() && outcome.bound_route.is_some() {
            println!("telegram route bound; continuing adapter run");
            bind_secret = None;
        }
    }
}

pub fn bind(args: TelegramBindArgs) -> Result<()> {
    validate_timeouts(args.poll_timeout, args.request_timeout)?;
    let token = read_token_env(&args.bot_token_env)?;
    let paths = ManagerPaths::new(args.manager_dir)?;
    let mut state = read_state(&paths)?;
    let mut trusted = trusted_chats(&state, Vec::new());
    let secret = generate_bind_secret()?;

    println!("cx telegram bind");
    println!("send this message to the Telegram chat you want to trust:");
    println!("/bind {secret}");
    println!("waiting for matching chat...");

    let client = Client::builder()
        .timeout(Duration::from_secs_f32(args.request_timeout))
        .build()
        .context("build Telegram HTTP client")?;
    sync_bot_commands(&client, &token)?;

    loop {
        let outcome = poll_once(
            &paths,
            &client,
            &token,
            &mut state,
            &mut trusted,
            PollOptions {
                bind_secret: Some(&secret),
                poll_timeout: args.poll_timeout,
                log_updates: args.log_updates,
                acquire_lease: false,
                steal: false,
                app_server_timeout: 0.0,
                trust_existing: false,
            },
        )?;
        if let Some(route) = outcome.bound_route {
            println!("trusted Telegram route {}", route.display());
            return Ok(());
        }
    }
}

pub fn menu(args: TelegramMenuArgs) -> Result<()> {
    if args.request_timeout <= 0.0 {
        anyhow::bail!("--request-timeout must be positive");
    }
    let token = read_token_env(&args.bot_token_env)?;
    let client = Client::builder()
        .timeout(Duration::from_secs_f32(args.request_timeout))
        .build()
        .context("build Telegram HTTP client")?;
    sync_bot_commands(&client, &token)?;
    println!("telegram command menu synced");
    Ok(())
}

pub fn status(args: TelegramStatusArgs) -> Result<()> {
    let paths = ManagerPaths::new(args.manager_dir)?;
    let state = read_state(&paths)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&state)?);
        return Ok(());
    }
    println!("cx telegram channel");
    println!("state: {}", paths.telegram_channel_state_file().display());
    match state.last_update_id {
        Some(update_id) => println!("last_update_id: {update_id}"),
        None => println!("last_update_id: <none>"),
    }
    if state.bindings.is_empty() {
        println!("bindings: 0");
    } else {
        for binding in state.bindings {
            let app_thread = binding.app_thread_id.as_deref().unwrap_or("<none>");
            let handoff = if binding.telegram_paused {
                "paused"
            } else {
                "active"
            };
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                binding.chat_id,
                binding.message_thread_id.unwrap_or_default(),
                binding.alias.as_deref().unwrap_or("default"),
                binding.channel_id,
                binding.session_id,
                app_thread,
                handoff
            );
        }
    }
    if state.trusted_routes.is_empty() {
        println!("trusted_routes: 0");
    } else {
        for route in state.trusted_routes {
            println!("trusted\t{}", route.display());
        }
    }
    Ok(())
}

struct PollOutcome {
    bound_route: Option<TelegramRoute>,
}

struct PollOptions<'a> {
    bind_secret: Option<&'a str>,
    poll_timeout: u64,
    log_updates: bool,
    acquire_lease: bool,
    steal: bool,
    app_server_timeout: f32,
    trust_existing: bool,
}

fn poll_once(
    paths: &ManagerPaths,
    client: &Client,
    token: &str,
    state: &mut TelegramState,
    trusted: &mut BTreeSet<TelegramRoute>,
    options: PollOptions<'_>,
) -> Result<PollOutcome> {
    let mut bound_route = None;
    let poll_timeout = if options.app_server_timeout > 0.0 && has_watched_bindings(state) {
        options.poll_timeout.min(2)
    } else {
        options.poll_timeout
    };
    let offset = state.last_update_id.map(|id| id + 1);
    let poll_start = Instant::now();
    let updates = get_updates(client, token, offset, poll_timeout)?;
    if options.log_updates {
        eprintln!(
                "telegram timing phase=poll offset={offset:?} timeout_s={poll_timeout} updates={} elapsed_ms={}",
                updates.len(),
                elapsed_ms(poll_start)
            );
    }
    for update in updates {
        let update_start = Instant::now();
        state.last_update_id = Some(
            state
                .last_update_id
                .map_or(update.update_id, |id| id.max(update.update_id)),
        );
        let update_id = update.update_id;
        let view = update.view();
        if options.log_updates {
            log_update_summary(update_id, &view, trusted);
        }
        let Some(message) = view.message else {
            continue;
        };
        let access = message_access(
            &message,
            state,
            trusted,
            options.bind_secret,
            options.trust_existing,
        );
        if matches!(access, MessageAccess::Ignored) {
            continue;
        }
        if matches!(access, MessageAccess::AuthorizedByBind) {
            let route = trusted_route_for_message(&message);
            state.trust_route(&route);
            trusted.insert(route.clone());
            bound_route = Some(route);
        }
        if options.log_updates {
            let route = TelegramRoute::from_message(&message);
            eprintln!(
                    "telegram timing update_id={update_id} phase=received source={} route={} message_id={} telegram_age_ms={} elapsed_ms={}",
                    view.source.as_str(),
                    route.display(),
                    message.message_id,
                    telegram_age_ms(message.date).map_or_else(|| String::from("<unknown>"), |age| age.to_string()),
                    elapsed_ms(update_start)
                );
        }
        let callback_message_id = view.callback_query_id.as_ref().map(|_| message.message_id);
        let notifier = TelegramNotifier { client, token };
        let ack_start = Instant::now();
        if let Some(callback_query_id) = view.callback_query_id.as_deref() {
            notifier.answer_callback_query(callback_query_id, None);
        } else {
            notifier.ack_seen(&message);
        }
        if options.log_updates {
            eprintln!(
                "telegram timing update_id={update_id} phase=ack_seen elapsed_ms={}",
                elapsed_ms(ack_start)
            );
        }
        let handle_start = Instant::now();
        let reply = handle_message(
            paths,
            state,
            message,
            HandleOptions {
                notifier: Some(&notifier),
                acquire_lease: options.acquire_lease,
                steal: options.steal,
                authorized_by_bind: matches!(access, MessageAccess::AuthorizedByBind),
                app_server_timeout: options.app_server_timeout,
                trace_timings: options.log_updates,
                callback_data: view.callback_data.as_deref(),
                callback_message_id,
            },
        )?;
        if options.log_updates {
            eprintln!(
                "telegram timing update_id={update_id} phase=handle_message reply={} elapsed_ms={}",
                reply.is_some(),
                elapsed_ms(handle_start)
            );
        }
        write_state(paths, state)?;
        if let Some(reply) = reply {
            let route = reply.route();
            let deliver_start = Instant::now();
            match deliver_reply(client, token, &reply) {
                Ok(panel_message_id) => {
                    if options.log_updates {
                        eprintln!(
                                "telegram timing update_id={update_id} phase=deliver_reply route={} message_id={panel_message_id:?} elapsed_ms={}",
                                route.display(),
                                elapsed_ms(deliver_start)
                            );
                    }
                    if reply.remember_panel_message {
                        if let Some(message_id) = panel_message_id {
                            state.remember_panel_message(&route, message_id);
                            write_state(paths, state)?;
                        }
                    }
                }
                Err(err) => {
                    if options.log_updates {
                        eprintln!(
                                "telegram timing update_id={update_id} phase=deliver_reply route={} error=true elapsed_ms={}",
                                route.display(),
                                elapsed_ms(deliver_start)
                            );
                    }
                    eprintln!(
                        "telegram reply delivery failed for {}: {err:#}",
                        route.display()
                    );
                }
            }
        }
        if options.log_updates {
            eprintln!(
                "telegram timing update_id={update_id} phase=update_complete elapsed_ms={}",
                elapsed_ms(update_start)
            );
        }
    }
    if options.app_server_timeout > 0.0 {
        let observe_start = Instant::now();
        observe_watched_bindings(paths, client, token, state, options.app_server_timeout)?;
        if options.log_updates && has_watched_bindings(state) {
            eprintln!(
                "telegram timing phase=observe_watches elapsed_ms={}",
                elapsed_ms(observe_start)
            );
        }
    }
    write_state(paths, state)?;
    Ok(PollOutcome { bound_route })
}

fn has_watched_bindings(state: &TelegramState) -> bool {
    state
        .bindings
        .iter()
        .any(|binding| binding.telegram_paused && binding.app_thread_id.is_some())
}

fn observe_watched_bindings(
    paths: &ManagerPaths,
    client: &Client,
    token: &str,
    state: &mut TelegramState,
    _app_server_timeout: f32,
) -> Result<()> {
    let notifier = TelegramNotifier { client, token };
    for binding in watched_bindings(state) {
        let route = TelegramRoute {
            chat_id: binding.chat_id,
            message_thread_id: binding.message_thread_id,
        };
        match drain_watched_binding(paths, state, &route, &binding, &notifier) {
            Ok(()) => {}
            Err(err) if is_telegram_missing_thread_error(&err) => {
                eprintln!(
                    "telegram watch route no longer exists; removing {}: {err:#}",
                    route.display()
                );
                state.remove_route_binding(&route, binding.alias.as_deref());
            }
            Err(err) => {
                eprintln!(
                    "telegram watch observe failed for {}: {err:#}",
                    route.display()
                );
            }
        }
    }
    Ok(())
}

fn drain_watched_binding(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    binding: &TelegramBinding,
    notifier: &TelegramNotifier<'_>,
) -> Result<()> {
    let Some(thread_id) = binding.app_thread_id.as_deref() else {
        return Ok(());
    };
    let source = select_watch_source(paths, thread_id, binding.watch_source);
    if let Some(stored) = state.binding_for_route_mut(route, binding.alias.as_deref()) {
        stored.watch_source = Some(source);
    }

    match source {
        TelegramWatchSource::Proxy => {
            drain_watched_proxy_binding(paths, state, route, binding, notifier, thread_id)
        }
        TelegramWatchSource::Rollout => {
            drain_watched_rollout_binding(paths, state, route, binding, notifier, thread_id)
        }
    }
}

fn drain_watched_proxy_binding(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    binding: &TelegramBinding,
    notifier: &TelegramNotifier<'_>,
    thread_id: &str,
) -> Result<()> {
    let proxy_path = proxy_event_log_path(paths);
    if !proxy_path.exists() {
        return Ok(());
    }
    let proxy_len = fs::metadata(&proxy_path)
        .with_context(|| format!("stat proxy event log {}", proxy_path.display()))?
        .len();
    let proxy_offset = binding
        .watch_proxy_offset
        .filter(|offset| *offset <= proxy_len)
        .unwrap_or(proxy_len);
    let mut pending_approvals = binding.watch_pending_approvals.clone();
    let proxy_drain = proxy_events_since(
        &proxy_path,
        proxy_offset,
        WATCH_DRAIN_MAX_LINES,
        thread_id,
        &mut pending_approvals,
        true,
    )?;
    let send_result =
        send_watch_events_for_binding(route, notifier, proxy_drain.events, state, binding)?;
    if let Some(stored) = state.binding_for_route_mut(route, binding.alias.as_deref()) {
        stored.watch_proxy_offset = Some(proxy_drain.next_offset);
        stored.watch_activity = send_result.activity;
        stored.watch_thinking = send_result.thinking;
        stored.watch_status = send_result.status;
        stored.watch_last_agent_message = send_result.last_agent_message;
        stored.watch_pending_approvals = pending_approvals;
    }
    Ok(())
}

fn drain_watched_rollout_binding(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    binding: &TelegramBinding,
    notifier: &TelegramNotifier<'_>,
    thread_id: &str,
) -> Result<()> {
    let Some(path) = rollout_path_for_thread(paths, thread_id) else {
        return Ok(());
    };
    if !path.exists() {
        return Ok(());
    }

    let file_len = fs::metadata(&path)
        .with_context(|| format!("stat rollout {}", path.display()))?
        .len();
    let active_turn_offset = latest_active_rollout_turn(&path)
        .ok()
        .flatten()
        .map(|(_, offset)| offset);
    let start_offset =
        watch_rollout_start_offset(binding.watch_rollout_offset, file_len, active_turn_offset);
    let drain = rollout_events_since(&path, start_offset, WATCH_DRAIN_MAX_LINES)?;
    let mut events = drain.events;
    let mut pending_approvals = binding.watch_pending_approvals.clone();
    let mut next_proxy_offset = binding.watch_proxy_offset;
    let proxy_path = proxy_event_log_path(paths);
    if proxy_path.exists() {
        let proxy_len = fs::metadata(&proxy_path)
            .with_context(|| format!("stat proxy event log {}", proxy_path.display()))?
            .len();
        let proxy_offset = binding
            .watch_proxy_offset
            .filter(|offset| *offset <= proxy_len)
            .unwrap_or(proxy_len);
        let proxy_drain = proxy_events_since(
            &proxy_path,
            proxy_offset,
            WATCH_DRAIN_MAX_LINES,
            thread_id,
            &mut pending_approvals,
            false,
        )?;
        next_proxy_offset = Some(proxy_drain.next_offset);
        events.extend(proxy_drain.events);
    }
    let send_result = send_watch_events_for_binding(route, notifier, events, state, binding)?;
    if let Some(stored) = state.binding_for_route_mut(route, binding.alias.as_deref()) {
        stored.watch_rollout_offset = Some(drain.next_offset);
        stored.watch_proxy_offset = next_proxy_offset;
        stored.watch_activity = send_result.activity;
        stored.watch_thinking = send_result.thinking;
        stored.watch_status = send_result.status;
        stored.watch_last_agent_message = send_result.last_agent_message;
        stored.watch_pending_approvals = pending_approvals;
    }
    Ok(())
}

fn send_watch_events_for_binding(
    route: &TelegramRoute,
    notifier: &TelegramNotifier<'_>,
    events: Vec<RolloutObserveEvent>,
    state: &TelegramState,
    binding: &TelegramBinding,
) -> Result<WatchSendResult> {
    let stored = state.binding_for_route(route, binding.alias.as_deref());
    let default_cwd = stored
        .and_then(|stored| stored.app_thread_cwd.as_deref())
        .or(binding.app_thread_cwd.as_deref());
    let events = events_with_default_cwd(events, default_cwd);
    let activity_state = stored
        .and_then(|stored| stored.watch_activity.clone())
        .or_else(|| binding.watch_activity.clone());
    let status_state = stored
        .and_then(|stored| stored.watch_status.clone())
        .or_else(|| binding.watch_status.clone());
    let thinking_state = stored
        .and_then(|stored| stored.watch_thinking.clone())
        .or_else(|| binding.watch_thinking.clone());
    let last_agent_message = stored
        .and_then(|stored| stored.watch_last_agent_message.clone())
        .or_else(|| binding.watch_last_agent_message.clone());
    send_watch_events(
        route,
        notifier,
        events,
        activity_state,
        thinking_state,
        status_state,
        last_agent_message,
    )
}

fn events_with_default_cwd(
    events: Vec<RolloutObserveEvent>,
    default_cwd: Option<&str>,
) -> Vec<RolloutObserveEvent> {
    let Some(default_cwd) = default_cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) else {
        return events;
    };
    events
        .into_iter()
        .map(|event| match event {
            RolloutObserveEvent::Stream(event) => {
                RolloutObserveEvent::Stream(stream_event_with_default_cwd(event, default_cwd))
            }
            event => event,
        })
        .collect()
}

fn stream_event_with_default_cwd(event: AppStreamEvent, default_cwd: &str) -> AppStreamEvent {
    match event {
        AppStreamEvent::CommandStarted(mut command) => {
            fill_default_command_cwd(&mut command.cwd, default_cwd);
            AppStreamEvent::CommandStarted(command)
        }
        AppStreamEvent::CommandCompleted(mut command) => {
            fill_default_command_cwd(&mut command.cwd, default_cwd);
            AppStreamEvent::CommandCompleted(command)
        }
        event => event,
    }
}

fn fill_default_command_cwd(cwd: &mut String, default_cwd: &str) {
    if command_cwd_is_missing(cwd) {
        *cwd = default_cwd.to_string();
    }
}

fn command_cwd_is_missing(cwd: &str) -> bool {
    matches!(cwd.trim(), "" | "<unknown>")
}

fn select_watch_source(
    paths: &ManagerPaths,
    thread_id: &str,
    stored_source: Option<TelegramWatchSource>,
) -> TelegramWatchSource {
    if rollout_path_for_thread(paths, thread_id).is_some() {
        return TelegramWatchSource::Rollout;
    }
    match stored_source {
        Some(TelegramWatchSource::Rollout) => TelegramWatchSource::Rollout,
        Some(TelegramWatchSource::Proxy) | None => TelegramWatchSource::Proxy,
    }
}

fn watch_rollout_start_offset(
    stored_offset: Option<u64>,
    file_len: u64,
    active_turn_offset: Option<u64>,
) -> u64 {
    match stored_offset {
        Some(offset) if offset <= file_len => offset,
        Some(_) | None => active_turn_offset.unwrap_or(file_len),
    }
}

fn watched_bindings(state: &TelegramState) -> Vec<TelegramBinding> {
    let mut seen = BTreeSet::<(i64, Option<i64>, String)>::new();
    state
        .bindings
        .iter()
        .filter(|binding| binding.telegram_paused)
        .filter_map(|binding| {
            let thread_id = binding.app_thread_id.clone()?;
            let key = (binding.chat_id, binding.message_thread_id, thread_id);
            seen.insert(key).then(|| binding.clone())
        })
        .collect()
}

fn validate_timeouts(poll_timeout: u64, request_timeout: f32) -> Result<()> {
    if poll_timeout == 0 {
        anyhow::bail!("--poll-timeout must be positive");
    }
    if request_timeout <= 0.0 {
        anyhow::bail!("--request-timeout must be positive");
    }
    Ok(())
}

fn refresh_startup_portals(
    paths: &ManagerPaths,
    client: &Client,
    token: &str,
    state: &mut TelegramState,
    app_server_timeout: f32,
) -> Result<()> {
    let mut routes = state
        .bindings
        .iter()
        .filter(|binding| binding.message_thread_id.is_none())
        .map(|binding| TelegramRoute {
            chat_id: binding.chat_id,
            message_thread_id: None,
        })
        .collect::<Vec<_>>();
    routes.sort();
    routes.dedup();

    for route in routes {
        if route.chat_id < 0 {
            if let Err(err) = unhide_general_forum_topic(client, token, route.chat_id) {
                eprintln!(
                    "telegram portal startup unhideGeneralForumTopic failed for {}: {err:#}",
                    route.display()
                );
            }
            if let Err(err) =
                edit_general_forum_topic(client, token, route.chat_id, PORTAL_TOPIC_TITLE)
            {
                eprintln!(
                    "telegram portal startup editGeneralForumTopic failed for {}: {err:#}",
                    route.display()
                );
            }
        }

        let edit_message_id = state.panel_message_id_for_route(&route);
        let reply = portal_reply(paths, &route, app_server_timeout, edit_message_id, false)
            .unwrap_or_else(|err| portal_unavailable_reply(&route, err, edit_message_id));
        match deliver_reply(client, token, &reply) {
            Ok(Some(message_id)) => {
                state.remember_panel_message(&route, message_id);
            }
            Ok(None) => {}
            Err(err) if reply.edit_message_id.is_some() => {
                eprintln!(
                    "telegram portal startup edit panel failed for {}: {err:#}",
                    route.display()
                );
                let fresh_reply = portal_reply(paths, &route, app_server_timeout, None, false)
                    .unwrap_or_else(|err| portal_unavailable_reply(&route, err, None));
                match deliver_reply(client, token, &fresh_reply) {
                    Ok(Some(message_id)) => {
                        state.remember_panel_message(&route, message_id);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        eprintln!(
                            "telegram portal startup fresh panel failed for {}: {err:#}",
                            route.display()
                        );
                    }
                }
            }
            Err(err) => {
                eprintln!(
                    "telegram portal startup send panel failed for {}: {err:#}",
                    route.display()
                );
            }
        }
    }
    write_state(paths, state)?;
    Ok(())
}

fn read_token_env(env_name: &str) -> Result<String> {
    let token = std::env::var(env_name).with_context(|| format!("{env_name} is not set"))?;
    if token.trim().is_empty() {
        anyhow::bail!("{env_name} is empty");
    }
    Ok(token)
}

fn trusted_chats(state: &TelegramState, allow_chats: Vec<i64>) -> BTreeSet<TelegramRoute> {
    state
        .trusted_routes
        .iter()
        .cloned()
        .chain(state.bindings.iter().map(|binding| TelegramRoute {
            chat_id: binding.chat_id,
            message_thread_id: binding.message_thread_id,
        }))
        .chain(allow_chats.into_iter().map(|chat_id| TelegramRoute {
            chat_id,
            message_thread_id: None,
        }))
        .collect()
}

fn trusted_route_for_message(message: &TelegramMessage) -> TelegramRoute {
    let route = TelegramRoute::from_message(message);
    if message.chat.is_forum {
        TelegramRoute {
            chat_id: route.chat_id,
            message_thread_id: None,
        }
    } else {
        route
    }
}

fn generate_bind_secret() -> Result<String> {
    let mut bytes = [0_u8; 16];
    let mut file = fs::File::open("/dev/urandom").context("open /dev/urandom")?;
    file.read_exact(&mut bytes).context("read /dev/urandom")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppThreadPortalEntry {
    thread_id: String,
    title: Option<String>,
    preview: String,
    cwd: String,
    active: bool,
    watchable: bool,
    status: String,
}

impl From<AppThreadSummary> for AppThreadPortalEntry {
    fn from(summary: AppThreadSummary) -> Self {
        Self {
            thread_id: summary.upstream_thread_id,
            title: summary.title,
            preview: summary.preview,
            cwd: summary.cwd,
            active: summary.active,
            watchable: true,
            status: summary.status,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TelegramCallbackCommand {
    RefreshPortal,
    RefreshWork,
    Observe { thread_id: String },
    Takeover,
    Release,
    Close,
}

impl TelegramCallbackCommand {
    fn parse(data: &str) -> Option<Self> {
        match data {
            "cx:p" => Some(Self::RefreshPortal),
            "cx:w" => Some(Self::RefreshWork),
            "cx:t" => Some(Self::Takeover),
            "cx:rel" => Some(Self::Release),
            "cx:c" => Some(Self::Close),
            _ => {
                if let Some(thread_id) = data.strip_prefix("cx:o:") {
                    if thread_id.is_empty() {
                        return None;
                    }
                    return Some(Self::Observe {
                        thread_id: thread_id.to_string(),
                    });
                }
                let thread_id = data.strip_prefix("cx:a:")?;
                if thread_id.is_empty() {
                    return None;
                }
                Some(Self::Observe {
                    thread_id: thread_id.to_string(),
                })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TelegramApprovalDecision {
    AcceptOnce,
    AcceptForSession,
    Decline,
    Cancel,
}

impl TelegramApprovalDecision {
    fn label(self) -> &'static str {
        match self {
            Self::AcceptOnce => "Approved once",
            Self::AcceptForSession => "Approved for session",
            Self::Decline => "Denied",
            Self::Cancel => "Canceled",
        }
    }
}

fn approval_callback_data(nonce: &str, decision: TelegramApprovalDecision) -> String {
    let suffix = match decision {
        TelegramApprovalDecision::AcceptOnce => "a",
        TelegramApprovalDecision::AcceptForSession => "s",
        TelegramApprovalDecision::Decline => "d",
        TelegramApprovalDecision::Cancel => "c",
    };
    format!("cx:ap:{nonce}:{suffix}")
}

fn parse_approval_callback_data(
    data: &str,
    expected_nonce: &str,
) -> Option<TelegramApprovalDecision> {
    let rest = data.strip_prefix("cx:ap:")?;
    let (nonce, suffix) = rest.rsplit_once(':')?;
    if nonce != expected_nonce {
        return None;
    }
    match suffix {
        "a" => Some(TelegramApprovalDecision::AcceptOnce),
        "s" => Some(TelegramApprovalDecision::AcceptForSession),
        "d" => Some(TelegramApprovalDecision::Decline),
        "c" => Some(TelegramApprovalDecision::Cancel),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageAccess {
    Allowed,
    AuthorizedByBind,
    DeniedBind,
    Ignored,
}

fn message_access(
    message: &TelegramMessage,
    state: &TelegramState,
    allowed: &BTreeSet<TelegramRoute>,
    bind_secret: Option<&str>,
    trust_existing: bool,
) -> MessageAccess {
    let route = TelegramRoute::from_message(message);
    if trust_existing && (route_is_trusted(&route, allowed) || route_has_binding(&route, state)) {
        return MessageAccess::Allowed;
    }

    let text = message.text.as_deref().unwrap_or_default();
    match TelegramTextCommand::parse(text) {
        TelegramTextCommand::Bind { secret } => {
            if bind_secret.is_some_and(|expected| secret.as_deref() == Some(expected)) {
                MessageAccess::AuthorizedByBind
            } else {
                MessageAccess::DeniedBind
            }
        }
        _ => MessageAccess::Ignored,
    }
}

fn route_is_trusted(route: &TelegramRoute, allowed: &BTreeSet<TelegramRoute>) -> bool {
    allowed.contains(route)
        || route.message_thread_id.is_some_and(|_| {
            allowed.contains(&TelegramRoute {
                chat_id: route.chat_id,
                message_thread_id: None,
            })
        })
}

fn route_has_binding(route: &TelegramRoute, state: &TelegramState) -> bool {
    state.active_binding_for_route(route).is_some()
        || route.message_thread_id.is_some_and(|_| {
            state.bindings.iter().any(|binding| {
                binding.chat_id == route.chat_id && binding.message_thread_id.is_none()
            })
        })
}

fn handle_message(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    message: TelegramMessage,
    options: HandleOptions<'_>,
) -> Result<Option<TelegramReply>> {
    let route = TelegramRoute::from_message(&message);
    let chat_id = route.chat_id;
    let chat_is_forum = message.chat.is_forum;
    if let Some(edit) = message.forum_topic_edited.as_ref() {
        if let Some(name) = edit.name.as_deref() {
            state.set_topic_title(&route, name);
        }
        return Ok(None);
    }
    let text = message.text.clone().unwrap_or_default();
    let command = TelegramTextCommand::parse(&text);

    if let Some(callback_data) = options.callback_data {
        return handle_callback(paths, state, &route, chat_is_forum, callback_data, &options);
    }
    if text.trim().is_empty() {
        return Ok(None);
    }

    match command {
        TelegramTextCommand::Portal => Ok(Some(
            portal_reply(
                paths,
                &route,
                options.app_server_timeout,
                None,
                options.trace_timings,
            )
            .unwrap_or_else(|err| portal_unavailable_reply(&route, err, None)),
        )),
        TelegramTextCommand::Attach { thread_id } => {
            let Some(thread_id) = thread_id.as_deref() else {
                return Ok(Some(reply(&route, "Usage: /attach <thread-id>")));
            };
            attach_app_thread(paths, state, &route, chat_is_forum, thread_id, &options)
                .or_else(|err| Ok(Some(reply(&route, portal_unavailable_message(err)))))
        }
        TelegramTextCommand::Takeover => takeover_handoff(paths, state, &route, &options, None),
        TelegramTextCommand::Status => {
            let Some(binding) = state.active_binding_for_route(&route) else {
                return Ok(Some(reply(&route, no_session_bound_message(state, &route))));
            };
            let session = session::show_session(paths, &binding.session_id)?;
            Ok(Some(reply(
                    &route,
                    format!(
                        "session: {}\nalias: {}\nroute: {}\ntopic_title: {}\nchannel: {}\ntelegram: {}\napp_thread: {}\ncwd: {}\nlease_epoch: {}\nactive_lease: {}",
                        session.session_id,
                        binding.alias.as_deref().unwrap_or("default"),
                        route.display(),
                        binding.topic_title.as_deref().unwrap_or("<unknown>"),
                        session.current_channel_id,
                        if binding.telegram_paused {
                            "paused"
                        } else {
                            "active"
                        },
                        binding.app_thread_id.as_deref().unwrap_or("<none>"),
                        binding.app_thread_cwd.as_deref().unwrap_or("<unknown>"),
                        session.lease_epoch,
                        session
                            .active_lease
                            .as_ref()
                            .map(|lease| lease.channel_id.to_string())
                            .unwrap_or_else(|| String::from("<none>"))
                    ),
                )))
        }
        TelegramTextCommand::Sessions => {
            let lines = route_session_lines(state, chat_id);
            if lines.is_empty() {
                return Ok(Some(reply(
                    &route,
                    "No Telegram sessions are bound for this chat. Send /start to create one.",
                )));
            }
            Ok(Some(reply(&route, lines.join("\n"))))
        }
        TelegramTextCommand::Release => release_handoff(paths, state, &route, None),
        TelegramTextCommand::Bind { .. } if !options.authorized_by_bind => {
            Ok(Some(reply(&route, "Invalid or disabled bind secret.")))
        }
        TelegramTextCommand::Bind { .. } => Ok(Some(reply(
            &route,
            format!(
                "Trusted Telegram route {}.",
                trusted_route_for_message(&message).display()
            ),
        ))),
        TelegramTextCommand::Start => {
            let binding = state.bind_route(paths, &route, None)?;
            session::record_channel_message(
                paths,
                RecordChannelMessageRequest {
                    session_id: binding.session_id.clone(),
                    channel_id: binding.channel_id.clone(),
                },
            )?;
            if options.acquire_lease {
                match session::acquire_lease(
                    paths,
                    AcquireLeaseRequest {
                        session_id: binding.session_id.clone(),
                        channel_id: binding.channel_id.clone(),
                        steal: options.steal,
                    },
                ) {
                    Ok(_) => {}
                    Err(err) => {
                        return Ok(Some(reply(
                                &route,
                                format!(
                                    "Session is controlled elsewhere. Use --steal on the adapter if this Telegram channel should take over.\n{err:#}"
                                ),
                            )));
                    }
                }
            }
            Ok(Some(reply(
                &route,
                format!(
                    "Bound route {} to cx session {}.",
                    route.display(),
                    binding.session_id
                ),
            )))
        }
        TelegramTextCommand::New { alias } => {
            let alias = normalize_alias(alias.as_deref())
                .unwrap_or_else(|| next_route_alias(state, &route));
            if state
                .bindings
                .iter()
                .any(|b| b.chat_id == route.chat_id && b.alias.as_deref() == Some(&alias))
            {
                return Ok(Some(reply(
                    &route,
                    format!("Session `{alias}` already exists. Use /use {alias} to switch to it."),
                )));
            }
            let effective_route = if route.message_thread_id.is_none() && chat_is_forum {
                if let Some(notifier) = options.notifier {
                    match create_forum_topic(notifier.client, notifier.token, route.chat_id, &alias)
                    {
                        Ok(topic_id) => TelegramRoute {
                            chat_id: route.chat_id,
                            message_thread_id: Some(topic_id),
                        },
                        Err(err) => {
                            return Ok(Some(reply(
                                &route,
                                format!("Failed to create forum topic: {err}"),
                            )))
                        }
                    }
                } else {
                    route.clone()
                }
            } else {
                route.clone()
            };
            let binding = match state.bind_route(paths, &effective_route, Some(&alias)) {
                Ok(binding) => binding,
                Err(err) => {
                    if let (Some(topic_id), Some(notifier)) =
                        (effective_route.message_thread_id, options.notifier)
                    {
                        let _ = delete_forum_topic(
                            notifier.client,
                            notifier.token,
                            effective_route.chat_id,
                            topic_id,
                        );
                    }
                    return Err(err);
                }
            };
            if effective_route.message_thread_id.is_some() && route.message_thread_id.is_none() {
                if let Some(b) = state.binding_for_route_mut(&effective_route, Some(&alias)) {
                    b.topic_created_by_adapter = true;
                }
            }
            Ok(Some(reply(
                &effective_route,
                format!(
                    "Created session `{alias}` for route {}: {}.",
                    effective_route.display(),
                    binding.session_id
                ),
            )))
        }
        TelegramTextCommand::Use { alias } => {
            let Some(alias) = normalize_alias(alias.as_deref()) else {
                return Ok(Some(reply(&route, "Usage: /use <name>")));
            };
            if state.binding_for_route(&route, Some(&alias)).is_none() {
                return Ok(Some(reply(
                    &route,
                    format!(
                        "No session named `{alias}` is bound to route {}.",
                        route.display()
                    ),
                )));
            }
            state.set_active_route(&route, Some(&alias));
            Ok(Some(reply(&route, format!("Using session `{alias}`."))))
        }
        TelegramTextCommand::Close { alias } => {
            let alias = normalize_alias(alias.as_deref())
                .or_else(|| state.active_alias_for_route(&route).map(str::to_string));
            close_bound_route(paths, state, &route, alias.as_deref(), &options)
        }
        TelegramTextCommand::Watch => {
            let Some(binding) = state.active_binding_for_route(&route).cloned() else {
                return Ok(Some(reply(&route, no_session_bound_message(state, &route))));
            };
            watch_bound_thread(paths, state, &route, &binding, &options, true)
        }
        TelegramTextCommand::Message => {
            if is_portal_route(&route, chat_is_forum) {
                return Ok(Some(
                    portal_reply(
                        paths,
                        &route,
                        options.app_server_timeout,
                        None,
                        options.trace_timings,
                    )
                    .unwrap_or_else(|err| portal_unavailable_reply(&route, err, None)),
                ));
            }
            let binding = state
                .active_binding_for_route(&route)
                .cloned()
                .map(Ok)
                .unwrap_or_else(|| state.bind_route(paths, &route, None))?;
            if binding.telegram_paused {
                return Ok(Some(work_panel_reply(
                    &route,
                    &binding,
                    "Telegram handoff is paused for this topic.",
                    None,
                )));
            }
            session::record_channel_message(
                paths,
                RecordChannelMessageRequest {
                    session_id: binding.session_id.clone(),
                    channel_id: binding.channel_id.clone(),
                },
            )?;
            if options.acquire_lease {
                match session::acquire_lease(
                    paths,
                    AcquireLeaseRequest {
                        session_id: binding.session_id.clone(),
                        channel_id: binding.channel_id.clone(),
                        steal: options.steal,
                    },
                ) {
                    Ok(_) => {}
                    Err(err) => {
                        return Ok(Some(reply(
                                &route,
                                format!(
                                    "Session is controlled elsewhere. Use --steal on the adapter if this Telegram channel should take over.\n{err:#}"
                                ),
                            )));
                    }
                }
            }
            let _typing = options.notifier.map(|notifier| notifier.typing(&route));
            match run_codex_turn(
                    paths,
                    state,
                    &route,
                    binding.alias.as_deref(),
                    text,
                    CodexTurnOptions {
                        timeout_secs: options.app_server_timeout,
                        notifier: options.notifier,
                        trace_timings: options.trace_timings,
                    },
                ) {
                    Ok(turn) if turn.assistant_text.trim().is_empty() => {
                        Ok(Some(reply(&route, "Codex completed without a text reply.")))
                    }
                    Ok(turn) if turn.streamed_to_telegram => Ok(None),
                    Ok(turn) => Ok(Some(reply(&route, turn.assistant_text))),
                    Err(err) => Ok(Some(reply(
                        &route,
                        format!("Codex turn failed.\n{err:#}\n\nStart app-server with `cx serve start` and keep it running, then retry."),
                    ))),
                }
        }
    }
}

fn handle_callback(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    chat_is_forum: bool,
    callback_data: &str,
    options: &HandleOptions<'_>,
) -> Result<Option<TelegramReply>> {
    let edit_message_id = options.callback_message_id;
    if callback_data.starts_with("cx:ap:") {
        return Ok(Some(reply(
            route,
            "That approval request is no longer pending.",
        )));
    }
    match TelegramCallbackCommand::parse(callback_data) {
        Some(TelegramCallbackCommand::RefreshPortal) => Ok(Some(
            portal_reply(
                paths,
                route,
                options.app_server_timeout,
                edit_message_id,
                options.trace_timings,
            )
            .unwrap_or_else(|err| portal_unavailable_reply(route, err, edit_message_id)),
        )),
        Some(TelegramCallbackCommand::RefreshWork) => {
            Ok(Some(refresh_work_panel(state, route, edit_message_id)))
        }
        Some(TelegramCallbackCommand::Observe { thread_id }) => {
            observe_portal_thread(paths, state, route, chat_is_forum, &thread_id, options)
                .or_else(|err| Ok(Some(reply(route, portal_unavailable_message(err)))))
        }
        Some(TelegramCallbackCommand::Takeover) => {
            takeover_handoff(paths, state, route, options, edit_message_id)
        }
        Some(TelegramCallbackCommand::Release) => {
            release_handoff(paths, state, route, edit_message_id)
        }
        Some(TelegramCallbackCommand::Close) => {
            close_bound_route(paths, state, route, None, options)
        }
        None => Ok(Some(reply(route, "Unsupported portal action."))),
    }
}

fn portal_reply(
    paths: &ManagerPaths,
    route: &TelegramRoute,
    _timeout_secs: f32,
    edit_message_id: Option<i64>,
    trace_timings: bool,
) -> Result<TelegramReply> {
    let portal_start = Instant::now();
    let route_log = route.display();
    let entries = list_portal_entries(paths, 8, trace_timings, &route_log)?;
    if trace_timings {
        eprintln!(
            "telegram timing route={} phase=portal_complete entries={} elapsed_ms={}",
            route_log,
            entries.len(),
            elapsed_ms(portal_start)
        );
    }
    if entries.is_empty() {
        return Ok(panel_reply_with_keyboard(
                route,
                "No Codex threads are visible. Start or resume a local cx session first, then tap Refresh.",
                portal_keyboard(&entries),
                edit_message_id,
            ));
    }
    Ok(panel_reply_with_keyboard(
        route,
        portal_text(&entries),
        portal_keyboard(&entries),
        edit_message_id,
    ))
}

fn attach_app_thread(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    chat_is_forum: bool,
    thread_id: &str,
    options: &HandleOptions<'_>,
) -> Result<Option<TelegramReply>> {
    observe_portal_thread(paths, state, route, chat_is_forum, thread_id, options)
}

fn observe_portal_thread(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    chat_is_forum: bool,
    thread_id: &str,
    options: &HandleOptions<'_>,
) -> Result<Option<TelegramReply>> {
    let entries = list_portal_entries(paths, 30, options.trace_timings, &route.display())?;
    let Some(entry) = entries.iter().find(|e| e.thread_id == thread_id) else {
        return Ok(Some(reply(
            route,
            "That Codex thread is no longer visible. Run /portal and choose again.",
        )));
    };
    if !entry.watchable {
        return Ok(Some(reply(
            route,
            "That thread cannot be observed right now. Refresh /portal and try again.",
        )));
    }

    let mut topic_created_by_adapter = false;
    let effective_route = if should_create_work_topic(state, route, chat_is_forum) {
        if let Some(notifier) = options.notifier {
            let topic_id = match create_forum_topic(
                notifier.client,
                notifier.token,
                route.chat_id,
                &work_topic_title(entry),
            ) {
                Ok(topic_id) => topic_id,
                Err(err) => {
                    return Ok(Some(reply(
                        route,
                        format!("Failed to create Telegram topic: {err:#}"),
                    )));
                }
            };
            topic_created_by_adapter = true;
            TelegramRoute {
                chat_id: route.chat_id,
                message_thread_id: Some(topic_id),
            }
        } else {
            route.clone()
        }
    } else {
        route.clone()
    };

    let mut binding =
        state.bind_app_thread(paths, &effective_route, entry, topic_created_by_adapter)?;
    if let Some(stored) = state.binding_for_route_mut(&effective_route, None) {
        stored.telegram_paused = true;
        initialize_watch_cursor(paths, stored);
        binding = stored.clone();
    }
    session::record_channel_message(
        paths,
        RecordChannelMessageRequest {
            session_id: binding.session_id.clone(),
            channel_id: binding.channel_id.clone(),
        },
    )?;
    write_state(paths, state)?;

    if let Some(notifier) = options.notifier {
        send_watch_intro(paths, state, notifier, &effective_route, &binding, entry)?;
        write_state(paths, state)?;
    }

    watch_bound_thread(paths, state, &effective_route, &binding, options, false)
}

fn takeover_handoff(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    options: &HandleOptions<'_>,
    edit_message_id: Option<i64>,
) -> Result<Option<TelegramReply>> {
    let Some(binding) = state.active_binding_for_route(route).cloned() else {
        return Ok(Some(reply(route, no_session_bound_message(state, route))));
    };
    match interrupt_binding_thread(paths, &binding, options.app_server_timeout) {
        Ok(interrupted) => {
            if let Some(stored) = state.binding_for_route_mut(route, binding.alias.as_deref()) {
                stored.telegram_paused = false;
            }
            let binding = state
                .binding_for_route(route, binding.alias.as_deref())
                .cloned()
                .unwrap_or(binding);
            Ok(Some(work_panel_reply(
                route,
                &binding,
                takeover_message(interrupted.interrupted_turn_id.as_deref()),
                edit_message_id,
            )))
        }
        Err(err) => Ok(Some(reply(route, portal_unavailable_message(err)))),
    }
}

fn release_handoff(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    edit_message_id: Option<i64>,
) -> Result<Option<TelegramReply>> {
    let Some(binding) = state.active_binding_for_route(route).cloned() else {
        return Ok(Some(reply(route, no_session_bound_message(state, route))));
    };
    if let Some(stored) = state.binding_for_route_mut(route, binding.alias.as_deref()) {
        stored.telegram_paused = true;
    }
    let mut released_lease = false;
    if let Ok(session) = session::show_session(paths, &binding.session_id) {
        if let Some(active_lease) = session.active_lease {
            if active_lease.channel_id == binding.channel_id {
                session::release_lease(
                    paths,
                    session::ReleaseLeaseRequest {
                        session_id: binding.session_id.clone(),
                        lease_token: active_lease.lease_token,
                    },
                )?;
                released_lease = true;
            }
        }
    }
    let suffix = if released_lease {
        " Lease released."
    } else {
        ""
    };
    let binding = state
        .binding_for_route(route, binding.alias.as_deref())
        .cloned()
        .unwrap_or(binding);
    Ok(Some(work_panel_reply(
        route,
        &binding,
        format!("Telegram handoff paused.{suffix} Continue on desktop."),
        edit_message_id,
    )))
}

fn release_binding_lease_if_owned(paths: &ManagerPaths, binding: &TelegramBinding) -> Result<bool> {
    let Ok(session) = session::show_session(paths, &binding.session_id) else {
        return Ok(false);
    };
    let Some(active_lease) = session.active_lease else {
        return Ok(false);
    };
    if active_lease.channel_id != binding.channel_id {
        return Ok(false);
    }
    session::release_lease(
        paths,
        session::ReleaseLeaseRequest {
            session_id: binding.session_id.clone(),
            lease_token: active_lease.lease_token,
        },
    )?;
    Ok(true)
}

fn watch_bound_thread(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    binding: &TelegramBinding,
    _options: &HandleOptions<'_>,
    show_idle_reply: bool,
) -> Result<Option<TelegramReply>> {
    let Some(_thread_id) = binding.app_thread_id.as_deref() else {
        return Ok(Some(reply(
            route,
            "This session is not bound to a Codex thread yet. Send a message first.",
        )));
    };
    if let Some(stored) = state.binding_for_route_mut(route, binding.alias.as_deref()) {
        stored.telegram_paused = true;
        initialize_watch_cursor(paths, stored);
    }
    release_binding_lease_if_owned(paths, binding)?;
    let binding = state
        .binding_for_route(route, binding.alias.as_deref())
        .cloned()
        .unwrap_or_else(|| binding.clone());

    if show_idle_reply {
        return Ok(Some(work_panel_reply(
            route,
            &binding,
            "Telegram handoff paused. Watching for desktop activity.",
            None,
        )));
    }
    Ok(None)
}

fn initialize_watch_cursor(paths: &ManagerPaths, binding: &mut TelegramBinding) {
    binding.watch_activity = None;
    binding.watch_thinking = None;
    binding.watch_status = None;
    binding.watch_source = None;
    binding.watch_last_agent_message = None;
    binding.watch_pending_approvals.clear();
    if binding.watch_proxy_offset.is_none() {
        if let Ok(metadata) = fs::metadata(proxy_event_log_path(paths)) {
            binding.watch_proxy_offset = Some(metadata.len());
        }
    }
    if binding.watch_rollout_offset.is_some() {
        return;
    }
    let Some(thread_id) = binding.app_thread_id.as_deref() else {
        return;
    };
    let Some(path) = rollout_path_for_thread(paths, thread_id) else {
        return;
    };
    let Ok(metadata) = fs::metadata(&path) else {
        return;
    };
    let offset = latest_active_rollout_turn(&path)
        .ok()
        .flatten()
        .map(|(_, offset)| offset)
        .unwrap_or_else(|| metadata.len());
    binding.watch_rollout_offset = Some(offset);
}

fn proxy_event_log_path(paths: &ManagerPaths) -> PathBuf {
    paths.serve_dir().join("events").join("default.jsonl")
}

fn refresh_work_panel(
    state: &TelegramState,
    route: &TelegramRoute,
    edit_message_id: Option<i64>,
) -> TelegramReply {
    let Some(binding) = state.active_binding_for_route(route) else {
        return reply(route, no_session_bound_message(state, route));
    };
    work_panel_reply(route, binding, "cx handoff", edit_message_id)
}

fn close_bound_route(
    _paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    alias: Option<&str>,
    options: &HandleOptions<'_>,
) -> Result<Option<TelegramReply>> {
    let alias = alias
        .map(str::to_string)
        .or_else(|| state.active_alias_for_route(route).map(str::to_string));
    let Some(binding) = state.binding_for_route(route, alias.as_deref()).cloned() else {
        return Ok(Some(reply(
            route,
            missing_session_message(state, route, alias.as_deref()),
        )));
    };
    let label = alias.as_deref().unwrap_or("default").to_string();
    if let Some(thread_id) = binding.message_thread_id {
        let mut close_errors = Vec::new();
        if let Some(notifier) = options.notifier {
            if let Err(err) =
                delete_forum_topic(notifier.client, notifier.token, binding.chat_id, thread_id)
            {
                close_errors.push(format!("Failed to delete Telegram topic: {err:#}"));
            }
            state.remove_all_route_bindings(route);
            if close_errors.is_empty() {
                return Ok(None);
            }
            return Ok(Some(reply(
                route,
                format!(
                    "Unbound session `{label}` locally, but some close actions failed.\n{}",
                    close_errors.join("\n")
                ),
            )));
        }
    }
    if state.remove_route_binding(route, alias.as_deref()) {
        Ok(Some(reply(route, format!("Unbound session `{label}`."))))
    } else {
        Ok(Some(reply(
            route,
            missing_session_message(state, route, alias.as_deref()),
        )))
    }
}

fn should_create_work_topic(
    state: &TelegramState,
    route: &TelegramRoute,
    chat_is_forum: bool,
) -> bool {
    chat_is_forum
        && (route.message_thread_id.is_none() || state.active_binding_for_route(route).is_none())
}

fn interrupt_binding_thread(
    paths: &ManagerPaths,
    binding: &TelegramBinding,
    timeout_secs: f32,
) -> Result<crate::app_server::InterruptOutcome> {
    let Some(thread_id) = binding.app_thread_id.as_deref() else {
        return Ok(crate::app_server::InterruptOutcome {
            interrupted_turn_id: None,
        });
    };
    let mut client = connect_app_server(paths, timeout_secs)?;
    client.interrupt_active_turn(thread_id)
}

fn connect_app_server(paths: &ManagerPaths, timeout_secs: f32) -> Result<AppServerClient> {
    connect_app_server_with_timeout(paths, Duration::from_secs_f32(timeout_secs))
}

fn connect_app_server_with_timeout(
    paths: &ManagerPaths,
    timeout: Duration,
) -> Result<AppServerClient> {
    let server = serve::ready_app_server(paths)?;
    let mut client = AppServerClient::connect(&server.listen_url, timeout)?;
    client.initialize("cx-telegram", env!("CARGO_PKG_VERSION"))?;
    Ok(client)
}

fn list_portal_entries(
    paths: &ManagerPaths,
    limit: u64,
    trace_timings: bool,
    route_log: &str,
) -> Result<Vec<AppThreadPortalEntry>> {
    let candidate_limit = limit.saturating_mul(8).max(50);
    let mut candidates = Vec::new();
    let mut app_server_error = None::<anyhow::Error>;

    match app_server_portal_candidates(paths, candidate_limit, trace_timings, route_log) {
        Ok(mut app_candidates) => candidates.append(&mut app_candidates),
        Err(err) => {
            if trace_timings {
                eprintln!(
                    "telegram timing route={} phase=portal_app_server_error error={err:#}",
                    route_log
                );
            }
            app_server_error = Some(err);
        }
    }

    let local_start = Instant::now();
    let mut local_candidates = local_state_portal_candidates(paths, candidate_limit);
    if trace_timings {
        eprintln!(
            "telegram timing route={} phase=portal_local_state threads={} elapsed_ms={}",
            route_log,
            local_candidates.len(),
            elapsed_ms(local_start)
        );
    }
    candidates.append(&mut local_candidates);

    let entries = filter_portal_candidates(candidates, limit);
    if entries.is_empty() {
        if let Some(err) = app_server_error {
            return Err(err);
        }
    }
    Ok(entries)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PortalEntryCandidate {
    entry: AppThreadPortalEntry,
    updated_at_unix: i64,
    priority: u8,
}

fn app_server_portal_candidates(
    paths: &ManagerPaths,
    limit: u64,
    trace_timings: bool,
    route_log: &str,
) -> Result<Vec<PortalEntryCandidate>> {
    let state_start = Instant::now();
    let server = serve::registered_app_server(paths)?;
    if trace_timings {
        eprintln!(
            "telegram timing route={} phase=portal_app_server_registry elapsed_ms={}",
            route_log,
            elapsed_ms(state_start)
        );
    }
    let connect_start = Instant::now();
    let mut client = AppServerClient::connect(&server.listen_url, PORTAL_APP_SERVER_TIMEOUT)?;
    client.initialize("cx-telegram", env!("CARGO_PKG_VERSION"))?;
    if trace_timings {
        eprintln!(
            "telegram timing route={} phase=portal_app_server_connect timeout_ms={} elapsed_ms={}",
            route_log,
            PORTAL_APP_SERVER_TIMEOUT.as_millis(),
            elapsed_ms(connect_start)
        );
    }
    let list_start = Instant::now();
    let page = client.thread_list(limit)?;
    if trace_timings {
        eprintln!(
            "telegram timing route={} phase=portal_thread_list threads={} elapsed_ms={}",
            route_log,
            page.threads.len(),
            elapsed_ms(list_start)
        );
    }
    Ok(page
        .threads
        .into_iter()
        .map(|summary| PortalEntryCandidate {
            updated_at_unix: summary.updated_at_unix,
            entry: AppThreadPortalEntry::from(summary),
            priority: 3,
        })
        .collect())
}

fn local_state_portal_candidates(paths: &ManagerPaths, limit: u64) -> Vec<PortalEntryCandidate> {
    let mut candidates = Vec::new();
    for db_path in codex_state_db_paths(paths) {
        if !db_path.exists() {
            continue;
        }
        match local_state_portal_candidates_from_db(&db_path, limit) {
            Ok(mut db_candidates) => candidates.append(&mut db_candidates),
            Err(err) => {
                eprintln!(
                    "telegram portal skipped local state db {}: {err:#}",
                    db_path.display()
                );
            }
        }
    }
    candidates
}

fn local_state_portal_candidates_from_db(
    db_path: &Path,
    limit: u64,
) -> Result<Vec<PortalEntryCandidate>> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", db_path.display()))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, title, first_user_message, cwd, updated_at \
                 FROM threads \
                 WHERE rollout_path <> '' \
                 ORDER BY updated_at DESC \
                 LIMIT ?1",
        )
        .with_context(|| format!("prepare portal state query for {}", db_path.display()))?;
    let rows = stmt.query_map(params![limit as i64], |row| {
        let thread_id: String = row.get(0)?;
        let title: Option<String> = row.get(1)?;
        let preview: Option<String> = row.get(2)?;
        let cwd: Option<String> = row.get(3)?;
        let updated_at_unix: i64 = row.get(4)?;
        Ok((
            thread_id,
            title.unwrap_or_default(),
            preview.unwrap_or_default(),
            cwd.unwrap_or_default(),
            updated_at_unix,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows {
        let (thread_id, title, preview, cwd, updated_at_unix) = row?;
        candidates.push(PortalEntryCandidate {
            entry: AppThreadPortalEntry {
                thread_id,
                title: (!title.trim().is_empty()).then_some(title),
                preview,
                cwd,
                active: false,
                watchable: true,
                status: "notLoaded".to_string(),
            },
            updated_at_unix,
            priority: 1,
        });
    }
    Ok(candidates)
}

fn filter_portal_candidates(
    candidates: Vec<PortalEntryCandidate>,
    limit: u64,
) -> Vec<AppThreadPortalEntry> {
    let mut by_thread = BTreeMap::<String, PortalEntryCandidate>::new();
    for candidate in candidates {
        if is_local_watch_regression_entry(&candidate.entry) {
            continue;
        }
        by_thread
            .entry(candidate.entry.thread_id.clone())
            .and_modify(|existing| {
                if candidate.priority > existing.priority
                    || candidate.priority == existing.priority
                        && candidate.updated_at_unix > existing.updated_at_unix
                {
                    *existing = candidate.clone();
                }
            })
            .or_insert(candidate);
    }
    let mut candidates = by_thread.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .updated_at_unix
            .cmp(&left.updated_at_unix)
            .then_with(|| right.priority.cmp(&left.priority))
            .then_with(|| left.entry.thread_id.cmp(&right.entry.thread_id))
    });
    candidates
        .into_iter()
        .take(limit as usize)
        .map(|candidate| candidate.entry)
        .collect()
}

#[cfg(test)]
fn filter_portal_entries<I>(entries: I, limit: u64) -> Vec<AppThreadPortalEntry>
where
    I: IntoIterator<Item = AppThreadPortalEntry>,
{
    filter_portal_candidates(
        entries
            .into_iter()
            .enumerate()
            .map(|(index, entry)| PortalEntryCandidate {
                entry,
                updated_at_unix: i64::MAX.saturating_sub(index as i64),
                priority: 1,
            })
            .collect(),
        limit,
    )
}

#[cfg(test)]
fn open_tui_portal_entries(
    paths: &ManagerPaths,
    max_candidates: u64,
) -> Result<Vec<AppThreadPortalEntry>> {
    Ok(filter_portal_candidates(
        local_state_portal_candidates(paths, max_candidates),
        max_candidates,
    ))
}

fn is_local_watch_regression_entry(entry: &AppThreadPortalEntry) -> bool {
    const MARKERS: [&str; 3] = [
        "For Telegram watch E2E marker",
        "Telegram watch regression",
        "For a local watch integration test",
    ];
    MARKERS.iter().any(|marker| {
        entry
            .title
            .as_deref()
            .is_some_and(|title| title.contains(marker))
            || entry.preview.contains(marker)
    })
}

fn portal_text(entries: &[AppThreadPortalEntry]) -> String {
    let mut lines = vec![String::from(PORTAL_TOPIC_TITLE)];
    for (index, entry) in entries.iter().enumerate() {
        lines.push(format!(
            "{}. {} [{}]\n   cwd: {}",
            index + 1,
            work_topic_title(entry),
            entry.status,
            entry.cwd
        ));
    }
    lines.push(String::from(
        "Threads are observed by default. Use Take over inside a topic to interrupt desktop Codex.",
    ));
    lines.join("\n")
}

fn portal_keyboard(entries: &[AppThreadPortalEntry]) -> TelegramInlineKeyboardMarkup {
    let mut rows = Vec::new();
    for entry in entries {
        rows.push(vec![TelegramInlineKeyboardButton {
            text: format!("Watch: {}", truncate_chars(&work_topic_title(entry), 48)),
            callback_data: format!("cx:o:{}", entry.thread_id),
        }]);
    }
    rows.push(vec![TelegramInlineKeyboardButton {
        text: String::from("Refresh"),
        callback_data: String::from("cx:p"),
    }]);
    TelegramInlineKeyboardMarkup {
        inline_keyboard: rows,
    }
}

fn work_topic_title(entry: &AppThreadPortalEntry) -> String {
    let title = entry
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| {
            let preview = entry.preview.trim();
            (!preview.is_empty()).then_some(preview)
        })
        .unwrap_or("Codex thread");
    truncate_chars(title.lines().next().unwrap_or("Codex thread"), 64)
}

fn send_watch_intro(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    notifier: &TelegramNotifier<'_>,
    route: &TelegramRoute,
    binding: &TelegramBinding,
    entry: &AppThreadPortalEntry,
) -> Result<()> {
    let panel_text = watch_started_text(entry);
    match notifier.send_with_keyboard(route, &panel_text, &work_panel_keyboard(binding, route)) {
        Ok(panel_message_id) => state.remember_panel_message(route, panel_message_id),
        Err(err) => {
            eprintln!(
                "telegram watch intro delivery failed for {}: {err:#}",
                route.display()
            );
            return Ok(());
        }
    }

    if let Some(history) = rollout_history_text(paths, &entry.thread_id, 6)? {
        if let Err(err) = notifier.send_chunks(route, &history) {
            eprintln!(
                "telegram watch history delivery failed for {}: {err:#}",
                route.display()
            );
        }
    }
    Ok(())
}

fn watch_started_text(entry: &AppThreadPortalEntry) -> String {
    format!(
            "Watching Codex thread.\nTelegram handoff is paused for this topic.\nthread: {}\ncwd: {}\nstatus: {}\nLive output will stream here while the desktop turn runs.",
            entry.thread_id, entry.cwd, entry.status
        )
}

fn takeover_message(interrupted_turn_id: Option<&str>) -> String {
    match interrupted_turn_id {
        Some(turn_id) => {
            format!("Telegram handoff active. Interrupted desktop turn {turn_id}.")
        }
        None => {
            String::from("Telegram handoff active. No active desktop turn needed interruption.")
        }
    }
}

fn portal_unavailable_message(err: anyhow::Error) -> String {
    format!(
            "Codex portal is unavailable.\n{err:#}\n\nStart or restart the local service with `cx service start`, then tap Refresh."
        )
}

fn portal_unavailable_reply(
    route: &TelegramRoute,
    err: anyhow::Error,
    edit_message_id: Option<i64>,
) -> TelegramReply {
    panel_reply_with_keyboard(
        route,
        portal_unavailable_message(err),
        portal_keyboard(&[]),
        edit_message_id,
    )
}

fn work_panel_reply(
    route: &TelegramRoute,
    binding: &TelegramBinding,
    headline: impl Into<String>,
    edit_message_id: Option<i64>,
) -> TelegramReply {
    panel_reply_with_keyboard(
        route,
        work_panel_text(binding, headline.into()),
        work_panel_keyboard(binding, route),
        edit_message_id,
    )
}

fn work_panel_text(binding: &TelegramBinding, headline: String) -> String {
    let status = if binding.telegram_paused {
        "paused"
    } else {
        "telegram active"
    };
    format!(
        "{headline}\nstate: {status}\nthread: {}\ncwd: {}",
        binding.app_thread_id.as_deref().unwrap_or("<none>"),
        binding.app_thread_cwd.as_deref().unwrap_or("<unknown>")
    )
}

fn work_panel_keyboard(
    binding: &TelegramBinding,
    route: &TelegramRoute,
) -> TelegramInlineKeyboardMarkup {
    let control = if binding.telegram_paused {
        TelegramInlineKeyboardButton {
            text: String::from("Take over from desktop"),
            callback_data: String::from("cx:t"),
        }
    } else {
        TelegramInlineKeyboardButton {
            text: String::from("Release to desktop"),
            callback_data: String::from("cx:rel"),
        }
    };
    let close_text = if route.message_thread_id.is_some() {
        "Close topic"
    } else {
        "Close session"
    };
    TelegramInlineKeyboardMarkup {
        inline_keyboard: vec![
            vec![control],
            vec![
                TelegramInlineKeyboardButton {
                    text: String::from("Refresh"),
                    callback_data: String::from("cx:w"),
                },
                TelegramInlineKeyboardButton {
                    text: String::from(close_text),
                    callback_data: String::from("cx:c"),
                },
            ],
        ],
    }
}

fn is_portal_route(route: &TelegramRoute, chat_is_forum: bool) -> bool {
    chat_is_forum && route.message_thread_id.is_none()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut result = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        result.push_str("...");
    }
    result
}

struct HandleOptions<'a> {
    notifier: Option<&'a TelegramNotifier<'a>>,
    acquire_lease: bool,
    steal: bool,
    authorized_by_bind: bool,
    app_server_timeout: f32,
    trace_timings: bool,
    callback_data: Option<&'a str>,
    callback_message_id: Option<i64>,
}

struct CodexTurnOptions<'a, 'b> {
    timeout_secs: f32,
    notifier: Option<&'a TelegramNotifier<'b>>,
    trace_timings: bool,
}

fn run_codex_turn(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    route: &TelegramRoute,
    alias: Option<&str>,
    prompt: String,
    options: CodexTurnOptions<'_, '_>,
) -> Result<CodexTurnOutput> {
    let timeout_secs = options.timeout_secs;
    let notifier = options.notifier;
    let trace_timings = options.trace_timings;
    let route_log = route.display();
    let ready_start = Instant::now();
    let server = serve::ready_app_server(paths)?;
    if trace_timings {
        eprintln!(
            "telegram timing route={} phase=app_server_ready elapsed_ms={}",
            route_log,
            elapsed_ms(ready_start)
        );
    }
    let connect_start = Instant::now();
    let mut client =
        AppServerClient::connect(&server.listen_url, Duration::from_secs_f32(timeout_secs))?;
    client.initialize("cx-telegram", env!("CARGO_PKG_VERSION"))?;
    if trace_timings {
        eprintln!(
            "telegram timing route={} phase=app_server_connect elapsed_ms={}",
            route_log,
            elapsed_ms(connect_start)
        );
    }

    let binding_snapshot = state
        .binding_for_route(route, alias)
        .cloned()
        .with_context(|| format!("Telegram route {} is not bound", route.display()))?;
    let thread_id = match binding_snapshot.app_thread_id.clone() {
        Some(thread_id) => {
            let resume_start = Instant::now();
            if let Err(err) =
                client.thread_resume(&thread_id, binding_snapshot.app_thread_cwd.as_deref())
            {
                eprintln!("app-server thread/resume before Telegram turn failed: {err:#}");
            }
            if trace_timings {
                eprintln!(
                    "telegram timing route={} thread_id={} phase=thread_resume elapsed_ms={}",
                    route_log,
                    thread_id,
                    elapsed_ms(resume_start)
                );
            }
            thread_id
        }
        None => {
            let start_thread_start = Instant::now();
            let thread = client.thread_start(binding_snapshot.app_thread_cwd.as_deref())?;
            let binding = state
                .binding_for_route_mut(route, alias)
                .with_context(|| format!("Telegram route {} is not bound", route.display()))?;
            binding.app_thread_id = Some(thread.upstream_thread_id.clone());
            if trace_timings {
                eprintln!(
                    "telegram timing route={} thread_id={} phase=thread_start elapsed_ms={}",
                    route_log,
                    thread.upstream_thread_id,
                    elapsed_ms(start_thread_start)
                );
            }
            thread.upstream_thread_id
        }
    };
    let mut sink = notifier.map(|notifier| {
        TelegramWatchSink::new_best_effort(notifier, route.clone(), None, None, None)
    });
    let turn_start = Instant::now();
    let mut first_event_seen = false;
    let turn = client.turn_start_stream(
        &thread_id,
        prompt,
        |event| {
            if trace_timings && !first_event_seen {
                first_event_seen = true;
                eprintln!(
                    "telegram timing route={} thread_id={} phase=turn_first_event elapsed_ms={}",
                    route_log,
                    thread_id,
                    elapsed_ms(turn_start)
                );
            }
            if let Some(sink) = sink.as_mut() {
                sink.push_event(event)?;
            }
            Ok(())
        },
        |approval| {
            let Some(notifier) = notifier else {
                anyhow::bail!("app-server requested approval but Telegram is unavailable");
            };
            handle_app_server_approval(paths, state, notifier, route, approval, timeout_secs)
        },
    )?;
    if trace_timings {
        eprintln!(
                "telegram timing route={} thread_id={} phase=turn_complete first_event={} elapsed_ms={}",
                route_log,
                thread_id,
                first_event_seen,
                elapsed_ms(turn_start)
            );
    }
    if let Some(sink) = sink.as_mut() {
        sink.turn_completed(None, TelegramTurnTerminal::Done)?;
        let finish_start = Instant::now();
        sink.flush_pending()?;
        if trace_timings {
            eprintln!(
                    "telegram timing route={} thread_id={} phase=telegram_sink_finish sent_any={} elapsed_ms={}",
                    route_log,
                    thread_id,
                    sink.sent_any(),
                    elapsed_ms(finish_start)
                );
        }
    }
    Ok(CodexTurnOutput {
        assistant_text: turn.assistant_text,
        streamed_to_telegram: sink.is_some_and(|sink| sink.assistant_completed_text_sent()),
    })
}

struct CodexTurnOutput {
    assistant_text: String,
    streamed_to_telegram: bool,
}

fn handle_app_server_approval(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    notifier: &TelegramNotifier<'_>,
    route: &TelegramRoute,
    approval: ApprovalRequest,
    timeout_secs: f32,
) -> Result<Value> {
    let nonce = generate_bind_secret()?;
    let prompt = approval_prompt_text(&approval);
    let message_id = notifier.send_with_keyboard(route, &prompt, &approval_keyboard(&nonce))?;
    let decision = wait_for_approval_callback(
        paths,
        state,
        notifier,
        route,
        message_id,
        &nonce,
        timeout_secs,
    )?;
    let resolved = format!("{prompt}\n\n{}", decision.label());
    if let Err(err) = notifier.edit_one(route, message_id, &resolved) {
        eprintln!("telegram edit approval prompt failed: {err:#}");
    }
    approval_response_result(&approval, decision)
}

fn wait_for_approval_callback(
    paths: &ManagerPaths,
    state: &mut TelegramState,
    notifier: &TelegramNotifier<'_>,
    route: &TelegramRoute,
    approval_message_id: i64,
    nonce: &str,
    timeout_secs: f32,
) -> Result<TelegramApprovalDecision> {
    let timeout = Duration::from_secs_f32(timeout_secs.max(1.0));
    let deadline = Instant::now() + timeout;
    loop {
        let now = Instant::now();
        if now >= deadline {
            anyhow::bail!("timed out waiting for Telegram approval");
        }
        let remaining = deadline.saturating_duration_since(now);
        let poll_timeout = remaining.as_secs().clamp(1, 10);
        let updates = get_updates(
            notifier.client,
            notifier.token,
            state.last_update_id.map(|id| id + 1),
            poll_timeout,
        )?;
        for update in updates {
            state.last_update_id = Some(
                state
                    .last_update_id
                    .map_or(update.update_id, |id| id.max(update.update_id)),
            );
            let view = update.view();
            let Some(callback_query_id) = view.callback_query_id.as_deref() else {
                continue;
            };
            let Some(message) = view.message.as_ref() else {
                notifier.answer_callback_query(callback_query_id, Some("Approval is pending."));
                continue;
            };
            let callback_route = TelegramRoute::from_message(message);
            if &callback_route != route || message.message_id != approval_message_id {
                notifier.answer_callback_query(
                    callback_query_id,
                    Some("A different cx approval is pending."),
                );
                continue;
            }
            let Some(data) = view.callback_data.as_deref() else {
                notifier.answer_callback_query(callback_query_id, Some("Missing approval action."));
                continue;
            };
            let Some(decision) = parse_approval_callback_data(data, nonce) else {
                notifier.answer_callback_query(callback_query_id, Some("Stale approval action."));
                continue;
            };
            notifier.answer_callback_query(callback_query_id, Some(decision.label()));
            write_state(paths, state)?;
            return Ok(decision);
        }
        write_state(paths, state)?;
    }
}

fn approval_keyboard(nonce: &str) -> TelegramInlineKeyboardMarkup {
    TelegramInlineKeyboardMarkup {
        inline_keyboard: vec![
            vec![
                TelegramInlineKeyboardButton {
                    text: String::from("Allow once"),
                    callback_data: approval_callback_data(
                        nonce,
                        TelegramApprovalDecision::AcceptOnce,
                    ),
                },
                TelegramInlineKeyboardButton {
                    text: String::from("Allow session"),
                    callback_data: approval_callback_data(
                        nonce,
                        TelegramApprovalDecision::AcceptForSession,
                    ),
                },
            ],
            vec![TelegramInlineKeyboardButton {
                text: String::from("Deny"),
                callback_data: approval_callback_data(nonce, TelegramApprovalDecision::Decline),
            }],
            vec![TelegramInlineKeyboardButton {
                text: String::from("Cancel turn"),
                callback_data: approval_callback_data(nonce, TelegramApprovalDecision::Cancel),
            }],
        ],
    }
}

fn approval_prompt_text(approval: &ApprovalRequest) -> String {
    let title = match approval.method.as_str() {
        "item/commandExecution/requestApproval" | "execCommandApproval" => {
            "Codex needs permission to run a command"
        }
        "item/fileChange/requestApproval" | "applyPatchApproval" => {
            "Codex needs permission to change files"
        }
        "item/permissions/requestApproval" => "Codex requests additional permissions",
        _ => "Codex requests approval",
    };
    let mut lines = vec![String::from(title)];
    if let Some(reason) = approval.params.get("reason").and_then(Value::as_str) {
        if !reason.trim().is_empty() {
            lines.push(format!("reason: {}", truncate_chars(reason.trim(), 400)));
        }
    }
    if let Some(cwd) = approval.params.get("cwd").and_then(Value::as_str) {
        lines.push(format!("cwd: {cwd}"));
    }
    if let Some(command) = approval_command_text(&approval.params) {
        lines.push(format!("command:\n{command}"));
    }
    if let Some(grant_root) = approval.params.get("grantRoot").and_then(Value::as_str) {
        lines.push(format!("grant root: {grant_root}"));
    }
    if approval.method == "item/permissions/requestApproval" {
        if let Some(permissions) = approval.params.get("permissions") {
            lines.push(format!(
                "permissions:\n{}",
                truncate_chars(&format_json_compact(permissions), 1200)
            ));
        }
    }
    lines.push(String::from(
        "Choose how cx should answer this approval request.",
    ));
    truncate_chars(&lines.join("\n"), 2800)
}

fn approval_command_text(params: &Value) -> Option<String> {
    if let Some(command) = params.get("command").and_then(Value::as_str) {
        if !command.trim().is_empty() {
            return Some(truncate_chars(command.trim(), 1200));
        }
    }
    let command = params.get("command")?.as_array()?;
    let tokens = command.iter().filter_map(Value::as_str).collect::<Vec<_>>();
    (!tokens.is_empty()).then(|| truncate_chars(&tokens.join(" "), 1200))
}

fn approval_response_result(
    approval: &ApprovalRequest,
    decision: TelegramApprovalDecision,
) -> Result<Value> {
    match approval.method.as_str() {
        "item/commandExecution/requestApproval" => Ok(json!({
            "decision": command_execution_decision(decision),
        })),
        "item/fileChange/requestApproval" => Ok(json!({
            "decision": file_change_decision(decision),
        })),
        "execCommandApproval" | "applyPatchApproval" => Ok(json!({
            "decision": legacy_review_decision(decision),
        })),
        "item/permissions/requestApproval" => {
            let permissions = if matches!(
                decision,
                TelegramApprovalDecision::Decline | TelegramApprovalDecision::Cancel
            ) {
                json!({})
            } else {
                approval
                    .params
                    .get("permissions")
                    .cloned()
                    .unwrap_or_else(|| json!({}))
            };
            let scope = match decision {
                TelegramApprovalDecision::AcceptForSession => "session",
                TelegramApprovalDecision::AcceptOnce
                | TelegramApprovalDecision::Decline
                | TelegramApprovalDecision::Cancel => "turn",
            };
            Ok(json!({
                "permissions": permissions,
                "scope": scope,
                "strictAutoReview": false,
            }))
        }
        method => anyhow::bail!("unsupported approval request method: {method}"),
    }
}

fn command_execution_decision(decision: TelegramApprovalDecision) -> &'static str {
    match decision {
        TelegramApprovalDecision::AcceptOnce => "accept",
        TelegramApprovalDecision::AcceptForSession => "acceptForSession",
        TelegramApprovalDecision::Decline => "decline",
        TelegramApprovalDecision::Cancel => "cancel",
    }
}

fn file_change_decision(decision: TelegramApprovalDecision) -> &'static str {
    match decision {
        TelegramApprovalDecision::AcceptOnce => "accept",
        TelegramApprovalDecision::AcceptForSession => "acceptForSession",
        TelegramApprovalDecision::Decline => "decline",
        TelegramApprovalDecision::Cancel => "cancel",
    }
}

fn legacy_review_decision(decision: TelegramApprovalDecision) -> &'static str {
    match decision {
        TelegramApprovalDecision::AcceptOnce => "approved",
        TelegramApprovalDecision::AcceptForSession => "approved_for_session",
        TelegramApprovalDecision::Decline => "denied",
        TelegramApprovalDecision::Cancel => "denied",
    }
}

fn format_json_compact(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn route_session_lines(state: &TelegramState, chat_id: i64) -> Vec<String> {
    let mut lines = state
        .bindings
        .iter()
        .filter(|binding| binding.chat_id == chat_id)
        .map(|binding| {
            let route = TelegramRoute {
                chat_id: binding.chat_id,
                message_thread_id: binding.message_thread_id,
            };
            let active = state
                .active_binding_for_route(&route)
                .is_some_and(|active| {
                    active.alias == binding.alias
                        && active.message_thread_id == binding.message_thread_id
                });
            let marker = if active { "*" } else { " " };
            format!(
                "{marker} {}\n  route: {}\n  session: {}",
                binding.alias.as_deref().unwrap_or("default"),
                route.display(),
                binding.session_id
            )
        })
        .collect::<Vec<_>>();
    if !lines.is_empty() {
        lines.push(String::from(
            "Commands: /use <name>, /new <name>, /close [name], /status",
        ));
    }
    lines
}

fn no_session_bound_message(state: &TelegramState, route: &TelegramRoute) -> String {
    let available = route_aliases(state, route);
    if available.is_empty() {
        format!(
                "No cx session is bound to route {}. Send /start to create the default session or /new <name> to create a named one.",
                route.display()
            )
    } else {
        format!(
                "No active cx session is bound to route {}. Available sessions: {}. Use /use <name> or send /start to create the default session.",
                route.display(),
                available.join(", ")
            )
    }
}

fn missing_session_message(
    state: &TelegramState,
    route: &TelegramRoute,
    alias: Option<&str>,
) -> String {
    let label = alias.unwrap_or("default");
    let available = route_aliases(state, route);
    if available.is_empty() {
        format!(
                "No session `{label}` is bound to route {}. Send /start to create the default session or /new <name> to create a named one.",
                route.display()
            )
    } else {
        format!(
                "No session `{label}` is bound to route {}. Available sessions: {}. Use /close <name> or /use <name>.",
                route.display(),
                available.join(", ")
            )
    }
}

fn route_aliases(state: &TelegramState, route: &TelegramRoute) -> Vec<String> {
    state
        .bindings
        .iter()
        .filter(|binding| {
            binding.chat_id == route.chat_id && binding.message_thread_id == route.message_thread_id
        })
        .map(|binding| binding.alias.as_deref().unwrap_or("default").to_string())
        .collect()
}

fn normalize_alias(alias: Option<&str>) -> Option<String> {
    let alias = alias?.trim().to_ascii_lowercase();
    if alias.is_empty() {
        return None;
    }
    let normalized = alias
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        .collect::<String>();
    let normalized = normalized.chars().take(32).collect::<String>();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn next_route_alias(state: &TelegramState, route: &TelegramRoute) -> String {
    for index in 1.. {
        let alias = format!("session-{index}");
        if state.binding_for_route(route, Some(&alias)).is_none() {
            return alias;
        }
    }
    unreachable!("unbounded alias search should always find a free alias")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TelegramTextCommand {
    Start,
    Bind { secret: Option<String> },
    Portal,
    Attach { thread_id: Option<String> },
    Takeover,
    Watch,
    New { alias: Option<String> },
    Use { alias: Option<String> },
    Status,
    Sessions,
    Close { alias: Option<String> },
    Release,
    Message,
}

impl TelegramTextCommand {
    fn parse(text: &str) -> Self {
        let mut parts = text.split_whitespace();
        let command = parts.next().unwrap_or_default();
        let command = command.split('@').next().unwrap_or(command);
        match command {
            "/start" => Self::Start,
            "/bind" => Self::Bind {
                secret: parts.next().map(String::from),
            },
            "/portal" | "/history" => Self::Portal,
            "/attach" => Self::Attach {
                thread_id: parts.next().map(String::from),
            },
            "/takeover" | "/take" => Self::Takeover,
            "/watch" | "/observe" => Self::Watch,
            "/new" => Self::New {
                alias: parts.next().map(String::from),
            },
            "/use" => Self::Use {
                alias: parts.next().map(String::from),
            },
            "/status" => Self::Status,
            "/sessions" => Self::Sessions,
            "/close" => Self::Close {
                alias: parts.next().map(String::from),
            },
            "/release" => Self::Release,
            _ => Self::Message,
        }
    }
}

fn log_update_summary(
    update_id: i64,
    view: &TelegramUpdateView,
    allowed: &BTreeSet<TelegramRoute>,
) {
    let chat_id = view
        .chat_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| String::from("<none>"));
    let text = view
        .message
        .as_ref()
        .and_then(|message| message.text.as_ref())
        .map(|text| {
            if text.trim().is_empty() {
                "empty"
            } else {
                "present"
            }
        })
        .unwrap_or("none");
    let route = view.message.as_ref().map(TelegramRoute::from_message);
    let allowed = route
        .as_ref()
        .map(|route| allowed.contains(route).to_string())
        .unwrap_or_else(|| String::from("false"));
    let route = route
        .map(|route| route.display())
        .unwrap_or_else(|| String::from("<none>"));
    eprintln!(
        "telegram update update_id={} source={} chat_id={} route={} text={} allowed={}",
        update_id,
        view.source.as_str(),
        chat_id,
        route,
        text,
        allowed
    );
}

fn elapsed_ms(start: Instant) -> u128 {
    start.elapsed().as_millis()
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn telegram_age_ms(message_date_secs: Option<i64>) -> Option<i128> {
    let message_date_secs = message_date_secs?;
    let now_ms = unix_millis() as i128;
    Some(now_ms - i128::from(message_date_secs) * 1000)
}
