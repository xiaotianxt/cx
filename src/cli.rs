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
    #[arg(long)]
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
    #[arg(long)]
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
    #[arg(long)]
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
    #[arg(long)]
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
pub struct RemoveArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long)]
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
    #[arg(long)]
    pub manager_dir: Option<PathBuf>,

    /// Print JSON instead of plain names.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct TargetShowArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long)]
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
    #[arg(long)]
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
    #[arg(long)]
    pub manager_dir: Option<PathBuf>,

    /// Target name.
    pub target: String,
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
