pub mod telegram {
    use std::collections::BTreeSet;
    use std::fs;
    use std::fs::OpenOptions;
    use std::io::Read;
    use std::io::Write;
    use std::path::Path;
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
    }

    #[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(rename_all = "camelCase")]
    struct TelegramBinding {
        chat_id: i64,
        channel_id: ChannelId,
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app_thread_id: Option<String>,
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
        text: Option<String>,
    }

    #[derive(Debug, Clone, Deserialize)]
    struct TelegramChat {
        id: i64,
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
            }
        }

        fn binding_for_chat(&self, chat_id: i64) -> Option<&TelegramBinding> {
            self.bindings
                .iter()
                .find(|binding| binding.chat_id == chat_id)
        }

        fn binding_for_chat_mut(&mut self, chat_id: i64) -> Option<&mut TelegramBinding> {
            self.bindings
                .iter_mut()
                .find(|binding| binding.chat_id == chat_id)
        }

        fn bind_chat(
            &mut self,
            paths: &ManagerPaths,
            chat_id: i64,
            channel_id: ChannelId,
        ) -> Result<TelegramBinding> {
            if let Some(binding) = self.binding_for_chat(chat_id) {
                return Ok(binding.clone());
            }
            let result = session::create_session(
                paths,
                CreateSessionRequest {
                    session_id: None,
                    channel_id: channel_id.clone(),
                },
            )?;
            let binding = TelegramBinding {
                chat_id,
                channel_id,
                session_id: result.session.session_id,
                app_thread_id: None,
            };
            self.bindings.push(binding.clone());
            Ok(binding)
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
            if bind_secret.is_some() && outcome.bound_chat.is_some() {
                println!("telegram chat bound; continuing adapter run");
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
            if let Some(chat_id) = outcome.bound_chat {
                println!("trusted Telegram chat {chat_id}");
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
                    "{}\t{}\t{}\t{}",
                    binding.chat_id, binding.channel_id, binding.session_id, app_thread
                );
            }
        }
        Ok(())
    }

    struct PollOutcome {
        bound_chat: Option<i64>,
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
        trusted: &mut BTreeSet<i64>,
        options: PollOptions<'_>,
    ) -> Result<PollOutcome> {
        let mut bound_chat = None;
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
                trusted.insert(message.chat.id);
                bound_chat = Some(message.chat.id);
            }
            let reply = handle_message(
                paths,
                state,
                message,
                options.acquire_lease,
                options.steal,
                matches!(access, MessageAccess::AuthorizedByBind),
                options.app_server_timeout,
            )?;
            write_state(paths, state)?;
            if let Some(reply) = reply {
                send_reply(client, token, reply.chat_id, &reply.text)?;
            }
        }
        write_state(paths, state)?;
        Ok(PollOutcome { bound_chat })
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

    fn trusted_chats(state: &TelegramState, allow_chats: Vec<i64>) -> BTreeSet<i64> {
        state
            .bindings
            .iter()
            .map(|binding| binding.chat_id)
            .chain(allow_chats)
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
        allowed: &BTreeSet<i64>,
        bind_secret: Option<&str>,
        trust_existing: bool,
    ) -> MessageAccess {
        let chat_id = message.chat.id;
        if trust_existing
            && (allowed.contains(&chat_id) || state.binding_for_chat(chat_id).is_some())
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

    fn handle_message(
        paths: &ManagerPaths,
        state: &mut TelegramState,
        message: TelegramMessage,
        acquire_lease: bool,
        steal: bool,
        authorized_by_bind: bool,
        app_server_timeout: f32,
    ) -> Result<Option<TelegramReply>> {
        let chat_id = message.chat.id;
        let channel_id = ChannelId::parse(format!("telegram:{chat_id}"))?;
        let text = message.text.unwrap_or_default();
        let command = TelegramTextCommand::parse(&text);

        match command {
            TelegramTextCommand::Status => {
                let Some(binding) = state.binding_for_chat(chat_id) else {
                    return Ok(Some(reply(chat_id, "No cx session is bound to this chat.")));
                };
                let session = session::show_session(paths, &binding.session_id)?;
                Ok(Some(reply(
                    chat_id,
                    format!(
                        "session: {}\nchannel: {}\nlease_epoch: {}\nactive_lease: {}",
                        session.session_id,
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
                let sessions = session::list_sessions(paths)?;
                if sessions.is_empty() {
                    return Ok(Some(reply(chat_id, "No cx sessions.")));
                }
                let mut lines = Vec::new();
                for session in sessions {
                    lines.push(format!(
                        "{}\t{}\tepoch {}",
                        session.session_id, session.current_channel_id, session.lease_epoch
                    ));
                }
                Ok(Some(reply(chat_id, lines.join("\n"))))
            }
            TelegramTextCommand::Release => {
                let Some(binding) = state.binding_for_chat(chat_id) else {
                    return Ok(Some(reply(chat_id, "No cx session is bound to this chat.")));
                };
                let session = session::show_session(paths, &binding.session_id)?;
                let Some(active_lease) = session.active_lease else {
                    return Ok(Some(reply(chat_id, "This session has no active lease.")));
                };
                if active_lease.channel_id != binding.channel_id {
                    return Ok(Some(reply(
                        chat_id,
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
                Ok(Some(reply(chat_id, "Released Telegram lease.")))
            }
            TelegramTextCommand::Bind { .. } if !authorized_by_bind => {
                Ok(Some(reply(chat_id, "Invalid or disabled bind secret.")))
            }
            TelegramTextCommand::Start | TelegramTextCommand::Bind { .. } => {
                let binding = state.bind_chat(paths, chat_id, channel_id.clone())?;
                session::record_channel_message(
                    paths,
                    RecordChannelMessageRequest {
                        session_id: binding.session_id.clone(),
                        channel_id: channel_id.clone(),
                    },
                )?;
                if acquire_lease {
                    match session::acquire_lease(
                        paths,
                        AcquireLeaseRequest {
                            session_id: binding.session_id.clone(),
                            channel_id,
                            steal,
                        },
                    ) {
                        Ok(_) => {}
                        Err(err) => {
                            return Ok(Some(reply(
                                chat_id,
                                format!(
                                    "Session is controlled elsewhere. Use --steal on the adapter if this Telegram channel should take over.\n{err:#}"
                                ),
                            )));
                        }
                    }
                }
                Ok(Some(reply(
                    chat_id,
                    format!("Bound to cx session {}.", binding.session_id),
                )))
            }
            TelegramTextCommand::Message => {
                if text.trim().is_empty() {
                    return Ok(Some(reply(
                        chat_id,
                        "Send a text message for Codex to answer.",
                    )));
                }
                let binding = state.bind_chat(paths, chat_id, channel_id.clone())?;
                session::record_channel_message(
                    paths,
                    RecordChannelMessageRequest {
                        session_id: binding.session_id.clone(),
                        channel_id: channel_id.clone(),
                    },
                )?;
                if acquire_lease {
                    match session::acquire_lease(
                        paths,
                        AcquireLeaseRequest {
                            session_id: binding.session_id.clone(),
                            channel_id,
                            steal,
                        },
                    ) {
                        Ok(_) => {}
                        Err(err) => {
                            return Ok(Some(reply(
                                chat_id,
                                format!(
                                    "Session is controlled elsewhere. Use --steal on the adapter if this Telegram channel should take over.\n{err:#}"
                                ),
                            )));
                        }
                    }
                }
                match run_codex_turn(paths, state, chat_id, text, app_server_timeout) {
                    Ok(answer) if answer.trim().is_empty() => {
                        Ok(Some(reply(chat_id, "Codex completed without a text reply.")))
                    }
                    Ok(answer) => Ok(Some(reply(chat_id, answer))),
                    Err(err) => Ok(Some(reply(
                        chat_id,
                        format!("Codex turn failed.\n{err:#}\n\nStart app-server with `cx serve start` and keep it running, then retry."),
                    ))),
                }
            }
        }
    }

    fn run_codex_turn(
        paths: &ManagerPaths,
        state: &mut TelegramState,
        chat_id: i64,
        prompt: String,
        timeout_secs: f32,
    ) -> Result<String> {
        let server = serve::ready_app_server(paths)?;
        let mut client =
            AppServerClient::connect(&server.listen_url, Duration::from_secs_f32(timeout_secs))?;
        client.initialize("cx-telegram", env!("CARGO_PKG_VERSION"))?;

        let thread_id = match state
            .binding_for_chat(chat_id)
            .and_then(|binding| binding.app_thread_id.clone())
        {
            Some(thread_id) => thread_id,
            None => {
                let thread = client.thread_start()?;
                let binding = state
                    .binding_for_chat_mut(chat_id)
                    .with_context(|| format!("Telegram chat {chat_id} is not bound"))?;
                binding.app_thread_id = Some(thread.upstream_thread_id.clone());
                thread.upstream_thread_id
            }
        };
        let turn = client.turn_start_collect(&thread_id, prompt)?;
        Ok(turn.assistant_text)
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum TelegramTextCommand {
        Start,
        Bind { secret: Option<String> },
        Status,
        Sessions,
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
                "/status" => Self::Status,
                "/sessions" => Self::Sessions,
                "/release" => Self::Release,
                _ => Self::Message,
            }
        }
    }

    fn reply(chat_id: i64, text: impl Into<String>) -> TelegramReply {
        TelegramReply {
            chat_id,
            text: text.into(),
        }
    }

    fn log_update_summary(update_id: i64, view: &TelegramUpdateView, allowed: &BTreeSet<i64>) {
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
        let allowed = view
            .chat_id
            .map(|id| allowed.contains(&id).to_string())
            .unwrap_or_else(|| String::from("false"));
        eprintln!(
            "telegram update update_id={} source={} chat_id={} text={} allowed={}",
            update_id,
            view.source.as_str(),
            chat_id,
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

    fn send_message(client: &Client, token: &str, chat_id: i64, text: &str) -> Result<()> {
        let url = telegram_method_url(token, "sendMessage");
        let chat_id = chat_id.to_string();
        let response = client
            .post(url)
            .form(&[("chat_id", chat_id.as_str()), ("text", text)])
            .send()
            .map_err(|err| {
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
                command: "status",
                description: "Show the bound cx session and lease",
            },
            TelegramBotCommand {
                command: "sessions",
                description: "List local cx sessions",
            },
            TelegramBotCommand {
                command: "release",
                description: "Release this chat's active lease",
            },
        ]
    }

    fn send_reply(client: &Client, token: &str, chat_id: i64, text: &str) -> Result<()> {
        for chunk in telegram_text_chunks(text) {
            send_message(client, token, chat_id, &chunk)?;
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

        #[test]
        fn telegram_state_round_trips_without_token() {
            let paths = temp_paths("state");
            let mut state = TelegramState::empty();
            state.last_update_id = Some(123);
            state.bindings.push(TelegramBinding {
                chat_id: 42,
                channel_id: ChannelId::parse("telegram:42").unwrap(),
                session_id: SessionId::parse("sess_manual").unwrap(),
                app_thread_id: None,
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
            let message = TelegramMessage {
                chat: TelegramChat { id: 42 },
                text: Some(String::from("/start")),
            };

            let reply = handle_message(&paths, &mut state, message, false, false, false, 600.0)
                .unwrap()
                .unwrap();

            assert_eq!(reply.chat_id, 42);
            assert!(reply.text.contains("Bound to cx session"));
            assert_eq!(state.bindings.len(), 1);
            assert_eq!(session::list_sessions(&paths).unwrap().len(), 1);

            let _ = fs::remove_dir_all(paths.serve_dir());
        }

        #[test]
        fn release_reports_missing_binding() {
            let paths = temp_paths("release-missing");
            let mut state = TelegramState::empty();
            let message = TelegramMessage {
                chat: TelegramChat { id: 42 },
                text: Some(String::from("/release")),
            };

            let reply = handle_message(&paths, &mut state, message, false, false, false, 600.0)
                .unwrap()
                .unwrap();

            assert_eq!(reply.text, "No cx session is bound to this chat.");

            let _ = fs::remove_dir_all(paths.serve_dir());
        }

        #[test]
        fn bind_command_trusts_matching_secret_for_unknown_chat() {
            let state = TelegramState::empty();
            let trusted = BTreeSet::new();
            let message = TelegramMessage {
                chat: TelegramChat { id: 42 },
                text: Some(String::from("/bind secret-123")),
            };

            let access = message_access(&message, &state, &trusted, Some("secret-123"), true);

            assert_eq!(access, MessageAccess::AuthorizedByBind);
        }

        #[test]
        fn bind_command_rejects_wrong_secret_for_unknown_chat() {
            let state = TelegramState::empty();
            let trusted = BTreeSet::new();
            let message = TelegramMessage {
                chat: TelegramChat { id: 42 },
                text: Some(String::from("/bind wrong")),
            };

            let access = message_access(&message, &state, &trusted, Some("secret-123"), true);

            assert_eq!(access, MessageAccess::DeniedBind);
        }

        #[test]
        fn existing_binding_is_trusted_without_allow_chat() {
            let mut state = TelegramState::empty();
            state.bindings.push(TelegramBinding {
                chat_id: 42,
                channel_id: ChannelId::parse("telegram:42").unwrap(),
                session_id: SessionId::parse("sess_manual").unwrap(),
                app_thread_id: None,
            });
            let trusted = trusted_chats(&state, Vec::new());

            assert!(trusted.contains(&42));
        }

        #[test]
        fn bot_commands_cover_supported_telegram_commands() {
            let commands = bot_commands()
                .iter()
                .map(|command| command.command)
                .collect::<Vec<_>>();

            assert_eq!(
                commands,
                vec!["start", "bind", "status", "sessions", "release"]
            );
        }

        #[test]
        fn update_view_accepts_channel_posts() {
            let update = TelegramUpdate {
                update_id: 7,
                message: None,
                edited_message: None,
                channel_post: Some(TelegramMessage {
                    chat: TelegramChat { id: -10042 },
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
                    chat: TelegramChat { id: 42 },
                }),
            };

            let view = update.view();

            assert_eq!(view.source, TelegramUpdateSource::MyChatMember);
            assert_eq!(view.chat_id, Some(42));
            assert!(view.message.is_none());
        }
    }
}
