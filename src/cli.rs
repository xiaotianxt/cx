use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use time::macros::format_description;
use time::Date;

use crate::paths::ManagerPaths;

#[derive(Debug, Parser)]
#[command(name = "cx")]
#[command(about = "Fast local Codex launcher, stdin wrapper, and slot manager")]
#[command(
    override_usage = "cx [COMMAND] [ARGS]...\n       cx [CX_OPTIONS] [-- CODEX_ARGS]...\n       <stdin> | cx [CX_OPTIONS] [-- PROMPT]"
)]
#[command(
    after_help = "Without a cx management command, cx launches Codex through the best available local slot. Codex arguments must follow `--`."
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
    /// Plan and run short quota-window priming requests.
    Prime(PrimeArgs),
    /// Print the best slot name for scripting.
    Select(SelectArgs),
    /// Create or update a slot.
    Add(AddArgs),
    /// Remove a slot from rotation.
    Remove(RemoveArgs),
    /// Run `codex login` inside a slot.
    Login(LoginArgs),
    /// Manage Keychain-backed Personal Access Tokens for slots.
    Pat(PatArgs),
    /// Launch ChatGPT Desktop through a selected slot.
    Desktop(DesktopArgs),
    /// Export and import portable profile-manager transfer bundles.
    Transfer(TransferArgs),
    /// Manage target-specific slot groups and overrides.
    Target(TargetArgs),
    /// Validate the local profile-manager layout.
    Doctor(DoctorArgs),
    /// Merge per-slot SQLite indexes into the shared ~/.codex/sqlite database.
    MergeSqlite(MergeSqliteArgs),
    /// Install cx into ~/.local/bin.
    Install(InstallArgs),
    /// Generate launcher shell completion scripts.
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
    Words,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum StatusSort {
    /// Show the best-scoring slots first.
    Score,
    /// Preserve rotation.txt or explicit argument order.
    Rotation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsRange {
    raw: String,
    kind: StatsRangeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatsRangeKind {
    All,
    LastDays(u32),
    Since(Date),
    Between { start: Date, end: Date },
}

impl StatsRange {
    pub fn key(&self) -> &str {
        &self.raw
    }

    pub fn kind(&self) -> &StatsRangeKind {
        &self.kind
    }

    pub fn label(&self) -> String {
        match &self.kind {
            StatsRangeKind::All => "All time".to_string(),
            StatsRangeKind::LastDays(days) => format!("Last {days} days"),
            StatsRangeKind::Since(date) => format!("Since {date}"),
            StatsRangeKind::Between { start, end } => format!("{start} to {end}"),
        }
    }
}

impl FromStr for StatsRange {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let raw = value.trim();
        if raw.is_empty() {
            return Err("range cannot be empty".to_string());
        }
        let normalized = raw.to_ascii_lowercase();
        if normalized == "all" {
            return Ok(Self {
                raw: "all".to_string(),
                kind: StatsRangeKind::All,
            });
        }
        if let Some(days) = parse_relative_days(&normalized) {
            return Ok(Self {
                raw: normalized,
                kind: StatsRangeKind::LastDays(days),
            });
        }
        if let Some((start, end)) = raw.split_once("..") {
            let start = parse_stats_date(start.trim())?;
            let end = parse_stats_date(end.trim())?;
            return Ok(Self {
                raw: format!("{start}..{end}"),
                kind: StatsRangeKind::Between { start, end },
            });
        }
        if let Ok(date) = parse_stats_date(raw) {
            return Ok(Self {
                raw: date.to_string(),
                kind: StatsRangeKind::Since(date),
            });
        }
        Err("range must be all, Nd/Nw/Nm/Ny, YYYY-MM-DD, or YYYY-MM-DD..YYYY-MM-DD".to_string())
    }
}

fn parse_relative_days(value: &str) -> Option<u32> {
    let value = value
        .strip_prefix("last-")
        .or_else(|| value.strip_prefix("last"))
        .unwrap_or(value)
        .strip_suffix("-days")
        .or_else(|| value.strip_suffix("days"))
        .or_else(|| value.strip_suffix("-day"))
        .or_else(|| value.strip_suffix("day"))
        .unwrap_or(value);
    if value.chars().all(|ch| ch.is_ascii_digit()) {
        return value.parse::<u32>().ok().filter(|days| *days > 0);
    }
    let (number, unit) = value.split_at(value.find(|ch: char| !ch.is_ascii_digit())?);
    let number = number.parse::<u32>().ok()?;
    if number == 0 {
        return None;
    }
    match unit {
        "d" => Some(number),
        "w" => number.checked_mul(7),
        "m" => number.checked_mul(30),
        "y" => number.checked_mul(365),
        _ => None,
    }
}

fn parse_stats_date(value: &str) -> std::result::Result<Date, String> {
    Date::parse(value, format_description!("[year]-[month]-[day]"))
        .map_err(|_| format!("invalid date `{value}`; expected YYYY-MM-DD"))
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

    /// Maximum number of slot usage requests to run at once.
    #[arg(long, default_value_t = crate::selector::DEFAULT_SLOT_QUERY_JOBS)]
    pub jobs: usize,

    /// Retry transient usage request failures this many times.
    #[arg(long, default_value_t = crate::selector::DEFAULT_SLOT_QUERY_RETRIES)]
    pub retries: usize,

    /// Skip the fresh usage cache and query live usage now.
    #[arg(long)]
    pub no_cache: bool,

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

    /// Disable the interactive progress line.
    #[arg(long)]
    pub no_progress: bool,

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

    /// Legacy alias to include best-effort OpenAI API price estimates.
    #[arg(long, hide = true)]
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

    /// Time range used by the chart and model mix: all, 17d, 2w, 3m, YYYY-MM-DD, or YYYY-MM-DD..YYYY-MM-DD.
    #[arg(long, default_value = "all", value_name = "RANGE")]
    pub range: StatsRange,

    /// Slot names to filter. Defaults to all known local Codex usage.
    pub slots: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct PrimeArgs {
    #[command(subcommand)]
    pub command: PrimeCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum PrimeCommand {
    /// Show data-derived daily prime times.
    Plan(PrimePlanArgs),
    /// Install a macOS LaunchAgent that runs prime checks at planned times.
    Install(PrimeInstallArgs),
    /// Run one prime check now.
    Run(PrimeRunArgs),
    /// Show saved prime config and last run state.
    Status(PrimeStatusArgs),
    /// Uninstall the macOS LaunchAgent.
    Uninstall(PrimeUninstallArgs),
}

#[derive(Debug, Clone, Args)]
pub struct PrimePlanArgs {
    #[command(flatten)]
    pub schedule: PrimeScheduleArgs,

    /// Print JSON instead of a human report.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct PrimeInstallArgs {
    #[command(flatten)]
    pub schedule: PrimeScheduleArgs,

    /// Target config whose slots and overrides are used by prime runs.
    #[arg(long)]
    pub target: Option<String>,

    /// Restrict prime runs to these slots. Defaults to target slots or rotation.txt.
    #[arg(long = "slot")]
    pub slots: Vec<String>,

    /// Maximum slots to prime in a single run. Defaults to every eligible slot.
    #[arg(long)]
    pub max_slots: Option<usize>,

    /// Real Codex binary used by launchd runs.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub codex_bin: Option<PathBuf>,

    /// Model to use for the tiny priming request. Defaults to Codex config.
    #[arg(long)]
    pub model: Option<String>,

    /// Prompt used for the tiny priming request.
    #[arg(long, default_value = crate::prime::DEFAULT_PRIME_PROMPT)]
    pub prompt: String,

    /// Per-slot usage request timeout in seconds.
    #[arg(long, default_value_t = 2.0)]
    pub timeout: f32,

    /// Maximum number of slot usage requests to run at once.
    #[arg(long, default_value_t = 4)]
    pub jobs: usize,

    /// Retry transient usage request failures this many times.
    #[arg(long, default_value_t = 1)]
    pub retries: usize,

    /// Seconds to poll live usage after each prime request.
    #[arg(long, default_value_t = crate::prime::DEFAULT_PRIME_VERIFY_TIMEOUT_SECONDS)]
    pub verify_timeout: u64,

    /// Maximum Codex requests to send per slot while trying to verify active.
    #[arg(long, default_value_t = crate::prime::DEFAULT_PRIME_MAX_REQUESTS)]
    pub max_requests: usize,

    /// Minimum weekly remaining percentage required before sending a prime.
    #[arg(long, default_value_t = 5.0)]
    pub min_weekly_remaining: f64,

    /// Prime immediately after installing.
    #[arg(long)]
    pub run_now: bool,
}

#[derive(Debug, Clone, Args)]
pub struct PrimeRunArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Target config whose slots and overrides are used by this run.
    #[arg(long)]
    pub target: Option<String>,

    /// Restrict this run to these slots.
    #[arg(long = "slot")]
    pub slots: Vec<String>,

    /// Maximum slots to prime in this run.
    #[arg(long)]
    pub max_slots: Option<usize>,

    /// Real Codex binary.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pub codex_bin: Option<PathBuf>,

    /// Model to use for the tiny priming request.
    #[arg(long)]
    pub model: Option<String>,

    /// Prompt used for the tiny priming request.
    #[arg(long)]
    pub prompt: Option<String>,

    /// Seconds to poll live usage after each prime request.
    #[arg(long)]
    pub verify_timeout: Option<u64>,

    /// Maximum Codex requests to send per slot while trying to verify active.
    #[arg(long)]
    pub max_requests: Option<usize>,

    /// Send a request even when the 5h window already looks active.
    #[arg(long)]
    pub force: bool,

    /// Print what would run without sending requests.
    #[arg(long)]
    pub dry_run: bool,

    /// Print JSON instead of a human report.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct PrimeStatusArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Print JSON instead of a human report.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct PrimeUninstallArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Also remove saved prime config and state.
    #[arg(long)]
    pub delete_state: bool,
}

#[derive(Debug, Clone, Args)]
pub struct PrimeScheduleArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Days of local history used to infer heavy work hours.
    #[arg(long, default_value_t = 30)]
    pub days: u32,

    /// Minutes before a heavy work hour to send the tiny request.
    #[arg(long, default_value_t = 210)]
    pub lead_minutes: u32,

    /// Maximum daily launchd times to install.
    #[arg(long, default_value_t = 6)]
    pub max_times: usize,

    /// Ignore hours below this token volume unless all hours are below it.
    #[arg(long, default_value_t = 20_000_000)]
    pub min_tokens: u64,
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
        assert!(!args.no_progress);
        assert_eq!(args.query.jobs, crate::selector::DEFAULT_SLOT_QUERY_JOBS);
        assert_eq!(
            args.query.retries,
            crate::selector::DEFAULT_SLOT_QUERY_RETRIES
        );
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
    fn status_accepts_network_tuning_options() {
        let cli = Cli::parse_from(["cx", "status", "--jobs", "2", "--retries", "3"]);

        let Command::Status(args) = cli.command else {
            panic!("expected status command");
        };
        assert_eq!(args.query.jobs, 2);
        assert_eq!(args.query.retries, 3);
    }

    #[test]
    fn status_accepts_no_progress() {
        let cli = Cli::parse_from(["cx", "status", "--no-progress"]);

        let Command::Status(args) = cli.command else {
            panic!("expected status command");
        };
        assert!(args.no_progress);
    }

    #[test]
    fn status_accepts_no_cache() {
        let cli = Cli::parse_from(["cx", "status", "--no-cache"]);

        let Command::Status(args) = cli.command else {
            panic!("expected status command");
        };
        assert!(args.query.no_cache);
    }

    #[test]
    fn stats_range_accepts_dynamic_relative_days() {
        let cli = Cli::parse_from(["cx", "stats", "--range", "17d"]);

        let Command::Stats(args) = cli.command else {
            panic!("expected stats command");
        };
        assert_eq!(args.range.key(), "17d");
        assert_eq!(args.range.label(), "Last 17 days");
    }

    #[test]
    fn stats_range_rejects_invalid_calendar_dates() {
        let result = Cli::try_parse_from(["cx", "stats", "--range", "2026-02-31"]);

        assert!(result.is_err());
    }

    #[test]
    fn prime_install_accepts_schedule_and_run_options() {
        let cli = Cli::parse_from([
            "cx",
            "prime",
            "install",
            "--lead-minutes",
            "180",
            "--max-times",
            "4",
            "--target",
            "work",
            "--slot",
            "bus1",
            "--max-slots",
            "2",
            "--model",
            "gpt-5.4-mini",
            "--verify-timeout",
            "20",
            "--max-requests",
            "2",
        ]);

        let Command::Prime(args) = cli.command else {
            panic!("expected prime command");
        };
        let PrimeCommand::Install(args) = args.command else {
            panic!("expected prime install command");
        };
        assert_eq!(args.schedule.lead_minutes, 180);
        assert_eq!(args.schedule.max_times, 4);
        assert_eq!(args.target, Some(String::from("work")));
        assert_eq!(args.slots, vec![String::from("bus1")]);
        assert_eq!(args.max_slots, Some(2));
        assert_eq!(args.model, Some(String::from("gpt-5.4-mini")));
        assert_eq!(args.verify_timeout, 20);
        assert_eq!(args.max_requests, 2);
    }

    #[test]
    fn prime_run_accepts_verification_policy() {
        let cli = Cli::parse_from([
            "cx",
            "prime",
            "run",
            "--slot",
            "bus1",
            "--verify-timeout",
            "30",
            "--max-requests",
            "3",
        ]);

        let Command::Prime(args) = cli.command else {
            panic!("expected prime command");
        };
        let PrimeCommand::Run(args) = args.command else {
            panic!("expected prime run command");
        };
        assert_eq!(args.slots, vec![String::from("bus1")]);
        assert_eq!(args.verify_timeout, Some(30));
        assert_eq!(args.max_requests, Some(3));
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
            "/Applications/ChatGPT.app",
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
        assert_eq!(
            args.app_bin,
            Some(PathBuf::from("/Applications/ChatGPT.app"))
        );
        assert!(args.wait);
        assert!(args.allow_parallel);
        assert_eq!(args.args, vec![String::from("--enable-logging")]);
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
pub struct PatArgs {
    #[command(subcommand)]
    pub command: PatCommand,
}

impl PatCommand {
    pub fn manager_dir(&self) -> Option<&std::path::Path> {
        match self {
            Self::Add(args) => args.manager_dir.as_deref(),
            Self::Check(args) => args.manager_dir.as_deref(),
            Self::Remove(args) => args.manager_dir.as_deref(),
            Self::Refresh(args) => args.manager_dir.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum PatCommand {
    /// Bind a Keychain PAT fallback to a slot and hydrate metadata.
    Add(PatAddArgs),
    /// Verify the Keychain entry exists and the PAT is valid.
    Check(PatCheckArgs),
    /// Remove the Keychain reference and metadata cache from a slot.
    Remove(PatRemoveArgs),
    /// Force re-hydrate metadata from the whoami endpoint.
    Refresh(PatRefreshArgs),
}

#[derive(Debug, Clone, Args)]
pub struct PatAddArgs {
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Slot name.
    pub slot: String,

    /// Keychain service name (e.g. codex-pat).
    #[arg(long)]
    pub service: String,

    /// Keychain account name (e.g. the user's email).
    #[arg(long)]
    pub account: String,

    /// Deprecated compatibility flag; retained only until 0.5.
    #[arg(long, hide = true)]
    pub force: bool,
}

#[derive(Debug, Clone, Args)]
pub struct PatCheckArgs {
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Slot name.
    pub slot: String,
}

#[derive(Debug, Clone, Args)]
pub struct PatRemoveArgs {
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Slot name.
    pub slot: String,
}

#[derive(Debug, Clone, Args)]
pub struct PatRefreshArgs {
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Slot name.
    pub slot: String,
}

#[derive(Debug, Clone, Args)]
pub struct DesktopArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Path to the ChatGPT Desktop executable or .app bundle.
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

    /// Wait for ChatGPT Desktop to exit instead of returning after launch.
    #[arg(long)]
    pub wait: bool,

    /// Launch even when another ChatGPT Desktop process is already running.
    #[arg(long)]
    pub allow_parallel: bool,

    /// Extra args forwarded to the ChatGPT Desktop executable.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
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

    /// Maximum number of slot usage requests to run at once.
    #[arg(long, default_value_t = crate::selector::DEFAULT_SLOT_QUERY_JOBS)]
    pub jobs: usize,

    /// Retry transient usage request failures this many times.
    #[arg(long, default_value_t = crate::selector::DEFAULT_SLOT_QUERY_RETRIES)]
    pub retries: usize,
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

#[derive(Debug, Clone, Args)]
pub struct MergeSqliteArgs {
    /// Profile-manager directory. Defaults to ~/.codex/profile-manager.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    pub manager_dir: Option<PathBuf>,

    /// Show what would be merged without writing.
    #[arg(long)]
    pub dry_run: bool,
}
