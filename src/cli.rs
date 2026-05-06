use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;

use crate::paths::ManagerPaths;

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
    /// Start and manage a local Codex app-server.
    Serve(ServeArgs),
    /// Inspect and export Codex App Server protocol bindings.
    Protocol(ProtocolArgs),
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
