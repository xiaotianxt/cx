pub mod telegram {
    use std::collections::BTreeSet;
    use std::fs;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::io::Write;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use anyhow::Context;
    use anyhow::Result;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use reqwest::blocking::Client;
    use serde::Deserialize;
    use serde::Serialize;

    use crate::app_server::AppServerClient;
    use crate::cli::TelegramBindArgs;
    use crate::cli::TelegramMenuArgs;
    use crate::cli::TelegramRunArgs;
    use crate::cli::TelegramStatusArgs;
    use crate::paths::ManagerPaths;
    use crate::serve;
    use crate::session;
    use crate::session::AcquireLeaseRequest;
    use crate::session::ChannelId;
    use crate::session::CreateSessionRequest;
    use crate::session::RecordChannelMessageRequest;
    use crate::session::SessionId;

    const TELEGRAM_STATE_SCHEMA_VERSION: u64 = 1;

    #[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(rename_all = "camelCase")]
    struct TelegramState {
        schema_version: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_update_id: Option<i64>,
        bindings: Vec<TelegramBinding>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        active_routes: Vec<TelegramActiveRoute>,
    }

    #[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(rename_all = "camelCase")]
    struct TelegramBinding {
        chat_id: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_thread_id: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<String>,
        channel_id: ChannelId,
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app_thread_id: Option<String>,
        #[serde(default)]
        topic_created_by_adapter: bool,
    }

    #[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(rename_all = "camelCase")]
    struct TelegramActiveRoute {
        chat_id: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_thread_id: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    struct TelegramRoute {
        chat_id: i64,
        message_thread_id: Option<i64>,
    }

    #[derive(Debug, Deserialize)]
    struct TelegramResponse<T> {
        ok: bool,
        result: Option<T>,
        description: Option<String>,
    }

    #[derive(Debug, Serialize)]
    struct TelegramBotCommand {
        command: &'static str,
        description: &'static str,
    }

    #[derive(Debug, Deserialize)]
    struct TelegramUpdate {
        update_id: i64,
        message: Option<TelegramMessage>,
        edited_message: Option<TelegramMessage>,
        channel_post: Option<TelegramMessage>,
        edited_channel_post: Option<TelegramMessage>,
        callback_query: Option<TelegramCallbackQuery>,
        my_chat_member: Option<TelegramChatMemberUpdated>,
    }

    #[derive(Debug, Clone, Deserialize)]
    struct TelegramMessage {
        chat: TelegramChat,
        message_id: i64,
        message_thread_id: Option<i64>,
        text: Option<String>,
    }

    #[derive(Debug, Clone, Deserialize)]
    struct TelegramChat {
        id: i64,
        #[serde(default)]
        is_forum: bool,
    }

    #[derive(Debug, Deserialize)]
    struct TelegramCallbackQuery {
        message: Option<TelegramMessage>,
    }

    #[derive(Debug, Deserialize)]
    struct TelegramChatMemberUpdated {
        chat: TelegramChat,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TelegramUpdateSource {
        Message,
        EditedMessage,
        ChannelPost,
        EditedChannelPost,
        CallbackQuery,
        MyChatMember,
        Unknown,
    }

    #[derive(Debug)]
    struct TelegramUpdateView {
        source: TelegramUpdateSource,
        chat_id: Option<i64>,
        message: Option<TelegramMessage>,
    }

    impl TelegramUpdate {
        fn view(self) -> TelegramUpdateView {
            if let Some(message) = self.message {
                return TelegramUpdateView::message(TelegramUpdateSource::Message, message);
            }
            if let Some(message) = self.edited_message {
                return TelegramUpdateView::message(TelegramUpdateSource::EditedMessage, message);
            }
            if let Some(message) = self.channel_post {
                return TelegramUpdateView::message(TelegramUpdateSource::ChannelPost, message);
            }
            if let Some(message) = self.edited_channel_post {
                return TelegramUpdateView::message(
                    TelegramUpdateSource::EditedChannelPost,
                    message,
                );
            }
            if let Some(callback_query) = self.callback_query {
                return match callback_query.message {
                    Some(message) => {
                        TelegramUpdateView::message(TelegramUpdateSource::CallbackQuery, message)
                    }
                    None => TelegramUpdateView {
                        source: TelegramUpdateSource::CallbackQuery,
                        chat_id: None,
                        message: None,
                    },
                };
            }
            if let Some(my_chat_member) = self.my_chat_member {
                return TelegramUpdateView {
                    source: TelegramUpdateSource::MyChatMember,
                    chat_id: Some(my_chat_member.chat.id),
                    message: None,
                };
            }
            TelegramUpdateView {
                source: TelegramUpdateSource::Unknown,
                chat_id: None,
                message: None,
            }
        }
    }

    impl TelegramUpdateView {
        fn message(source: TelegramUpdateSource, message: TelegramMessage) -> Self {
            Self {
                source,
                chat_id: Some(message.chat.id),
                message: Some(message),
            }
        }
    }

    impl TelegramUpdateSource {
        fn as_str(self) -> &'static str {
            match self {
                Self::Message => "message",
                Self::EditedMessage => "edited_message",
                Self::ChannelPost => "channel_post",
                Self::EditedChannelPost => "edited_channel_post",
                Self::CallbackQuery => "callback_query",
                Self::MyChatMember => "my_chat_member",
                Self::Unknown => "unknown",
            }
        }
    }

    impl TelegramState {
        fn empty() -> Self {
            Self {
                schema_version: TELEGRAM_STATE_SCHEMA_VERSION,
                last_update_id: None,
                bindings: Vec::new(),
                active_routes: Vec::new(),
            }
        }

        fn binding_for_route(
            &self,
            route: &TelegramRoute,
            alias: Option<&str>,
        ) -> Option<&TelegramBinding> {
            self.bindings.iter().find(|binding| {
                binding.chat_id == route.chat_id
                    && binding.message_thread_id == route.message_thread_id
                    && binding.alias.as_deref() == alias
            })
        }

        fn active_binding_for_route(&self, route: &TelegramRoute) -> Option<&TelegramBinding> {
            let active_alias = self.active_alias_for_route(route);
            self.binding_for_route(route, active_alias)
                .or_else(|| self.binding_for_route(route, None))
        }

        fn active_alias_for_route(&self, route: &TelegramRoute) -> Option<&str> {
            self.active_routes
                .iter()
                .find(|active| {
                    active.chat_id == route.chat_id
                        && active.message_thread_id == route.message_thread_id
                })
                .and_then(|active| active.alias.as_deref())
        }

        fn binding_for_route_mut(
            &mut self,
            route: &TelegramRoute,
            alias: Option<&str>,
        ) -> Option<&mut TelegramBinding> {
            self.bindings.iter_mut().find(|binding| {
                binding.chat_id == route.chat_id
                    && binding.message_thread_id == route.message_thread_id
                    && binding.alias.as_deref() == alias
            })
        }

        fn bind_route(
            &mut self,
            paths: &ManagerPaths,
            route: &TelegramRoute,
            alias: Option<&str>,
        ) -> Result<TelegramBinding> {
            if let Some(binding) = self.binding_for_route(route, alias) {
                return Ok(binding.clone());
            }
            let channel_id = route.channel_id(alias)?;
            let result = session::create_session(
                paths,
                CreateSessionRequest {
                    session_id: None,
                    channel_id: channel_id.clone(),
                },
            )?;
            let binding = TelegramBinding {
                chat_id: route.chat_id,
                message_thread_id: route.message_thread_id,
                alias: alias.map(str::to_string),
                channel_id,
                session_id: result.session.session_id,
                app_thread_id: None,
                topic_created_by_adapter: false,
            };
            self.bindings.push(binding.clone());
            self.set_active_route(route, alias);
            Ok(binding)
        }

        fn set_active_route(&mut self, route: &TelegramRoute, alias: Option<&str>) {
            if let Some(active) = self.active_routes.iter_mut().find(|active| {
                active.chat_id == route.chat_id
                    && active.message_thread_id == route.message_thread_id
            }) {
                active.alias = alias.map(str::to_string);
                return;
            }
            self.active_routes.push(TelegramActiveRoute {
                chat_id: route.chat_id,
                message_thread_id: route.message_thread_id,
                alias: alias.map(str::to_string),
            });
        }

        fn remove_route_binding(&mut self, route: &TelegramRoute, alias: Option<&str>) -> bool {
            let original_len = self.bindings.len();
            self.bindings.retain(|binding| {
                !(binding.chat_id == route.chat_id
                    && binding.message_thread_id == route.message_thread_id
                    && binding.alias.as_deref() == alias)
            });
            if original_len != self.bindings.len() {
                self.active_routes.retain(|active| {
                    !(active.chat_id == route.chat_id
                        && active.message_thread_id == route.message_thread_id
                        && active.alias.as_deref() == alias)
                });
                return true;
            }
            false
        }
    }

    impl TelegramRoute {
        fn from_message(message: &TelegramMessage) -> Self {
            Self {
                chat_id: message.chat.id,
                message_thread_id: message.message_thread_id,
            }
        }

        fn channel_id(&self, alias: Option<&str>) -> Result<ChannelId> {
            let mut raw = format!("telegram:{}", self.chat_id);
            if let Some(thread_id) = self.message_thread_id {
                raw.push_str(&format!(":topic:{thread_id}"));
            }
            if let Some(alias) = alias {
                raw.push_str(":session:");
                raw.push_str(alias);
            }
            ChannelId::parse(raw)
        }

        fn display(&self) -> String {
            match self.message_thread_id {
                Some(thread_id) => format!("{} topic {}", self.chat_id, thread_id),
                None => self.chat_id.to_string(),
            }
        }
    }

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
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    binding.chat_id,
                    binding.message_thread_id.unwrap_or_default(),
                    binding.alias.as_deref().unwrap_or("default"),
                    binding.channel_id,
                    binding.session_id,
                    app_thread
                );
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
        let updates = get_updates(
            client,
            token,
            state.last_update_id.map(|id| id + 1),
            options.poll_timeout,
        )?;
        for update in updates {
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
                let route = TelegramRoute::from_message(&message);
                trusted.insert(route.clone());
                bound_route = Some(route);
            }
            let notifier = TelegramNotifier { client, token };
            notifier.ack_seen(&message);
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
                },
            )?;
            write_state(paths, state)?;
            if let Some(reply) = reply {
                send_reply(
                    client,
                    token,
                    reply.chat_id,
                    reply.message_thread_id,
                    &reply.text,
                )?;
            }
        }
        write_state(paths, state)?;
        Ok(PollOutcome { bound_route })
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

    fn read_token_env(env_name: &str) -> Result<String> {
        let token = std::env::var(env_name).with_context(|| format!("{env_name} is not set"))?;
        if token.trim().is_empty() {
            anyhow::bail!("{env_name} is empty");
        }
        Ok(token)
    }

    fn trusted_chats(state: &TelegramState, allow_chats: Vec<i64>) -> BTreeSet<TelegramRoute> {
        state
            .bindings
            .iter()
            .map(|binding| TelegramRoute {
                chat_id: binding.chat_id,
                message_thread_id: binding.message_thread_id,
            })
            .chain(allow_chats.into_iter().map(|chat_id| TelegramRoute {
                chat_id,
                message_thread_id: None,
            }))
            .collect()
    }

    fn generate_bind_secret() -> Result<String> {
        let mut bytes = [0_u8; 16];
        let mut file = fs::File::open("/dev/urandom").context("open /dev/urandom")?;
        file.read_exact(&mut bytes).context("read /dev/urandom")?;
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TelegramReply {
        chat_id: i64,
        message_thread_id: Option<i64>,
        text: String,
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
        if trust_existing && (route_is_trusted(&route, allowed) || route_has_binding(&route, state))
        {
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
        let text = message.text.unwrap_or_default();
        let command = TelegramTextCommand::parse(&text);

        match command {
            TelegramTextCommand::Status => {
                let Some(binding) = state.active_binding_for_route(&route) else {
                    return Ok(Some(reply(&route, no_session_bound_message(state, &route))));
                };
                let session = session::show_session(paths, &binding.session_id)?;
                Ok(Some(reply(
                    &route,
                    format!(
                        "session: {}\nalias: {}\nroute: {}\nchannel: {}\nlease_epoch: {}\nactive_lease: {}",
                        session.session_id,
                        binding.alias.as_deref().unwrap_or("default"),
                        route.display(),
                        session.current_channel_id,
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
            TelegramTextCommand::Release => {
                let Some(binding) = state.active_binding_for_route(&route) else {
                    return Ok(Some(reply(&route, no_session_bound_message(state, &route))));
                };
                let session = session::show_session(paths, &binding.session_id)?;
                let Some(active_lease) = session.active_lease else {
                    return Ok(Some(reply(&route, "This session has no active lease.")));
                };
                if active_lease.channel_id != binding.channel_id {
                    return Ok(Some(reply(
                        &route,
                        format!("Lease is held by {}.", active_lease.channel_id),
                    )));
                }
                session::release_lease(
                    paths,
                    session::ReleaseLeaseRequest {
                        session_id: binding.session_id.clone(),
                        lease_token: active_lease.lease_token,
                    },
                )?;
                Ok(Some(reply(&route, "Released Telegram lease.")))
            }
            TelegramTextCommand::Bind { .. } if !options.authorized_by_bind => {
                Ok(Some(reply(&route, "Invalid or disabled bind secret.")))
            }
            TelegramTextCommand::Start | TelegramTextCommand::Bind { .. } => {
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
                        format!(
                            "Session `{alias}` already exists. Use /use {alias} to switch to it."
                        ),
                    )));
                }
                let effective_route = if route.message_thread_id.is_none() && message.chat.is_forum
                {
                    if let Some(notifier) = options.notifier {
                        match create_forum_topic(
                            notifier.client,
                            notifier.token,
                            route.chat_id,
                            &alias,
                        ) {
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
                            let _ = close_forum_topic(
                                notifier.client,
                                notifier.token,
                                effective_route.chat_id,
                                topic_id,
                            );
                        }
                        return Err(err);
                    }
                };
                if effective_route.message_thread_id.is_some() && route.message_thread_id.is_none()
                {
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
                if let Some(binding) = state.binding_for_route(&route, alias.as_deref()) {
                    if let (Some(thread_id), Some(notifier)) =
                        (binding.message_thread_id, options.notifier)
                    {
                        if binding.topic_created_by_adapter {
                            if let Err(err) = close_forum_topic(
                                notifier.client,
                                notifier.token,
                                route.chat_id,
                                thread_id,
                            ) {
                                eprintln!("telegram closeForumTopic failed: {err:#}");
                            }
                        }
                    }
                }
                if state.remove_route_binding(&route, alias.as_deref()) {
                    Ok(Some(reply(
                        &route,
                        format!(
                            "Unbound session `{}`.",
                            alias.as_deref().unwrap_or("default")
                        ),
                    )))
                } else {
                    Ok(Some(reply(
                        &route,
                        missing_session_message(state, &route, alias.as_deref()),
                    )))
                }
            }
            TelegramTextCommand::Message => {
                if text.trim().is_empty() {
                    return Ok(Some(reply(
                        &route,
                        "Send a message for Codex to answer, or use /new <name>, /use <name>, /sessions, /status, /close.",
                    )));
                }
                let binding = state
                    .active_binding_for_route(&route)
                    .cloned()
                    .map(Ok)
                    .unwrap_or_else(|| state.bind_route(paths, &route, None))?;
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
                    options.app_server_timeout,
                    options.notifier,
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

    struct HandleOptions<'a> {
        notifier: Option<&'a TelegramNotifier<'a>>,
        acquire_lease: bool,
        steal: bool,
        authorized_by_bind: bool,
        app_server_timeout: f32,
    }

    fn run_codex_turn(
        paths: &ManagerPaths,
        state: &mut TelegramState,
        route: &TelegramRoute,
        alias: Option<&str>,
        prompt: String,
        timeout_secs: f32,
        notifier: Option<&TelegramNotifier<'_>>,
    ) -> Result<CodexTurnOutput> {
        let server = serve::ready_app_server(paths)?;
        let mut client =
            AppServerClient::connect(&server.listen_url, Duration::from_secs_f32(timeout_secs))?;
        client.initialize("cx-telegram", env!("CARGO_PKG_VERSION"))?;

        let thread_id = match state
            .binding_for_route(route, alias)
            .and_then(|binding| binding.app_thread_id.clone())
        {
            Some(thread_id) => thread_id,
            None => {
                let thread = client.thread_start()?;
                let binding = state
                    .binding_for_route_mut(route, alias)
                    .with_context(|| format!("Telegram route {} is not bound", route.display()))?;
                binding.app_thread_id = Some(thread.upstream_thread_id.clone());
                thread.upstream_thread_id
            }
        };
        let mut sink = notifier.map(|notifier| TelegramDeltaSink::new(notifier, route.clone()));
        let turn = client.turn_start_stream(&thread_id, prompt, |delta| {
            if let Some(sink) = sink.as_mut() {
                sink.push(delta)?;
            }
            Ok(())
        })?;
        if let Some(sink) = sink.as_mut() {
            sink.finish()?;
        }
        Ok(CodexTurnOutput {
            assistant_text: turn.assistant_text,
            streamed_to_telegram: sink.is_some_and(|sink| sink.sent_any()),
        })
    }

    struct CodexTurnOutput {
        assistant_text: String,
        streamed_to_telegram: bool,
    }

    struct TelegramNotifier<'a> {
        client: &'a Client,
        token: &'a str,
    }

    impl TelegramNotifier<'_> {
        fn ack_seen(&self, message: &TelegramMessage) {
            if let Err(err) = set_message_reaction(
                self.client,
                self.token,
                message.chat.id,
                message.message_id,
                "\u{1f440}",
            ) {
                eprintln!("telegram setMessageReaction failed: {err:#}");
            }
        }

        fn typing(&self, route: &TelegramRoute) -> TelegramTypingGuard {
            TelegramTypingGuard::start(self.client.clone(), self.token.to_string(), route.clone())
        }

        fn send(&self, route: &TelegramRoute, text: &str) -> Result<()> {
            send_reply(
                self.client,
                self.token,
                route.chat_id,
                route.message_thread_id,
                text,
            )
        }
    }

    struct TelegramTypingGuard {
        stop: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl TelegramTypingGuard {
        fn start(client: Client, token: String, route: TelegramRoute) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = Arc::clone(&stop);
            let handle = thread::spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    if let Err(err) =
                        send_chat_action(&client, &token, route.chat_id, route.message_thread_id)
                    {
                        eprintln!("telegram sendChatAction failed: {err:#}");
                    }
                    for _ in 0..8 {
                        if worker_stop.load(Ordering::Relaxed) {
                            return;
                        }
                        thread::sleep(Duration::from_millis(500));
                    }
                }
            });
            Self {
                stop,
                handle: Some(handle),
            }
        }
    }

    impl Drop for TelegramTypingGuard {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    struct TelegramDeltaSink<'a> {
        notifier: &'a TelegramNotifier<'a>,
        route: TelegramRoute,
        buffer: String,
        sent_any: bool,
    }

    impl<'a> TelegramDeltaSink<'a> {
        fn new(notifier: &'a TelegramNotifier<'a>, route: TelegramRoute) -> Self {
            Self {
                notifier,
                route,
                buffer: String::new(),
                sent_any: false,
            }
        }

        fn push(&mut self, delta: &str) -> Result<()> {
            self.buffer.push_str(delta);
            if self.buffer.chars().count() >= 900 || delta.contains('\n') {
                self.flush()?;
            }
            Ok(())
        }

        fn finish(&mut self) -> Result<()> {
            self.flush()
        }

        fn sent_any(&self) -> bool {
            self.sent_any
        }

        fn flush(&mut self) -> Result<()> {
            if self.buffer.trim().is_empty() {
                self.buffer.clear();
                return Ok(());
            }
            self.notifier.send(&self.route, &self.buffer)?;
            self.buffer.clear();
            self.sent_any = true;
            Ok(())
        }
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
                binding.chat_id == route.chat_id
                    && binding.message_thread_id == route.message_thread_id
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

    fn reply(route: &TelegramRoute, text: impl Into<String>) -> TelegramReply {
        TelegramReply {
            chat_id: route.chat_id,
            message_thread_id: route.message_thread_id,
            text: text.into(),
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

    fn get_updates(
        client: &Client,
        token: &str,
        offset: Option<i64>,
        timeout: u64,
    ) -> Result<Vec<TelegramUpdate>> {
        let url = telegram_method_url(token, "getUpdates");
        let timeout_string = timeout.to_string();
        let mut request = client
            .get(url)
            .query(&[("timeout", timeout_string.as_str())]);
        let offset_string;
        if let Some(offset) = offset {
            offset_string = offset.to_string();
            request = request.query(&[("offset", offset_string.as_str())]);
        }
        let response = request.send().map_err(|err| {
            anyhow::anyhow!("Telegram getUpdates request failed: {}", err.without_url())
        })?;
        let response = response
            .json::<TelegramResponse<Vec<TelegramUpdate>>>()
            .map_err(|err| {
                anyhow::anyhow!(
                    "decode Telegram getUpdates response failed: {}",
                    err.without_url()
                )
            })?;
        if !response.ok {
            anyhow::bail!(
                "Telegram getUpdates failed: {}",
                response
                    .description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }
        Ok(response.result.unwrap_or_default())
    }

    #[derive(Debug, Serialize)]
    struct TelegramSendMessage<'a> {
        chat_id: &'a str,
        text: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        message_thread_id: Option<String>,
    }

    fn send_message(
        client: &Client,
        token: &str,
        chat_id: i64,
        message_thread_id: Option<i64>,
        text: &str,
    ) -> Result<()> {
        let url = telegram_method_url(token, "sendMessage");
        let chat_id = chat_id.to_string();
        let message_thread_id = message_thread_id.map(|thread_id| thread_id.to_string());
        let payload = TelegramSendMessage {
            chat_id: chat_id.as_str(),
            text,
            message_thread_id,
        };
        let response = client.post(url).form(&payload).send().map_err(|err| {
            anyhow::anyhow!("Telegram sendMessage request failed: {}", err.without_url())
        })?;
        let response = response
            .json::<TelegramResponse<serde_json::Value>>()
            .map_err(|err| {
                anyhow::anyhow!(
                    "decode Telegram sendMessage response failed: {}",
                    err.without_url()
                )
            })?;
        if !response.ok {
            anyhow::bail!(
                "Telegram sendMessage failed: {}",
                response
                    .description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }
        Ok(())
    }

    #[derive(Debug, Serialize)]
    struct TelegramForumTopicParams<'a> {
        chat_id: &'a str,
        name: &'a str,
    }

    fn create_forum_topic(client: &Client, token: &str, chat_id: i64, name: &str) -> Result<i64> {
        let url = telegram_method_url(token, "createForumTopic");
        let chat_id = chat_id.to_string();
        let payload = TelegramForumTopicParams {
            chat_id: chat_id.as_str(),
            name,
        };
        let response = client.post(url).form(&payload).send().map_err(|err| {
            anyhow::anyhow!(
                "Telegram createForumTopic request failed: {}",
                err.without_url()
            )
        })?;
        let response = response
            .json::<TelegramResponse<serde_json::Value>>()
            .map_err(|err| {
                anyhow::anyhow!(
                    "decode Telegram createForumTopic response failed: {}",
                    err.without_url()
                )
            })?;
        if !response.ok {
            anyhow::bail!(
                "Telegram createForumTopic failed: {}",
                response
                    .description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }
        let topic_id = response
            .result
            .and_then(|r| r.get("message_thread_id")?.as_i64())
            .ok_or_else(|| {
                anyhow::anyhow!("Telegram createForumTopic: no message_thread_id in response")
            })?;
        Ok(topic_id)
    }

    #[derive(Debug, Serialize)]
    struct TelegramCloseForumTopicParams<'a> {
        chat_id: &'a str,
        message_thread_id: &'a str,
    }

    fn close_forum_topic(
        client: &Client,
        token: &str,
        chat_id: i64,
        message_thread_id: i64,
    ) -> Result<()> {
        let url = telegram_method_url(token, "closeForumTopic");
        let chat_id = chat_id.to_string();
        let message_thread_id = message_thread_id.to_string();
        let payload = TelegramCloseForumTopicParams {
            chat_id: chat_id.as_str(),
            message_thread_id: message_thread_id.as_str(),
        };
        let response = client.post(url).form(&payload).send().map_err(|err| {
            anyhow::anyhow!(
                "Telegram closeForumTopic request failed: {}",
                err.without_url()
            )
        })?;
        let response = response
            .json::<TelegramResponse<serde_json::Value>>()
            .map_err(|err| {
                anyhow::anyhow!(
                    "decode Telegram closeForumTopic response failed: {}",
                    err.without_url()
                )
            })?;
        if !response.ok {
            anyhow::bail!(
                "Telegram closeForumTopic failed: {}",
                response
                    .description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }
        Ok(())
    }

    #[derive(Debug, Serialize)]
    struct TelegramSendChatAction<'a> {
        chat_id: &'a str,
        action: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        message_thread_id: Option<String>,
    }

    fn send_chat_action(
        client: &Client,
        token: &str,
        chat_id: i64,
        message_thread_id: Option<i64>,
    ) -> Result<()> {
        let url = telegram_method_url(token, "sendChatAction");
        let chat_id = chat_id.to_string();
        let message_thread_id = message_thread_id.map(|thread_id| thread_id.to_string());
        let payload = TelegramSendChatAction {
            chat_id: chat_id.as_str(),
            action: "typing",
            message_thread_id,
        };
        let response = client.post(url).form(&payload).send().map_err(|err| {
            anyhow::anyhow!(
                "Telegram sendChatAction request failed: {}",
                err.without_url()
            )
        })?;
        let response = response
            .json::<TelegramResponse<serde_json::Value>>()
            .map_err(|err| {
                anyhow::anyhow!(
                    "decode Telegram sendChatAction response failed: {}",
                    err.without_url()
                )
            })?;
        if !response.ok {
            anyhow::bail!(
                "Telegram sendChatAction failed: {}",
                response
                    .description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }
        Ok(())
    }

    #[derive(Debug, Serialize)]
    struct TelegramReaction<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        emoji: &'a str,
    }

    #[derive(Debug, Serialize)]
    struct TelegramSetMessageReaction<'a> {
        chat_id: &'a str,
        message_id: &'a str,
        reaction: &'a str,
    }

    fn set_message_reaction(
        client: &Client,
        token: &str,
        chat_id: i64,
        message_id: i64,
        emoji: &str,
    ) -> Result<()> {
        let url = telegram_method_url(token, "setMessageReaction");
        let chat_id = chat_id.to_string();
        let message_id = message_id.to_string();
        let reaction = serde_json::to_string(&[TelegramReaction {
            kind: "emoji",
            emoji,
        }])
        .context("serialize Telegram reaction")?;
        let payload = TelegramSetMessageReaction {
            chat_id: chat_id.as_str(),
            message_id: message_id.as_str(),
            reaction: reaction.as_str(),
        };
        let response = client.post(url).form(&payload).send().map_err(|err| {
            anyhow::anyhow!(
                "Telegram setMessageReaction request failed: {}",
                err.without_url()
            )
        })?;
        let response = response
            .json::<TelegramResponse<serde_json::Value>>()
            .map_err(|err| {
                anyhow::anyhow!(
                    "decode Telegram setMessageReaction response failed: {}",
                    err.without_url()
                )
            })?;
        if !response.ok {
            anyhow::bail!(
                "Telegram setMessageReaction failed: {}",
                response
                    .description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }
        Ok(())
    }

    fn sync_bot_commands(client: &Client, token: &str) -> Result<()> {
        let url = telegram_method_url(token, "setMyCommands");
        let commands = serde_json::to_string(bot_commands()).context("serialize bot commands")?;
        let response = client
            .post(url)
            .form(&[("commands", commands.as_str())])
            .send()
            .map_err(|err| {
                anyhow::anyhow!(
                    "Telegram setMyCommands request failed: {}",
                    err.without_url()
                )
            })?;
        let response = response
            .json::<TelegramResponse<serde_json::Value>>()
            .map_err(|err| {
                anyhow::anyhow!(
                    "decode Telegram setMyCommands response failed: {}",
                    err.without_url()
                )
            })?;
        if !response.ok {
            anyhow::bail!(
                "Telegram setMyCommands failed: {}",
                response
                    .description
                    .unwrap_or_else(|| "unknown error".to_string())
            );
        }
        Ok(())
    }

    fn bot_commands() -> &'static [TelegramBotCommand] {
        &[
            TelegramBotCommand {
                command: "start",
                description: "Bind or show the current cx session",
            },
            TelegramBotCommand {
                command: "bind",
                description: "Trust this chat with a one-time secret",
            },
            TelegramBotCommand {
                command: "new",
                description: "Create and switch to a named session",
            },
            TelegramBotCommand {
                command: "use",
                description: "Switch to a named session",
            },
            TelegramBotCommand {
                command: "status",
                description: "Show the bound cx session and lease",
            },
            TelegramBotCommand {
                command: "sessions",
                description: "List Telegram sessions for this chat",
            },
            TelegramBotCommand {
                command: "close",
                description: "Unbind a named session from this route",
            },
            TelegramBotCommand {
                command: "release",
                description: "Release this chat's active lease",
            },
        ]
    }

    fn send_reply(
        client: &Client,
        token: &str,
        chat_id: i64,
        message_thread_id: Option<i64>,
        text: &str,
    ) -> Result<()> {
        for chunk in telegram_text_chunks(text) {
            send_message(client, token, chat_id, message_thread_id, &chunk)?;
        }
        Ok(())
    }

    fn telegram_text_chunks(text: &str) -> Vec<String> {
        const MAX_CHARS: usize = 3900;
        if text.chars().count() <= MAX_CHARS {
            return vec![text.to_string()];
        }
        let mut chunks = Vec::new();
        let mut current = String::new();
        for ch in text.chars() {
            if current.chars().count() >= MAX_CHARS {
                chunks.push(current);
                current = String::new();
            }
            current.push(ch);
        }
        if !current.is_empty() {
            chunks.push(current);
        }
        chunks
    }

    fn telegram_method_url(token: &str, method: &str) -> String {
        format!("https://api.telegram.org/bot{token}/{method}")
    }

    fn read_state(paths: &ManagerPaths) -> Result<TelegramState> {
        let path = paths.telegram_channel_state_file();
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TelegramState::empty());
            }
            Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
        };
        let state = serde_json::from_str::<TelegramState>(&content)
            .with_context(|| format!("parse {}", path.display()))?;
        if state.schema_version != TELEGRAM_STATE_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported Telegram state schema version: {}",
                state.schema_version
            );
        }
        Ok(state)
    }

    fn write_state(paths: &ManagerPaths, state: &TelegramState) -> Result<()> {
        fs::create_dir_all(paths.serve_channels_dir())
            .with_context(|| format!("create {}", paths.serve_channels_dir().display()))?;
        set_private_dir_permissions(&paths.serve_dir())?;
        set_private_dir_permissions(&paths.serve_channels_dir())?;

        let path = paths.telegram_channel_state_file();
        let tmp_path = paths.serve_channels_dir().join("telegram.json.tmp");
        let content = serde_json::to_vec_pretty(state).context("serialize Telegram state")?;
        let mut file = private_open_for_write(&tmp_path)?;
        file.write_all(&content)
            .with_context(|| format!("write {}", tmp_path.display()))?;
        file.write_all(b"\n")
            .with_context(|| format!("write {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &path)
            .with_context(|| format!("rename {} to {}", tmp_path.display(), path.display()))?;
        set_private_file_permissions(&path)?;
        Ok(())
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
                "cx-telegram-test-{name}-{}-{unique}",
                std::process::id()
            ));
            ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
        }

        fn text_message(
            chat_id: i64,
            message_thread_id: Option<i64>,
            text: &str,
        ) -> TelegramMessage {
            TelegramMessage {
                chat: TelegramChat {
                    id: chat_id,
                    is_forum: false,
                },
                message_id: 1,
                message_thread_id,
                text: Some(text.to_string()),
            }
        }

        fn handle_options() -> HandleOptions<'static> {
            HandleOptions {
                notifier: None,
                acquire_lease: false,
                steal: false,
                authorized_by_bind: false,
                app_server_timeout: 600.0,
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
                topic_created_by_adapter: false,
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
        fn existing_binding_is_trusted_without_allow_chat() {
            let mut state = TelegramState::empty();
            state.bindings.push(TelegramBinding {
                chat_id: 42,
                message_thread_id: None,
                alias: None,
                channel_id: ChannelId::parse("telegram:42").unwrap(),
                session_id: SessionId::parse("sess_manual").unwrap(),
                app_thread_id: None,
                topic_created_by_adapter: false,
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
                topic_created_by_adapter: false,
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

            assert_eq!(
                commands,
                vec!["start", "bind", "new", "use", "status", "sessions", "close", "release"]
            );
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
                    message_thread_id: Some(7),
                    text: Some(String::from("/start")),
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
                message_thread_id: None,
                text: Some("/new Build!".to_string()),
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
                message_thread_id: Some(99),
                text: Some("/new config".to_string()),
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
                    message_thread_id: Some(99),
                    text: Some("/close smoke".to_string()),
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
                    message_thread_id: Some(99),
                    text: Some("/close".to_string()),
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
                    message_thread_id: Some(31),
                    text: Some("/close".to_string()),
                },
                handle_options(),
            )
            .unwrap()
            .unwrap();

            assert_eq!(reply.text, "Unbound session `session-2`.");
            assert!(state.binding_for_route(&route, Some("session-2")).is_none());

            let _ = fs::remove_dir_all(paths.serve_dir());
        }
    }
}
