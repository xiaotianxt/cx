use std::path::PathBuf;

use clap::Args;
use clap::Subcommand;
use clap::ValueEnum;

#[derive(Debug, Clone, Args)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub command: ServiceCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ServiceCommand {
    /// Start the cx service supervisor in the background.
    Start(ServiceStartArgs),
    /// Run the cx service supervisor in the foreground.
    #[command(hide = true)]
    Run(ServiceRunArgs),
    /// Stop the cx service supervisor and its children.
    Stop(ServiceStopArgs),
    /// Show cx service supervisor status.
    Status(ServiceStatusArgs),
    /// Print recent cx service logs.
    Logs(ServiceLogsArgs),
    /// Configure service-scoped secrets.
    Token(ServiceTokenArgs),
    /// Install a macOS launchd service.
    Install(ServiceInstallArgs),
    /// Remove the macOS launchd service.
    Uninstall(ServiceUninstallArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ServiceStartArgs {
    #[command(flatten)]
    pub spec: ServiceSpecArgs,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServiceRunArgs {
    #[command(flatten)]
    pub spec: ServiceSpecArgs,
}

#[derive(Debug, Clone, Args)]
pub struct ServiceInstallArgs {
    #[command(flatten)]
    pub spec: ServiceSpecArgs,

    /// Start the launchd service after installing it.
    #[arg(long)]
    pub start: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServiceSpecArgs {
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

    /// Do not run the Telegram adapter.
    #[arg(long)]
    pub no_telegram: bool,

    /// Environment variable containing the Telegram bot token.
    #[arg(long, default_value = "TELEGRAM_BOT_TOKEN")]
    pub telegram_bot_token_env: String,

    /// Allowed Telegram chat id. Repeat for multiple chats.
    #[arg(long = "allow-chat")]
    pub allow_chats: Vec<i64>,

    /// Acquire an active lease for incoming Telegram messages.
    #[arg(long)]
    pub acquire_lease: bool,

    /// Steal an existing lease when acquiring for Telegram.
    #[arg(long)]
    pub steal: bool,

    /// Log safe Telegram update summaries to the service log.
    #[arg(long)]
    pub log_updates: bool,

    /// App-server WebSocket timeout in seconds for Telegram Codex turns.
    #[arg(long, default_value_t = 600.0)]
    pub app_server_timeout: f32,
}

#[derive(Debug, Clone, Args)]
pub struct ServiceStopArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Seconds to wait for the service and child processes to exit.
    #[arg(long, default_value_t = 10.0)]
    pub wait_timeout: f32,

    /// Send SIGKILL if graceful stop does not finish before --wait-timeout.
    #[arg(long)]
    pub force: bool,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServiceStatusArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServiceLogsArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Number of recent log lines to print.
    #[arg(long, default_value_t = 80)]
    pub lines: usize,
}

#[derive(Debug, Clone, Args)]
pub struct ServiceUninstallArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct ServiceTokenArgs {
    #[command(subcommand)]
    pub command: ServiceTokenCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ServiceTokenCommand {
    /// Store a service secret from stdin.
    Set(ServiceTokenSetArgs),
    /// Show whether a service secret is configured.
    Status(ServiceTokenStatusArgs),
    /// Remove a configured service secret.
    Delete(ServiceTokenDeleteArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum ServiceTokenName {
    Telegram,
}

#[derive(Debug, Clone, Args)]
pub struct ServiceTokenSetArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Token to store. The token value is read from stdin.
    #[arg(value_enum)]
    pub token: ServiceTokenName,
}

#[derive(Debug, Clone, Args)]
pub struct ServiceTokenStatusArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Print JSON instead of human output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct ServiceTokenDeleteArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Token to delete.
    #[arg(value_enum)]
    pub token: ServiceTokenName,
}
