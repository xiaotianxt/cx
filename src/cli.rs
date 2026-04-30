use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use clap::Parser;
use clap::Subcommand;

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
    /// Run `codex login` inside a slot.
    Login(LoginArgs),
    /// Validate the local profile-manager layout.
    Doctor(DoctorArgs),
    /// Install cx into ~/.local/bin.
    Install(InstallArgs),
}

#[derive(Debug, Clone, Args)]
pub struct SlotQueryArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long)]
    pub manager_dir: Option<PathBuf>,

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
        if self.slots.is_empty() {
            crate::slot::load_rotation(paths)
        } else {
            Ok(self.slots.clone())
        }
    }
}

pub type StatusArgs = SlotQueryArgs;
pub type SelectArgs = SlotQueryArgs;

#[derive(Debug, Clone, Args)]
pub struct StatsArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long)]
    pub manager_dir: Option<PathBuf>,

    /// Print JSON instead of a human table.
    #[arg(long)]
    pub json: bool,

    /// Break each period down by inferred slot/account.
    #[arg(long)]
    pub by_slot: bool,

    /// Skip price fetching and price estimates.
    #[arg(long)]
    pub no_price: bool,

    /// Force-refresh the cached OpenAI pricing table.
    #[arg(long)]
    pub refresh_prices: bool,

    /// Scan rollout token_count events, save a calibrated price-estimate token mix, and exit.
    #[arg(long)]
    pub calibrate: bool,

    /// Pricing page to fetch. Defaults to the official OpenAI API pricing docs.
    #[arg(long)]
    pub price_url: Option<String>,

    /// Slot names to filter. Defaults to all known local Codex usage.
    pub slots: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct AddArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long)]
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
pub struct LoginArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long)]
    pub manager_dir: Option<PathBuf>,

    /// Path to the real Codex binary.
    #[arg(long)]
    pub codex_bin: Option<PathBuf>,

    /// Slot name to log into.
    pub slot: String,

    /// Extra args forwarded to `codex login`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct DoctorArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long)]
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
    #[arg(long)]
    pub bin_dir: Option<PathBuf>,

    /// Replace an existing cx binary.
    #[arg(long)]
    pub force: bool,
}
