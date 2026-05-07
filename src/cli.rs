use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;

use crate::paths::ManagerPaths;

mod service;
pub use service::*;

#[derive(Debug, Parser)]
#[command(name = "cx")]
#[command(about = "Fast local Codex launcher, stdin wrapper, and slot manager")]
#[command(
    override_usage = "cx [COMMAND] [ARGS]...\n       cx [CODEX_ARGS]...\n       <stdin> | cx [PROMPT]"
)]
#[command(
    after_help = "Without a cx management command, arguments are forwarded to Codex through the best available slot. Use `--` when a Codex prompt or arg starts with a cx command name."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Query every slot and show current availability.
    Status(StatusArgs),
    /// Show local Codex token usage totals from the Codex state database.
    Stats(StatsArgs),
    /// Print the best slot name for scripting.
    Select(SelectArgs),
    /// Create or update a slot.
    Add(AddArgs),
    /// Remove a slot from rotation.
    Remove(RemoveArgs),
    /// Run `codex login` inside a slot.
    Login(LoginArgs),
    /// Launch Codex Desktop through a selected slot.
    Desktop(DesktopArgs),
    /// Run external channel adapters.
    Channel(ChannelArgs),
    /// Start and manage a local Codex app-server.
    Serve(ServeArgs),
    /// Start and manage a background cx service supervisor.
    Service(ServiceArgs),
    /// Inspect and export Codex App Server protocol bindings.
    Protocol(ProtocolArgs),
    /// Export and import portable profile-manager transfer bundles.
    Transfer(TransferArgs),
    /// Manage target-specific slot groups and overrides.
    Target(TargetArgs),
    /// Validate the local profile-manager layout.
    Doctor(DoctorArgs),
    /// Install cx into ~/.local/bin.
    Install(InstallArgs),
    /// Generate shell completion scripts.
    Completions(CompletionsArgs),
    /// Internal dynamic completion helper.
    #[command(name = "__complete", hide = true)]
    Complete(CompleteArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CompletionShell {
    Fish,
    Zsh,
    Bash,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CompleteKind {
    Slots,
    Targets,
    Models,
    Words,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum StatusSort {
    /// Show the best-scoring slots first.
    Score,
    /// Preserve rotation.txt or explicit argument order.
    Rotation,
}

#[derive(Debug, Clone, Args)]
pub struct CompletionsArgs {
    /// Shell to generate completions for.
    pub shell: CompletionShell,
}

#[derive(Debug, Clone, Args)]
pub struct CompleteArgs {
    /// Dynamic candidate kind.
    pub kind: CompleteKind,

    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Shell requesting word completion.
    #[arg(long, value_enum)]
    pub shell: Option<CompletionShell>,

    /// Active word index reported by the shell.
    #[arg(long)]
    pub cursor: Option<usize>,

    /// Active token prefix reported by the shell.
    #[arg(long, allow_hyphen_values = true)]
    pub current: Option<String>,

    /// Full shell words for runtime completion.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub words: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct SlotQueryArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Target config from targets/<name>.toml. Defaults to rotation.txt.
    #[arg(long)]
    pub target: Option<String>,

    /// Per-slot usage request timeout in seconds.
    #[arg(long, default_value_t = 2.0)]
    pub timeout: f32,

    /// Print JSON instead of a human table.
    #[arg(long)]
    pub json: bool,

    /// Slot names. Defaults to rotation.txt.
    pub slots: Vec<String>,
}

impl SlotQueryArgs {
    pub fn slots_or_rotation(&self, paths: &ManagerPaths) -> Result<Vec<String>> {
        if let Some(target) = self.target.as_deref().filter(|_| self.slots.is_empty()) {
            return crate::target::load_target(paths, target)?.slots_or_rotation(paths);
        }
        if self.slots.is_empty() {
            crate::slot::load_rotation(paths)
        } else {
            Ok(self.slots.clone())
        }
    }
}

#[derive(Debug, Clone, Args)]
pub struct StatusArgs {
    #[command(flatten)]
    pub query: SlotQueryArgs,

    /// Sort status output.
    #[arg(long, value_enum, default_value = "score")]
    pub sort: StatusSort,
}

impl StatusArgs {
    pub fn slots_or_rotation(&self, paths: &ManagerPaths) -> Result<Vec<String>> {
        self.query.slots_or_rotation(paths)
    }
}

pub type SelectArgs = SlotQueryArgs;

#[derive(Debug, Clone, Args)]
pub struct StatsArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Print JSON instead of a human table.
    #[arg(long)]
    pub json: bool,

    /// Target config from targets/<name>.toml. Defaults to all known local usage.
    #[arg(long)]
    pub target: Option<String>,

    /// Break each period down by inferred slot/account.
    #[arg(long)]
    pub by_slot: bool,

    /// Include best-effort OpenAI API price estimates.
    #[arg(long)]
    pub price: bool,

    /// Skip price estimates even if another price flag is supplied.
    #[arg(long)]
    pub no_price: bool,

    /// Include price estimates and force-refresh the cached OpenAI pricing table.
    #[arg(long)]
    pub refresh_prices: bool,

    /// Scan rollout token_count events, save a calibrated price-estimate token mix, and exit.
    #[arg(long)]
    pub calibrate: bool,

    /// Include price estimates from this pricing page.
    #[arg(long, value_hint = clap::ValueHint::Url)]
    pub price_url: Option<String>,

    /// Slot names to filter. Defaults to all known local Codex usage.
    pub slots: Vec<String>,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn status_defaults_to_score_sort() {
        let cli = Cli::parse_from(["cx", "status"]);

        let Command::Status(args) = cli.command else {
            panic!("expected status command");
        };
        assert_eq!(args.sort, StatusSort::Score);
    }

    #[test]
    fn status_accepts_rotation_sort() {
        let cli = Cli::parse_from(["cx", "status", "--sort", "rotation", "bus1"]);

        let Command::Status(args) = cli.command else {
            panic!("expected status command");
        };
        assert_eq!(args.sort, StatusSort::Rotation);
        assert_eq!(args.query.slots, vec![String::from("bus1")]);
    }

    #[test]
    fn status_accepts_target_filter() {
        let cli = Cli::parse_from(["cx", "status", "--target", "work"]);

        let Command::Status(args) = cli.command else {
            panic!("expected status command");
        };
        assert_eq!(args.query.target, Some(String::from("work")));
    }

    #[test]
    fn protocol_export_accepts_format_subset() {
        let cli = Cli::parse_from([
            "cx",
            "protocol",
            "export",
            "--out",
            "/tmp/protocol",
            "--json-schema",
        ]);

        let Command::Protocol(args) = cli.command else {
            panic!("expected protocol command");
        };
        let ProtocolCommand::Export(args) = args.command;
        assert_eq!(args.out, PathBuf::from("/tmp/protocol"));
        assert!(args.json_schema);
        assert!(!args.typescript);
    }

    #[test]
    fn serve_stop_accepts_force_and_json() {
        let cli = Cli::parse_from([
            "cx",
            "serve",
            "stop",
            "--wait-timeout",
            "0.5",
            "--force",
            "--json",
        ]);

        let Command::Serve(args) = cli.command else {
            panic!("expected serve command");
        };
        let ServeCommand::Stop(args) = args.command else {
            panic!("expected serve stop command");
        };
        assert_eq!(args.wait_timeout, 0.5);
        assert!(args.force);
        assert!(args.json);
    }

    #[test]
    fn serve_threads_accepts_limit_and_listen_url() {
        let cli = Cli::parse_from([
            "cx",
            "serve",
            "threads",
            "--listen",
            "ws://127.0.0.1:17654",
            "--limit",
            "5",
        ]);

        let Command::Serve(args) = cli.command else {
            panic!("expected serve command");
        };
        let ServeCommand::Threads(args) = args.command else {
            panic!("expected serve threads command");
        };
        assert_eq!(args.listen, Some(String::from("ws://127.0.0.1:17654")));
        assert_eq!(args.limit, 5);
    }

    #[test]
    fn telegram_run_accepts_allow_chat_and_token_env() {
        let cli = Cli::parse_from([
            "cx",
            "channel",
            "telegram",
            "run",
            "--allow-chat",
            "12345",
            "--bot-token-env",
            "CX_TG_TOKEN",
            "--acquire-lease",
            "--log-updates",
            "--app-server-timeout",
            "120",
        ]);

        let Command::Channel(args) = cli.command else {
            panic!("expected channel command");
        };
        let ChannelCommand::Telegram(args) = args.command;
        let TelegramCommand::Run(args) = args.command else {
            panic!("expected telegram run command");
        };
        assert_eq!(args.allow_chats, vec![12345]);
        assert_eq!(args.bot_token_env, "CX_TG_TOKEN");
        assert_eq!(args.app_server_timeout, 120.0);
        assert!(args.acquire_lease);
        assert!(args.log_updates);
    }

    #[test]
    fn telegram_run_accepts_negative_allow_chat_with_equals() {
        let cli = Cli::parse_from([
            "cx",
            "channel",
            "telegram",
            "run",
            "--allow-chat=-1003586916929",
        ]);

        let Command::Channel(args) = cli.command else {
            panic!("expected channel command");
        };
        let ChannelCommand::Telegram(args) = args.command;
        let TelegramCommand::Run(args) = args.command else {
            panic!("expected telegram run command");
        };
        assert_eq!(args.allow_chats, vec![-1003586916929]);
    }

    #[test]
    fn telegram_bind_accepts_token_env_and_timeouts() {
        let cli = Cli::parse_from([
            "cx",
            "channel",
            "telegram",
            "bind",
            "--bot-token-env",
            "CX_TG_TOKEN",
            "--poll-timeout",
            "5",
            "--request-timeout",
            "10",
            "--log-updates",
        ]);

        let Command::Channel(args) = cli.command else {
            panic!("expected channel command");
        };
        let ChannelCommand::Telegram(args) = args.command;
        let TelegramCommand::Bind(args) = args.command else {
            panic!("expected telegram bind command");
        };
        assert_eq!(args.bot_token_env, "CX_TG_TOKEN");
        assert_eq!(args.poll_timeout, 5);
        assert_eq!(args.request_timeout, 10.0);
        assert!(args.log_updates);
    }

    #[test]
    fn telegram_menu_accepts_token_env_and_timeout() {
        let cli = Cli::parse_from([
            "cx",
            "channel",
            "telegram",
            "menu",
            "--bot-token-env",
            "CX_TG_TOKEN",
            "--request-timeout",
            "10",
        ]);

        let Command::Channel(args) = cli.command else {
            panic!("expected channel command");
        };
        let ChannelCommand::Telegram(args) = args.command;
        let TelegramCommand::Menu(args) = args.command else {
            panic!("expected telegram menu command");
        };
        assert_eq!(args.bot_token_env, "CX_TG_TOKEN");
        assert_eq!(args.request_timeout, 10.0);
    }

    #[test]
    fn service_start_defaults_to_telegram() {
        let cli = Cli::parse_from([
            "cx",
            "service",
            "start",
            "--target",
            "work",
            "--allow-chat",
            "12345",
            "--acquire-lease",
        ]);

        let Command::Service(args) = cli.command else {
            panic!("expected service command");
        };
        let ServiceCommand::Start(args) = args.command else {
            panic!("expected service start command");
        };
        assert_eq!(args.spec.target, Some(String::from("work")));
        assert!(!args.spec.no_telegram);
        assert_eq!(args.spec.allow_chats, vec![12345]);
        assert!(args.spec.acquire_lease);
    }

    #[test]
    fn service_start_accepts_negative_allow_chat_with_equals() {
        let cli = Cli::parse_from(["cx", "service", "start", "--allow-chat=-1003586916929"]);

        let Command::Service(args) = cli.command else {
            panic!("expected service command");
        };
        let ServiceCommand::Start(args) = args.command else {
            panic!("expected service start command");
        };
        assert_eq!(args.spec.allow_chats, vec![-1003586916929]);
    }

    #[test]
    fn service_stop_accepts_force_and_json() {
        let cli = Cli::parse_from([
            "cx",
            "service",
            "stop",
            "--wait-timeout",
            "0.5",
            "--force",
            "--json",
        ]);

        let Command::Service(args) = cli.command else {
            panic!("expected service command");
        };
        let ServiceCommand::Stop(args) = args.command else {
            panic!("expected service stop command");
        };
        assert_eq!(args.wait_timeout, 0.5);
        assert!(args.force);
        assert!(args.json);
    }

    #[test]
    fn service_token_set_reads_named_token_from_stdin() {
        let cli = Cli::parse_from(["cx", "service", "token", "set", "telegram"]);

        let Command::Service(args) = cli.command else {
            panic!("expected service command");
        };
        let ServiceCommand::Token(args) = args.command else {
            panic!("expected service token command");
        };
        let ServiceTokenCommand::Set(args) = args.command else {
            panic!("expected service token set command");
        };
        assert_eq!(args.token, ServiceTokenName::Telegram);
    }

    #[test]
    fn serve_ping_accepts_timeout_and_json() {
        let cli = Cli::parse_from(["cx", "serve", "ping", "--timeout", "0.5", "--json"]);

        let Command::Serve(args) = cli.command else {
            panic!("expected serve command");
        };
        let ServeCommand::Ping(args) = args.command else {
            panic!("expected serve ping command");
        };
        assert_eq!(args.timeout, 0.5);
        assert!(args.json);
    }

    #[test]
    fn desktop_accepts_slot_target_app_bin_wait_and_app_args() {
        let cli = Cli::parse_from([
            "cx",
            "desktop",
            "--slot",
            "bus1",
            "--target",
            "work",
            "--app-bin",
            "/Applications/Codex.app",
            "--wait",
            "--allow-parallel",
            "--",
            "--enable-logging",
        ]);

        let Command::Desktop(args) = cli.command else {
            panic!("expected desktop command");
        };
        assert_eq!(args.slot, Some(String::from("bus1")));
        assert_eq!(args.target, Some(String::from("work")));
        assert_eq!(args.app_bin, Some(PathBuf::from("/Applications/Codex.app")));
        assert!(args.wait);
        assert!(args.allow_parallel);
        assert_eq!(args.args, vec![String::from("--enable-logging")]);
    }

    #[test]
    fn serve_session_create_accepts_id_channel_and_json() {
        let cli = Cli::parse_from([
            "cx",
            "serve",
            "session",
            "create",
            "--id",
            "sess_manual",
            "--channel",
            "telegram:12345",
            "--json",
        ]);

        let Command::Serve(args) = cli.command else {
            panic!("expected serve command");
        };
        let ServeCommand::Session(args) = args.command else {
            panic!("expected serve session command");
        };
        let ServeSessionCommand::Create(args) = args.command else {
            panic!("expected serve session create command");
        };
        assert_eq!(args.id, Some(String::from("sess_manual")));
        assert_eq!(args.channel, "telegram:12345");
        assert!(args.json);
    }

    #[test]
    fn serve_lease_acquire_accepts_session_channel_and_json() {
        let cli = Cli::parse_from([
            "cx",
            "serve",
            "lease",
            "acquire",
            "--session",
            "sess_manual",
            "--channel",
            "terminal",
            "--steal",
            "--json",
        ]);

        let Command::Serve(args) = cli.command else {
            panic!("expected serve command");
        };
        let ServeCommand::Lease(args) = args.command else {
            panic!("expected serve lease command");
        };
        let ServeLeaseCommand::Acquire(args) = args.command else {
            panic!("expected serve lease acquire command");
        };
        assert_eq!(args.session, "sess_manual");
        assert_eq!(args.channel, "terminal");
        assert!(args.steal);
        assert!(args.json);
    }

    #[test]
    fn serve_event_list_accepts_session_filter_and_json() {
        let cli = Cli::parse_from([
            "cx",
            "serve",
            "event",
            "list",
            "--session",
            "sess_manual",
            "--json",
        ]);

        let Command::Serve(args) = cli.command else {
            panic!("expected serve command");
        };
        let ServeCommand::Event(args) = args.command else {
            panic!("expected serve event command");
        };
        let ServeEventCommand::List(args) = args.command;
        assert_eq!(args.session, Some(String::from("sess_manual")));
        assert!(args.json);
    }
}

#[derive(Debug, Clone, Args)]
pub struct AddArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Slot name to create or update.
    pub slot: String,

    /// Add the slot to rotation.txt.
    #[arg(long)]
    pub rotate: bool,

    /// Copy auth.json/current/accounts from ~/.codex into the new slot.
    #[arg(long)]
    pub from_current: bool,

    /// Per-slot Codex config override, passed as `codex -c key=value`.
    #[arg(long = "set")]
    pub sets: Vec<String>,

    /// Per-slot environment variable, stored in env.conf.
    #[arg(long = "env")]
    pub envs: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct RemoveArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Slot name to remove.
    pub slot: String,

    /// Also delete the slot directory and its auth files.
    #[arg(long)]
    pub delete_files: bool,
}

#[derive(Debug, Clone, Args)]
pub struct LoginArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Path to the real Codex binary.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub codex_bin: Option<PathBuf>,

    /// Slot name to log into.
    pub slot: String,

    /// Extra args forwarded to `codex login`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct DesktopArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Path to the Codex Desktop executable or .app bundle.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub app_bin: Option<PathBuf>,

    /// Force a specific slot instead of selecting from rotation or target.
    #[arg(long)]
    pub slot: Option<String>,

    /// Target config from targets/<name>.toml.
    #[arg(long)]
    pub target: Option<String>,

    /// Print slot selection details before launch.
    #[arg(long)]
    pub cx_debug: bool,

    /// Wait for Codex Desktop to exit instead of returning after launch.
    #[arg(long)]
    pub wait: bool,

    /// Launch even when another Codex Desktop process is already running.
    #[arg(long)]
    pub allow_parallel: bool,

    /// Extra args forwarded to the Codex Desktop executable.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct ServeArgs {
    #[command(subcommand)]
    pub command: ServeCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ServeCommand {
    /// Start the foreground cx control daemon on a private local socket.
    Daemon(ServeDaemonArgs),
    /// Ping the local cx control daemon.
    Ping(ServePingArgs),
    /// Ask the local cx control daemon to exit.
    Shutdown(ServeShutdownArgs),
    /// Create and inspect cx-owned sessions through the control daemon.
    Session(ServeSessionArgs),
    /// Acquire or release cx session channel leases.
    Lease(ServeLeaseArgs),
    /// Inspect cx control-plane events.
    Event(ServeEventArgs),
    /// Start a foreground Codex app-server through a selected slot.
    Start(ServeStartArgs),
    /// Stop the recorded foreground app-server or clean stale serve state.
    Stop(ServeStopArgs),
    /// Show the last recorded foreground app-server state.
    Status(ServeStatusArgs),
    /// Connect to a running app-server and verify the Codex protocol handshake.
    Probe(ServeProbeArgs),
    /// List Codex app-server threads through the cx protocol adapter.
    Threads(ServeThreadsArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ServeDaemonArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Print daemon startup metadata as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServePingArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Seconds to wait for the daemon response.
    #[arg(long, default_value_t = 2.0)]
    pub timeout: f32,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServeShutdownArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Seconds to wait for the daemon response.
    #[arg(long, default_value_t = 2.0)]
    pub timeout: f32,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServeSessionArgs {
    #[command(subcommand)]
    pub command: ServeSessionCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ServeSessionCommand {
    /// Create a cx session and append a session-created event.
    Create(ServeSessionCreateArgs),
    /// List cx sessions from the control daemon.
    List(ServeSessionListArgs),
    /// Show one cx session.
    Show(ServeSessionShowArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ServeSessionCreateArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Optional session id. Generated when omitted.
    #[arg(long)]
    pub id: Option<String>,

    /// Channel creating the session.
    #[arg(long, default_value = "terminal")]
    pub channel: String,

    /// Seconds to wait for the daemon response.
    #[arg(long, default_value_t = 2.0)]
    pub timeout: f32,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServeSessionListArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Seconds to wait for the daemon response.
    #[arg(long, default_value_t = 2.0)]
    pub timeout: f32,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServeSessionShowArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Seconds to wait for the daemon response.
    #[arg(long, default_value_t = 2.0)]
    pub timeout: f32,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,

    /// cx session id.
    pub session_id: String,
}

#[derive(Debug, Clone, Args)]
pub struct ServeLeaseArgs {
    #[command(subcommand)]
    pub command: ServeLeaseCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ServeLeaseCommand {
    /// Acquire control of a cx session for a channel.
    Acquire(ServeLeaseAcquireArgs),
    /// Release the active lease for a cx session.
    Release(ServeLeaseReleaseArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ServeLeaseAcquireArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// cx session id.
    #[arg(long)]
    pub session: String,

    /// Channel acquiring the lease.
    #[arg(long, default_value = "terminal")]
    pub channel: String,

    /// Acquire even when another channel currently holds the lease.
    #[arg(long)]
    pub steal: bool,

    /// Seconds to wait for the daemon response.
    #[arg(long, default_value_t = 2.0)]
    pub timeout: f32,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServeLeaseReleaseArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// cx session id.
    #[arg(long)]
    pub session: String,

    /// Lease token returned by `cx serve lease acquire`.
    #[arg(long)]
    pub token: String,

    /// Seconds to wait for the daemon response.
    #[arg(long, default_value_t = 2.0)]
    pub timeout: f32,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServeEventArgs {
    #[command(subcommand)]
    pub command: ServeEventCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ServeEventCommand {
    /// List append-only cx control-plane events.
    List(ServeEventListArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ServeEventListArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Filter events to one cx session id.
    #[arg(long)]
    pub session: Option<String>,

    /// Seconds to wait for the daemon response.
    #[arg(long, default_value_t = 2.0)]
    pub timeout: f32,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServeStartArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Path to the real Codex binary.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub codex_bin: Option<PathBuf>,

    /// Force a specific slot instead of selecting from rotation or target.
    #[arg(long)]
    pub slot: Option<String>,

    /// Target config from targets/<name>.toml.
    #[arg(long)]
    pub target: Option<String>,

    /// WebSocket listen URL for Codex app-server. Only loopback ws:// URLs are accepted.
    #[arg(long, default_value = "ws://127.0.0.1:0")]
    pub listen: String,

    /// Seconds to wait for the app-server ready endpoint.
    #[arg(long, default_value_t = 10.0)]
    pub ready_timeout: f32,

    /// Extra args forwarded after `codex app-server --listen <url>`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct ServeStopArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Seconds to wait for the app-server process to exit.
    #[arg(long, default_value_t = 5.0)]
    pub wait_timeout: f32,

    /// Send SIGKILL if the graceful stop does not finish before --wait-timeout.
    #[arg(long)]
    pub force: bool,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServeStatusArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServeProbeArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Probe this loopback app-server URL instead of the saved serve state.
    #[arg(long)]
    pub listen: Option<String>,

    /// Seconds to wait for the WebSocket protocol probe.
    #[arg(long, default_value_t = 5.0)]
    pub timeout: f32,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServeThreadsArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Query this loopback app-server URL instead of the saved serve state.
    #[arg(long)]
    pub listen: Option<String>,

    /// Maximum number of threads to return.
    #[arg(long, default_value_t = 20)]
    pub limit: u64,

    /// Seconds to wait for the WebSocket protocol request.
    #[arg(long, default_value_t = 5.0)]
    pub timeout: f32,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ChannelArgs {
    #[command(subcommand)]
    pub command: ChannelCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ChannelCommand {
    /// Run the Telegram channel adapter.
    Telegram(TelegramArgs),
}

#[derive(Debug, Clone, Args)]
pub struct TelegramArgs {
    #[command(subcommand)]
    pub command: TelegramCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum TelegramCommand {
    /// Poll Telegram and bridge allowed chats into cx sessions.
    Run(TelegramRunArgs),
    /// Generate a one-time /bind secret and trust the first matching chat.
    Bind(TelegramBindArgs),
    /// Sync the Telegram bot command menu and exit.
    Menu(TelegramMenuArgs),
    /// Show local Telegram adapter state.
    Status(TelegramStatusArgs),
}

#[derive(Debug, Clone, Args)]
pub struct TelegramRunArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Environment variable containing the Telegram bot token.
    #[arg(long, default_value = "TELEGRAM_BOT_TOKEN")]
    pub bot_token_env: String,

    /// Allowed Telegram chat id. Repeat for multiple chats.
    #[arg(long = "allow-chat")]
    pub allow_chats: Vec<i64>,

    /// Long-poll timeout passed to Telegram getUpdates.
    #[arg(long, default_value_t = 30)]
    pub poll_timeout: u64,

    /// HTTP request timeout in seconds.
    #[arg(long, default_value_t = 35.0)]
    pub request_timeout: f32,

    /// App-server WebSocket timeout in seconds for Codex turns.
    #[arg(long, default_value_t = 600.0)]
    pub app_server_timeout: f32,

    /// Acquire an active lease for incoming messages.
    #[arg(long)]
    pub acquire_lease: bool,

    /// Steal an existing lease when acquiring for Telegram.
    #[arg(long)]
    pub steal: bool,

    /// Log safe Telegram update summaries to stderr.
    #[arg(long)]
    pub log_updates: bool,
}

#[derive(Debug, Clone, Args)]
pub struct TelegramBindArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Environment variable containing the Telegram bot token.
    #[arg(long, default_value = "TELEGRAM_BOT_TOKEN")]
    pub bot_token_env: String,

    /// Long-poll timeout passed to Telegram getUpdates.
    #[arg(long, default_value_t = 30)]
    pub poll_timeout: u64,

    /// HTTP request timeout in seconds.
    #[arg(long, default_value_t = 35.0)]
    pub request_timeout: f32,

    /// Log safe Telegram update summaries to stderr.
    #[arg(long)]
    pub log_updates: bool,
}

#[derive(Debug, Clone, Args)]
pub struct TelegramMenuArgs {
    /// Environment variable containing the Telegram bot token.
    #[arg(long, default_value = "TELEGRAM_BOT_TOKEN")]
    pub bot_token_env: String,

    /// HTTP request timeout in seconds.
    #[arg(long, default_value_t = 35.0)]
    pub request_timeout: f32,
}

#[derive(Debug, Clone, Args)]
pub struct TelegramStatusArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ProtocolArgs {
    #[command(subcommand)]
    pub command: ProtocolCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ProtocolCommand {
    /// Export version-matched Codex App Server schema and TypeScript bindings.
    Export(ProtocolExportArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ProtocolExportArgs {
    /// Path to the real Codex binary.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub codex_bin: Option<PathBuf>,

    /// Output directory. Subdirectories are created for each exported format.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub out: PathBuf,

    /// Export JSON Schema only unless another format flag is also supplied.
    #[arg(long)]
    pub json_schema: bool,

    /// Export TypeScript bindings only unless another format flag is also supplied.
    #[arg(long)]
    pub typescript: bool,

    /// Include experimental protocol methods and fields.
    #[arg(long)]
    pub experimental: bool,
}

#[derive(Debug, Clone, Args)]
pub struct TransferArgs {
    #[command(subcommand)]
    pub command: TransferCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum TransferCommand {
    /// Export rotation slots and login state into a portable directory bundle.
    Export(TransferExportArgs),
    /// Import a portable directory bundle into this machine's profile-manager.
    Import(TransferImportArgs),
}

#[derive(Debug, Clone, Args)]
pub struct TransferExportArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Output directory. Must be outside the live ~/.codex tree.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub out: PathBuf,

    /// Replace an existing output directory.
    #[arg(long)]
    pub replace: bool,

    /// Slot names to export. Defaults to rotation.txt.
    pub slots: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct TransferImportArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Replace existing destination files.
    #[arg(long)]
    pub replace: bool,

    /// Bundle directory created by `cx transfer export`.
    #[arg(value_hint = clap::ValueHint::DirPath)]
    pub bundle: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct TargetArgs {
    #[command(subcommand)]
    pub command: TargetCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum TargetCommand {
    /// List configured targets.
    List(TargetListArgs),
    /// Show one target config.
    Show(TargetShowArgs),
    /// Create or update a target config.
    Add(TargetAddArgs),
    /// Remove a target config.
    Remove(TargetRemoveArgs),
}

#[derive(Debug, Clone, Args)]
pub struct TargetListArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Print JSON instead of plain names.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct TargetShowArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,

    /// Target name.
    pub target: String,
}

#[derive(Debug, Clone, Args)]
pub struct TargetAddArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Target name.
    pub target: String,

    /// Target-specific Codex config override, passed after slot overrides.
    #[arg(long = "set")]
    pub sets: Vec<String>,

    /// Target-specific environment variable, merged after slot env.conf.
    #[arg(long = "env")]
    pub envs: Vec<String>,

    /// Slots used by this target. Defaults to rotation.txt when omitted.
    pub slots: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct TargetRemoveArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Target name.
    pub target: String,
}

#[derive(Debug, Clone, Args)]
pub struct DoctorArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Also query the live usage endpoint.
    #[arg(long)]
    pub online: bool,

    /// Per-slot usage request timeout in seconds.
    #[arg(long, default_value_t = 2.0)]
    pub timeout: f32,
}

#[derive(Debug, Clone, Args)]
pub struct InstallArgs {
    /// Directory to install cx into.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub bin_dir: Option<PathBuf>,

    /// Replace an existing cx binary.
    #[arg(long)]
    pub force: bool,
}
