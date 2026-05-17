use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::Error;
use rusqlite::ErrorCode;
use rusqlite::OpenFlags;
use serde::Deserialize;
use serde::Serialize;

use crate::cache::CacheStore;
use crate::cli::PrimeInstallArgs;
use crate::cli::PrimePlanArgs;
use crate::cli::PrimeRunArgs;
use crate::cli::PrimeScheduleArgs;
use crate::cli::PrimeStatusArgs;
use crate::cli::PrimeUninstallArgs;
use crate::paths;
use crate::paths::ManagerPaths;
use crate::run;
use crate::selector;
use crate::slot;
use crate::target;
use crate::usage::format_refresh_in;
use crate::usage::SlotResult;
use crate::usage::SlotStatus;

pub const DEFAULT_PRIME_PROMPT: &str = "Reply exactly: hi";

const PRIME_SCHEMA_VERSION: u64 = 2;
const PRIME_CONFIG: &str = "prime/config.json";
const PRIME_STATE: &str = "prime/state.json";
const PRIME_DIR: &str = "prime";
const LAUNCHD_LABEL: &str = "dev.xiaotian.cx.prime";
const LAUNCHD_PLIST: &str = "dev.xiaotian.cx.prime.plist";
const ROLLOUT_CACHE: &str = "stats-rollout-cache.sqlite";
const STATE_DB: &str = "state_5.sqlite";
const MAX_PROMPT_BYTES: usize = 2_000;
const PRIME_TIMEOUT_SECONDS: u64 = 90;
const FIVE_HOUR_WINDOW_SECONDS: i64 = 5 * 60 * 60;
const ACTIVE_REFRESH_GRACE_SECONDS: i64 = 5 * 60;
const MAX_IDLE_FIVE_HOUR_USED_PERCENT: f64 = 1.0;
const START_HOUR_WEIGHT: f64 = 1.0;
const ROLLOUT_HOUR_WEIGHT: f64 = 0.35;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimePlanReport {
    schema_version: u64,
    days: u32,
    lead_minutes: u32,
    max_times: usize,
    min_tokens: u64,
    source_databases: Vec<String>,
    rollout_cache: Option<String>,
    schedules: Vec<PrimeScheduleTime>,
    hour_scores: Vec<PrimeHourScore>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PrimeScheduleTime {
    hour: u8,
    minute: u8,
    source_hour: u8,
    score: f64,
    start_tokens: u64,
    rollout_tokens: u64,
    threads: u64,
    rollout_samples: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrimeHourScore {
    hour: u8,
    score: f64,
    start_tokens: u64,
    rollout_tokens: u64,
    threads: u64,
    rollout_samples: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrimeConfig {
    schema_version: u64,
    installed_at: i64,
    days: u32,
    lead_minutes: u32,
    max_times: usize,
    min_tokens: u64,
    schedules: Vec<PrimeScheduleTime>,
    target: Option<String>,
    slots: Vec<String>,
    #[serde(default)]
    max_slots: Option<usize>,
    codex_bin: Option<PathBuf>,
    model: Option<String>,
    prompt: String,
    timeout: f32,
    jobs: usize,
    retries: usize,
    min_weekly_remaining: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrimeState {
    schema_version: u64,
    updated_at: i64,
    #[serde(default, rename = "lastSuccessfulPrimes")]
    last_successful_primes: BTreeMap<String, i64>,
    last_run: Option<PrimeRunReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrimeRunReport {
    schema_version: u64,
    ran_at: i64,
    dry_run: bool,
    force: bool,
    checked_slots: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_slots: Option<usize>,
    primed: Vec<PrimeAttempt>,
    skipped: Vec<PrimeSkip>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrimeAttempt {
    slot: String,
    account: Option<String>,
    dry_run: bool,
    success: bool,
    exit_code: Option<i32>,
    elapsed_ms: u128,
    note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrimeSkip {
    slot: String,
    reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PrimeStatusReport {
    schema_version: u64,
    launch_agent_path: String,
    launch_agent_installed: bool,
    launch_agent_loaded: bool,
    config: Option<PrimeConfig>,
    state: Option<PrimeState>,
}

#[derive(Debug, Clone)]
struct EffectiveRunConfig {
    target: Option<String>,
    slots: Vec<String>,
    max_slots: Option<usize>,
    codex_bin: Option<PathBuf>,
    model: Option<String>,
    prompt: String,
    timeout: f32,
    jobs: usize,
    retries: usize,
    min_weekly_remaining: f64,
}

#[derive(Debug, Clone)]
struct PrimeCandidate<'a> {
    result: &'a SlotResult,
    weekly_remaining: f64,
}

struct NoProgress;

impl selector::SlotQueryProgress for NoProgress {}

pub fn plan(paths: &ManagerPaths, args: PrimePlanArgs) -> Result<()> {
    let report = build_plan(paths, &args.schedule)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_plan(&report);
    }
    Ok(())
}

pub fn install(paths: &ManagerPaths, args: PrimeInstallArgs) -> Result<()> {
    let report = build_plan(paths, &args.schedule)?;
    if report.schedules.is_empty() {
        anyhow::bail!("no prime schedule could be inferred from local usage");
    }

    let codex_bin = args
        .codex_bin
        .clone()
        .map(Ok)
        .unwrap_or_else(|| run::resolve_codex_bin(None))?;
    let config = PrimeConfig {
        schema_version: PRIME_SCHEMA_VERSION,
        installed_at: now_unix(),
        days: args.schedule.days,
        lead_minutes: args.schedule.lead_minutes,
        max_times: args.schedule.max_times,
        min_tokens: args.schedule.min_tokens,
        schedules: report.schedules.clone(),
        target: args.target.clone(),
        slots: args.slots.clone(),
        max_slots: args.max_slots.map(|max_slots| max_slots.max(1)),
        codex_bin: Some(codex_bin),
        model: args.model.clone(),
        prompt: validate_prompt(&args.prompt)?,
        timeout: args.timeout.max(0.1),
        jobs: args.jobs.max(1),
        retries: args.retries,
        min_weekly_remaining: args.min_weekly_remaining.clamp(0.0, 100.0),
    };
    save_config(paths, &config)?;

    let cx_bin = std::env::current_exe().context("resolve current cx executable")?;
    let plist = write_launch_agent(paths, &cx_bin, &config.schedules)?;
    load_launch_agent(&plist)?;

    println!(
        "saved prime config: {}",
        cache_path(paths, PRIME_CONFIG).display()
    );
    println!("installed LaunchAgent: {}", plist.display());
    print_plan(&report);

    if args.run_now {
        run(
            paths,
            PrimeRunArgs {
                manager_dir: Some(paths.manager_dir.clone()),
                target: None,
                slots: Vec::new(),
                max_slots: None,
                codex_bin: None,
                model: None,
                prompt: None,
                force: false,
                dry_run: false,
                json: false,
            },
        )?;
    }

    Ok(())
}

pub fn run(paths: &ManagerPaths, args: PrimeRunArgs) -> Result<()> {
    let config = load_config(paths);
    let previous_state = load_state(paths);
    let effective = EffectiveRunConfig::from_config_and_args(config.as_ref(), &args)?;
    let report = run_prime_check(paths, &effective, args.force, args.dry_run)?;
    save_state(
        paths,
        &PrimeState {
            schema_version: PRIME_SCHEMA_VERSION,
            updated_at: now_unix(),
            last_successful_primes: updated_prime_history(previous_state.as_ref(), &report),
            last_run: Some(report.clone()),
        },
    )?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_run_report(&report);
    }
    Ok(())
}

pub fn status(paths: &ManagerPaths, args: PrimeStatusArgs) -> Result<()> {
    let plist = launch_agent_path()?;
    let report = PrimeStatusReport {
        schema_version: PRIME_SCHEMA_VERSION,
        launch_agent_installed: plist.is_file(),
        launch_agent_loaded: launch_agent_loaded(),
        launch_agent_path: plist.display().to_string(),
        config: load_config(paths),
        state: load_state(paths),
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_status_report(&report);
    }
    Ok(())
}

pub fn uninstall(paths: &ManagerPaths, args: PrimeUninstallArgs) -> Result<()> {
    let plist = launch_agent_path()?;
    if plist.exists() {
        unload_launch_agent(&plist);
        fs::remove_file(&plist).with_context(|| format!("remove {}", plist.display()))?;
        println!("removed LaunchAgent: {}", plist.display());
    } else {
        println!("LaunchAgent not installed: {}", plist.display());
    }

    if args.delete_state {
        let store = CacheStore::new(paths);
        let removed_config = store.remove_file_if_present(PRIME_CONFIG)?;
        let removed_state = store.remove_file_if_present(PRIME_STATE)?;
        let dir = cache_path(paths, PRIME_DIR);
        let _ignored = fs::remove_dir(&dir);
        if removed_config {
            println!(
                "removed config: {}",
                cache_path(paths, PRIME_CONFIG).display()
            );
        }
        if removed_state {
            println!(
                "removed state: {}",
                cache_path(paths, PRIME_STATE).display()
            );
        }
    }

    Ok(())
}

impl EffectiveRunConfig {
    fn from_config_and_args(config: Option<&PrimeConfig>, args: &PrimeRunArgs) -> Result<Self> {
        let target = args
            .target
            .clone()
            .or_else(|| config.and_then(|config| config.target.clone()));
        let slots = if args.slots.is_empty() {
            config
                .map(|config| config.slots.clone())
                .unwrap_or_default()
        } else {
            args.slots.clone()
        };
        let prompt = args
            .prompt
            .clone()
            .or_else(|| config.map(|config| config.prompt.clone()))
            .unwrap_or_else(|| DEFAULT_PRIME_PROMPT.to_string());

        Ok(Self {
            target,
            slots,
            max_slots: args
                .max_slots
                .map(|max_slots| max_slots.max(1))
                .or_else(|| config.and_then(|config| config.max_slots)),
            codex_bin: args
                .codex_bin
                .clone()
                .or_else(|| config.and_then(|config| config.codex_bin.clone())),
            model: args
                .model
                .clone()
                .or_else(|| config.and_then(|config| config.model.clone())),
            prompt: validate_prompt(&prompt)?,
            timeout: config.map(|config| config.timeout).unwrap_or(2.0).max(0.1),
            jobs: config.map(|config| config.jobs).unwrap_or(4).max(1),
            retries: config.map(|config| config.retries).unwrap_or(1),
            min_weekly_remaining: config
                .map(|config| config.min_weekly_remaining)
                .unwrap_or(5.0)
                .clamp(0.0, 100.0),
        })
    }
}

fn build_plan(paths: &ManagerPaths, args: &PrimeScheduleArgs) -> Result<PrimePlanReport> {
    let days = args.days.max(1);
    let cutoff = now_unix().saturating_sub(i64::from(days) * 24 * 60 * 60);
    let mut source_databases = Vec::new();
    let mut scores = BTreeMap::<u8, PrimeHourScore>::new();
    let mut seen_threads = HashSet::new();

    for db_path in state_db_paths(paths)? {
        if !db_path.is_file() {
            continue;
        }
        source_databases.push(db_path.display().to_string());
        add_state_db_hour_scores(&db_path, cutoff, &mut seen_threads, &mut scores)?;
    }

    let rollout_cache = cache_path(paths, ROLLOUT_CACHE);
    let rollout_cache_report = if rollout_cache.is_file() {
        add_rollout_hour_scores(&rollout_cache, cutoff, &mut scores)?;
        Some(rollout_cache.display().to_string())
    } else {
        None
    };

    let mut hour_scores = scores.into_values().collect::<Vec<_>>();
    hour_scores.sort_by(compare_hour_scores);

    let schedules = schedule_times_from_scores(
        &hour_scores,
        args.lead_minutes,
        args.max_times.max(1),
        args.min_tokens,
    );

    Ok(PrimePlanReport {
        schema_version: PRIME_SCHEMA_VERSION,
        days,
        lead_minutes: args.lead_minutes,
        max_times: args.max_times.max(1),
        min_tokens: args.min_tokens,
        source_databases,
        rollout_cache: rollout_cache_report,
        schedules,
        hour_scores,
    })
}

fn add_state_db_hour_scores(
    db_path: &Path,
    cutoff: i64,
    seen_threads: &mut HashSet<String>,
    scores: &mut BTreeMap<u8, PrimeHourScore>,
) -> Result<()> {
    let conn = open_query_only_connection(db_path)?;
    let mut statement = conn
        .prepare(
            "SELECT id,
                    CAST(strftime('%H', created_at, 'unixepoch', 'localtime') AS INTEGER),
                    COALESCE(tokens_used, 0)
             FROM threads
             WHERE created_at >= ?1 AND tokens_used > 0",
        )
        .with_context(|| format!("prepare prime state query for {}", db_path.display()))?;
    let rows = statement.query_map(params![cutoff], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    for row in rows {
        let (id, hour, tokens) = row?;
        if !seen_threads.insert(id) {
            continue;
        }
        let Some(hour) = normalize_hour(hour) else {
            continue;
        };
        let tokens = tokens.max(0) as u64;
        let entry = scores.entry(hour).or_insert_with(|| PrimeHourScore {
            hour,
            ..PrimeHourScore::default()
        });
        entry.threads += 1;
        entry.start_tokens = entry.start_tokens.saturating_add(tokens);
        entry.score += tokens as f64 * START_HOUR_WEIGHT;
    }
    Ok(())
}

fn add_rollout_hour_scores(
    db_path: &Path,
    cutoff: i64,
    scores: &mut BTreeMap<u8, PrimeHourScore>,
) -> Result<()> {
    let conn = open_query_only_connection(db_path)?;
    let mut statement = conn
        .prepare(
            "SELECT CAST(strftime('%H', timestamp_unix, 'unixepoch', 'localtime') AS INTEGER),
                    COALESCE(SUM(total_tokens), 0),
                    COALESCE(SUM(samples), 0)
             FROM rollout_events
             WHERE timestamp_unix >= ?1
             GROUP BY 1",
        )
        .with_context(|| format!("prepare prime rollout query for {}", db_path.display()))?;
    let rows = statement.query_map(params![cutoff], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    for row in rows {
        let (hour, tokens, samples) = row?;
        let Some(hour) = normalize_hour(hour) else {
            continue;
        };
        let tokens = tokens.max(0) as u64;
        let samples = samples.max(0) as u64;
        let entry = scores.entry(hour).or_insert_with(|| PrimeHourScore {
            hour,
            ..PrimeHourScore::default()
        });
        entry.rollout_tokens = entry.rollout_tokens.saturating_add(tokens);
        entry.rollout_samples = entry.rollout_samples.saturating_add(samples);
        entry.score += tokens as f64 * ROLLOUT_HOUR_WEIGHT;
    }
    Ok(())
}

fn open_query_only_connection(db_path: &Path) -> Result<Connection> {
    match open_validated_connection(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => Ok(conn),
        Err(err) if is_cannot_open(&err) => {
            let conn = Connection::open_with_flags(
                db_path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
            )
            .and_then(|conn| {
                conn.pragma_update(None, "query_only", true)?;
                validate_connection(&conn)?;
                Ok(conn)
            })
            .with_context(|| {
                format!(
                    "open {} read-write query-only after read-only open failed",
                    db_path.display()
                )
            })?;
            Ok(conn)
        }
        Err(err) => Err(err).with_context(|| format!("open {}", db_path.display())),
    }
}

fn open_validated_connection(
    db_path: &Path,
    flags: OpenFlags,
) -> std::result::Result<Connection, Error> {
    let conn = Connection::open_with_flags(db_path, flags)?;
    validate_connection(&conn)?;
    Ok(conn)
}

fn validate_connection(conn: &Connection) -> std::result::Result<(), Error> {
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))
}

fn is_cannot_open(err: &Error) -> bool {
    matches!(err, Error::SqliteFailure(error, _) if error.code == ErrorCode::CannotOpen)
}

fn schedule_times_from_scores(
    hour_scores: &[PrimeHourScore],
    lead_minutes: u32,
    max_times: usize,
    min_tokens: u64,
) -> Vec<PrimeScheduleTime> {
    let mut candidates = hour_scores
        .iter()
        .filter(|score| score.start_tokens.max(score.rollout_tokens) >= min_tokens)
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        candidates = hour_scores.iter().take(max_times).collect();
    }

    let mut by_time = BTreeMap::<(u8, u8), PrimeScheduleTime>::new();
    for score in candidates {
        let minutes = prime_minutes_before(score.hour, lead_minutes);
        let time_key = ((minutes / 60) as u8, (minutes % 60) as u8);
        let schedule = PrimeScheduleTime {
            hour: time_key.0,
            minute: time_key.1,
            source_hour: score.hour,
            score: score.score,
            start_tokens: score.start_tokens,
            rollout_tokens: score.rollout_tokens,
            threads: score.threads,
            rollout_samples: score.rollout_samples,
        };
        by_time
            .entry(time_key)
            .and_modify(|existing| {
                if schedule.score > existing.score {
                    *existing = schedule.clone();
                }
            })
            .or_insert(schedule);
    }

    let mut schedules = by_time.into_values().collect::<Vec<_>>();
    schedules.sort_by(compare_schedule_scores);
    schedules.truncate(max_times);
    schedules.sort_by_key(|schedule| (schedule.hour, schedule.minute));
    schedules
}

fn run_prime_check(
    paths: &ManagerPaths,
    config: &EffectiveRunConfig,
    force: bool,
    dry_run: bool,
) -> Result<PrimeRunReport> {
    let target = target::load_optional_target(paths, config.target.as_deref())?;
    let slots = if !config.slots.is_empty() {
        config.slots.clone()
    } else if let Some(target) = target.as_ref() {
        target.slots_or_rotation(paths)?
    } else {
        slot::load_rotation(paths)?
    };
    if slots.is_empty() {
        anyhow::bail!("no slots configured for prime run");
    }

    let mut progress = NoProgress;
    let results = selector::query_slots_with_progress(
        paths,
        &slots,
        selector::SlotQueryOptions::new(config.timeout, config.jobs, config.retries),
        &mut progress,
    )?;
    let now = now_unix();
    let mut skipped = Vec::new();
    let mut candidates = Vec::new();

    for result in &results {
        match prime_candidate(result, now, config.min_weekly_remaining, force) {
            Ok(candidate) => candidates.push(candidate),
            Err(reason) => skipped.push(PrimeSkip {
                slot: result.slot.clone(),
                reason,
            }),
        }
    }
    candidates.sort_by(compare_candidates);

    let real_codex = if dry_run || candidates.is_empty() {
        None
    } else {
        Some(run::resolve_codex_bin(config.codex_bin.as_deref())?)
    };
    let max_slots = config.max_slots.unwrap_or(usize::MAX);
    let selected_candidates = candidates.into_iter().take(max_slots).collect::<Vec<_>>();
    let mut primed = Vec::new();
    if dry_run {
        for candidate in selected_candidates {
            primed.push(PrimeAttempt {
                slot: candidate.result.slot.clone(),
                account: candidate.result.account_label.clone(),
                dry_run: true,
                success: true,
                exit_code: None,
                elapsed_ms: 0,
                note: format!(
                    "would prime; weekly remaining {:.1}%",
                    candidate.weekly_remaining
                ),
            });
        }
    } else if let Some(real_codex) = real_codex.as_ref() {
        thread::scope(|scope| {
            let mut handles = Vec::with_capacity(selected_candidates.len());
            for candidate in selected_candidates {
                let real_codex = real_codex.as_path();
                let target = target.as_ref();
                handles.push(scope.spawn(move || {
                    run_prime_slot(
                        paths,
                        real_codex,
                        &candidate.result.slot,
                        target,
                        config,
                        candidate.result.account_label.clone(),
                    )
                }));
            }

            for handle in handles {
                match handle.join() {
                    Ok(attempt) => primed.push(attempt),
                    Err(_panic) => primed.push(PrimeAttempt {
                        slot: "<unknown>".to_string(),
                        account: None,
                        dry_run: false,
                        success: false,
                        exit_code: None,
                        elapsed_ms: 0,
                        note: "prime worker panicked".to_string(),
                    }),
                }
            }
        });
    }

    Ok(PrimeRunReport {
        schema_version: PRIME_SCHEMA_VERSION,
        ran_at: now,
        dry_run,
        force,
        checked_slots: results.len(),
        max_slots: config.max_slots,
        primed,
        skipped,
    })
}

fn prime_candidate<'a>(
    result: &'a SlotResult,
    now: i64,
    min_weekly_remaining: f64,
    force: bool,
) -> std::result::Result<PrimeCandidate<'a>, String> {
    if result.status != SlotStatus::Available {
        return Err(format!("status {}", result.status.as_str()));
    }
    if result.stale && !force {
        return Err("stale usage cache".to_string());
    }

    let weekly_remaining = remaining_percent(result.weekly_used_percent);
    if weekly_remaining < min_weekly_remaining {
        return Err(format!(
            "weekly remaining {:.1}% below {:.1}%",
            weekly_remaining, min_weekly_remaining
        ));
    }

    if !force {
        if let Some(reason) = remote_active_five_hour_window_reason(result, now) {
            return Err(reason);
        }
    }

    Ok(PrimeCandidate {
        result,
        weekly_remaining,
    })
}

fn remote_active_five_hour_window_reason(result: &SlotResult, now: i64) -> Option<String> {
    let used = result.five_hour_used_percent.unwrap_or(0.0);
    let remaining = result
        .five_hour_refresh_at
        .map(|refresh_at| refresh_at.saturating_sub(now));
    let full_like_window = remaining.is_some_and(|remaining| {
        remaining >= FIVE_HOUR_WINDOW_SECONDS - ACTIVE_REFRESH_GRACE_SECONDS
    });
    let refresh_in_future =
        remaining.is_some_and(|remaining| remaining > ACTIVE_REFRESH_GRACE_SECONDS);

    if used <= MAX_IDLE_FIVE_HOUR_USED_PERCENT && full_like_window {
        return None;
    }
    if used <= 0.0 || !refresh_in_future {
        return None;
    }

    let refresh = result
        .five_hour_refresh_at
        .and_then(format_refresh_in)
        .map(|refresh| format!(", refresh {refresh}"))
        .unwrap_or_default();
    Some(format!(
        "5h window already active (used {used:.1}%{refresh})"
    ))
}

fn updated_prime_history(
    previous_state: Option<&PrimeState>,
    report: &PrimeRunReport,
) -> BTreeMap<String, i64> {
    let mut history = previous_state
        .map(|state| state.last_successful_primes.clone())
        .unwrap_or_default();
    let cutoff = report
        .ran_at
        .saturating_sub(FIVE_HOUR_WINDOW_SECONDS.saturating_mul(2));
    history.retain(|_, primed_at| *primed_at >= cutoff);
    if !report.dry_run {
        for attempt in &report.primed {
            if attempt.success {
                history.insert(attempt.slot.clone(), report.ran_at);
            }
        }
    }
    history
}

fn run_prime_slot(
    paths: &ManagerPaths,
    real_codex: &Path,
    slot: &str,
    target: Option<&target::TargetSpec>,
    config: &EffectiveRunConfig,
    account: Option<String>,
) -> PrimeAttempt {
    let started = Instant::now();
    let result = run_prime_slot_inner(paths, real_codex, slot, target, config);
    let elapsed_ms = started.elapsed().as_millis();
    match result {
        Ok(outcome) => PrimeAttempt {
            slot: slot.to_string(),
            account,
            dry_run: false,
            success: outcome.success,
            exit_code: outcome.exit_code,
            elapsed_ms,
            note: outcome.note,
        },
        Err(err) => PrimeAttempt {
            slot: slot.to_string(),
            account,
            dry_run: false,
            success: false,
            exit_code: None,
            elapsed_ms,
            note: format!("{err:#}"),
        },
    }
}

fn run_prime_slot_inner(
    paths: &ManagerPaths,
    real_codex: &Path,
    slot: &str,
    target: Option<&target::TargetSpec>,
    config: &EffectiveRunConfig,
) -> Result<CommandOutcome> {
    let spec = run::build_slot_command_spec(
        paths,
        real_codex.to_path_buf(),
        slot,
        target,
        prime_codex_args(config),
    )?;
    let mut command = spec.into_command();
    command.current_dir(paths::home_dir()?);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let output = wait_with_timeout(command, Duration::from_secs(PRIME_TIMEOUT_SECONDS))?;
    let exit_code = output.status.code();
    let success = output.status.success();
    let note = if success {
        "primed".to_string()
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let summary = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("codex exec failed");
        format!("codex exec failed: {}", truncate(summary, 240))
    };
    Ok(CommandOutcome {
        success,
        exit_code,
        note,
    })
}

fn prime_codex_args(config: &EffectiveRunConfig) -> Vec<OsString> {
    let mut args = Vec::new();
    if let Some(model) = config.model.as_deref() {
        args.push(OsString::from("-m"));
        args.push(OsString::from(model));
    }
    args.push(OsString::from("--ask-for-approval"));
    args.push(OsString::from("never"));
    args.push(OsString::from("exec"));
    args.push(OsString::from("--skip-git-repo-check"));
    args.push(OsString::from("--ephemeral"));
    args.push(OsString::from("--ignore-rules"));
    args.push(OsString::from("-s"));
    args.push(OsString::from("read-only"));
    args.push(OsString::from("--color"));
    args.push(OsString::from("never"));
    args.push(OsString::from(config.prompt.clone()));
    args
}

#[derive(Debug, Clone)]
struct CommandOutcome {
    success: bool,
    exit_code: Option<i32>,
    note: String,
}

fn wait_with_timeout(mut command: Command, timeout: Duration) -> Result<std::process::Output> {
    let mut child = command.spawn().context("spawn codex prime command")?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().context("read codex prime output");
        }
        if started.elapsed() >= timeout {
            let _ignored = child.kill();
            let _ignored = child
                .wait_with_output()
                .context("read timed-out prime output")?;
            anyhow::bail!("codex prime command timed out after {}s", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn save_config(paths: &ManagerPaths, config: &PrimeConfig) -> Result<()> {
    CacheStore::new(paths)
        .write_json(PRIME_CONFIG, config)
        .map(|_| ())
}

fn load_config(paths: &ManagerPaths) -> Option<PrimeConfig> {
    CacheStore::new(paths)
        .read_json(PRIME_CONFIG, |config: &PrimeConfig| {
            matches!(config.schema_version, 1 | PRIME_SCHEMA_VERSION)
        })
        .map(|mut config| {
            if config.schema_version == 1 && config.max_slots == Some(3) {
                config.max_slots = None;
            }
            config
        })
}

fn save_state(paths: &ManagerPaths, state: &PrimeState) -> Result<()> {
    CacheStore::new(paths)
        .write_json(PRIME_STATE, state)
        .map(|_| ())
}

fn load_state(paths: &ManagerPaths) -> Option<PrimeState> {
    CacheStore::new(paths).read_json(PRIME_STATE, |state: &PrimeState| {
        state.schema_version == PRIME_SCHEMA_VERSION
    })
}

fn write_launch_agent(
    paths: &ManagerPaths,
    cx_bin: &Path,
    schedules: &[PrimeScheduleTime],
) -> Result<PathBuf> {
    let plist = launch_agent_path()?;
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::create_dir_all(cache_path(paths, PRIME_DIR))
        .with_context(|| format!("create {}", cache_path(paths, PRIME_DIR).display()))?;

    let mut intervals = String::new();
    for schedule in schedules {
        intervals.push_str("    <dict>\n");
        intervals.push_str(&format!(
            "      <key>Hour</key><integer>{}</integer>\n",
            schedule.hour
        ));
        intervals.push_str(&format!(
            "      <key>Minute</key><integer>{}</integer>\n",
            schedule.minute
        ));
        intervals.push_str("    </dict>\n");
    }

    let stdout_path = cache_path(paths, "prime/launchd.out.log");
    let stderr_path = cache_path(paths, "prime/launchd.err.log");
    let home = paths::home_dir()?;
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{cx_bin}</string>
    <string>prime</string>
    <string>run</string>
    <string>--manager-dir</string>
    <string>{manager_dir}</string>
  </array>
  <key>StartCalendarInterval</key>
  <array>
{intervals}  </array>
  <key>WorkingDirectory</key>
  <string>{home}</string>
  <key>StandardOutPath</key>
  <string>{stdout_path}</string>
  <key>StandardErrorPath</key>
  <string>{stderr_path}</string>
</dict>
</plist>
"#,
        label = xml_escape(LAUNCHD_LABEL),
        cx_bin = xml_escape(&cx_bin.display().to_string()),
        manager_dir = xml_escape(&paths.manager_dir.display().to_string()),
        intervals = intervals,
        home = xml_escape(&home.display().to_string()),
        stdout_path = xml_escape(&stdout_path.display().to_string()),
        stderr_path = xml_escape(&stderr_path.display().to_string()),
    );

    fs::write(&plist, content).with_context(|| format!("write {}", plist.display()))?;
    Ok(plist)
}

fn load_launch_agent(plist: &Path) -> Result<()> {
    unload_launch_agent(plist);
    let domain = launchctl_domain()?;
    run_launchctl(&["bootstrap", &domain, &plist.display().to_string()])?;
    run_launchctl(&["enable", &format!("{domain}/{LAUNCHD_LABEL}")])?;
    Ok(())
}

fn unload_launch_agent(plist: &Path) {
    let Ok(domain) = launchctl_domain() else {
        return;
    };
    let _ignored = Command::new("launchctl")
        .arg("bootout")
        .arg(domain)
        .arg(plist)
        .output();
}

fn launch_agent_loaded() -> bool {
    let Ok(domain) = launchctl_domain() else {
        return false;
    };
    Command::new("launchctl")
        .arg("print")
        .arg(format!("{domain}/{LAUNCHD_LABEL}"))
        .output()
        .is_ok_and(|output| output.status.success())
}

fn run_launchctl(args: &[&str]) -> Result<()> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .context("run launchctl")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!("launchctl {} failed: {}", args.join(" "), stderr.trim());
}

fn launchctl_domain() -> Result<String> {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions and returns the current process user id.
        let uid = unsafe { libc::getuid() };
        Ok(format!("gui/{uid}"))
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("launchd is only supported on Unix platforms")
    }
}

fn launch_agent_path() -> Result<PathBuf> {
    Ok(paths::home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join(LAUNCHD_PLIST))
}

fn state_db_paths(paths: &ManagerPaths) -> Result<Vec<PathBuf>> {
    let mut dbs = Vec::new();
    dbs.push(paths.base_codex_home.join(STATE_DB));
    if paths.slots_dir.is_dir() {
        for entry in fs::read_dir(&paths.slots_dir)
            .with_context(|| format!("read {}", paths.slots_dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            dbs.push(entry.path().join("home/sqlite").join(STATE_DB));
        }
    }
    dbs.sort();
    dbs.dedup();
    Ok(dbs)
}

fn cache_path(paths: &ManagerPaths, relative: impl AsRef<Path>) -> PathBuf {
    paths.manager_dir.join(relative)
}

fn validate_prompt(prompt: &str) -> Result<String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        anyhow::bail!("prime prompt cannot be empty");
    }
    if prompt.len() > MAX_PROMPT_BYTES {
        anyhow::bail!("prime prompt is too long; keep it below {MAX_PROMPT_BYTES} bytes");
    }
    Ok(prompt.to_string())
}

fn normalize_hour(hour: i64) -> Option<u8> {
    (0..=23).contains(&hour).then_some(hour as u8)
}

fn prime_minutes_before(source_hour: u8, lead_minutes: u32) -> u32 {
    let day_minutes = 24 * 60;
    let source = u32::from(source_hour) * 60;
    (source + day_minutes - (lead_minutes % day_minutes)) % day_minutes
}

fn compare_hour_scores(left: &PrimeHourScore, right: &PrimeHourScore) -> Ordering {
    right
        .score
        .partial_cmp(&left.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| right.start_tokens.cmp(&left.start_tokens))
        .then_with(|| left.hour.cmp(&right.hour))
}

fn compare_schedule_scores(left: &PrimeScheduleTime, right: &PrimeScheduleTime) -> Ordering {
    right
        .score
        .partial_cmp(&left.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left.source_hour.cmp(&right.source_hour))
}

fn compare_candidates(left: &PrimeCandidate<'_>, right: &PrimeCandidate<'_>) -> Ordering {
    right
        .weekly_remaining
        .partial_cmp(&left.weekly_remaining)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left.result.index.cmp(&right.result.index))
}

fn remaining_percent(used_percent: Option<f64>) -> f64 {
    100.0 - used_percent.unwrap_or(0.0).clamp(0.0, 100.0)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut text = value
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    text.push_str("...");
    text
}

fn format_time(hour: u8, minute: u8) -> String {
    format!("{hour:02}:{minute:02}")
}

fn print_plan(report: &PrimePlanReport) {
    println!(
        "prime plan: {}d history, lead {}m, {} schedule(s)",
        report.days,
        report.lead_minutes,
        report.schedules.len()
    );
    if report.source_databases.is_empty() {
        println!("source state dbs: none");
    } else {
        println!("source state dbs: {}", report.source_databases.len());
    }
    if let Some(cache) = report.rollout_cache.as_deref() {
        println!("rollout cache: {cache}");
    }
    println!();
    for schedule in &report.schedules {
        println!(
            "- {}  from heavy hour {:02}:00  score {:.0}  start {}  rollout {}",
            format_time(schedule.hour, schedule.minute),
            schedule.source_hour,
            schedule.score,
            schedule.start_tokens,
            schedule.rollout_tokens
        );
    }
}

fn print_run_report(report: &PrimeRunReport) {
    println!(
        "prime run: checked {} slot(s), primed {} slot(s){}",
        report.checked_slots,
        report.primed.len(),
        if report.dry_run { " (dry run)" } else { "" }
    );
    for attempt in &report.primed {
        let status = if attempt.success { "ok" } else { "failed" };
        println!(
            "- {}  {}  {}ms  {}",
            attempt.slot, status, attempt.elapsed_ms, attempt.note
        );
    }
    if report.primed.is_empty() {
        println!("no slots needed priming");
    }
    if !report.skipped.is_empty() {
        println!("skipped:");
        for skipped in report.skipped.iter().take(12) {
            println!("- {}  {}", skipped.slot, skipped.reason);
        }
        if report.skipped.len() > 12 {
            println!("- ... {} more", report.skipped.len() - 12);
        }
    }
}

fn print_status_report(report: &PrimeStatusReport) {
    println!("LaunchAgent: {}", report.launch_agent_path);
    println!(
        "installed: {}",
        if report.launch_agent_installed {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "loaded: {}",
        if report.launch_agent_loaded {
            "yes"
        } else {
            "no"
        }
    );
    if let Some(config) = report.config.as_ref() {
        println!();
        println!(
            "config: lead {}m, max slots {}, min weekly {:.1}%",
            config.lead_minutes,
            config
                .max_slots
                .map(|max_slots| max_slots.to_string())
                .unwrap_or_else(|| "all".to_string()),
            config.min_weekly_remaining
        );
        for schedule in &config.schedules {
            println!(
                "- {}  from heavy hour {:02}:00",
                format_time(schedule.hour, schedule.minute),
                schedule.source_hour
            );
        }
    }
    if let Some(state) = report
        .state
        .as_ref()
        .and_then(|state| state.last_run.as_ref())
    {
        println!();
        let dry_run = if state.dry_run { " (dry run)" } else { "" };
        println!(
            "last run{dry_run}: checked {}, primed {}",
            state.checked_slots,
            state.primed.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_time_wraps_around_midnight() {
        assert_eq!(prime_minutes_before(1, 210), 21 * 60 + 30);
        assert_eq!(prime_minutes_before(10, 210), 6 * 60 + 30);
    }

    #[test]
    fn schedule_uses_highest_scoring_hours() {
        let scores = vec![
            PrimeHourScore {
                hour: 10,
                score: 100.0,
                start_tokens: 100,
                ..PrimeHourScore::default()
            },
            PrimeHourScore {
                hour: 20,
                score: 80.0,
                start_tokens: 80,
                ..PrimeHourScore::default()
            },
            PrimeHourScore {
                hour: 4,
                score: 1.0,
                start_tokens: 1,
                ..PrimeHourScore::default()
            },
        ];

        let schedule = schedule_times_from_scores(&scores, 210, 2, 10);

        assert_eq!(schedule.len(), 2);
        assert_eq!(
            schedule
                .iter()
                .map(|item| (item.hour, item.minute, item.source_hour))
                .collect::<Vec<_>>(),
            vec![(6, 30, 10), (16, 30, 20)]
        );
    }

    #[test]
    fn state_hour_scores_read_wal_database_when_sidecars_are_missing() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("cx-prime-wal-test-{}-{unique}", std::process::id()));
        let db_path = root.join(STATE_DB);
        fs::create_dir_all(&root).expect("create temp dir");
        {
            let conn = Connection::open(&db_path).expect("open writable db");
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE threads (
                   id TEXT PRIMARY KEY,
                   created_at INTEGER NOT NULL,
                   tokens_used INTEGER NOT NULL
                 );
                 INSERT INTO threads (id, created_at, tokens_used)
                 VALUES ('thread-1', 3600, 1234);
                 PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .expect("seed db");
        }
        let _ = fs::remove_file(format!("{}-wal", db_path.display()));
        let _ = fs::remove_file(format!("{}-shm", db_path.display()));

        let mut seen_threads = HashSet::new();
        let mut scores = BTreeMap::new();
        add_state_db_hour_scores(&db_path, 0, &mut seen_threads, &mut scores)
            .expect("read state db");

        let total_tokens = scores.values().map(|score| score.start_tokens).sum::<u64>();
        assert_eq!(total_tokens, 1234);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn active_window_is_not_a_candidate_without_force() {
        let now = 1_000;
        let mut result = SlotResult::new("slot", 0, SlotStatus::Available, 90.0, "usage");
        result.five_hour_used_percent = Some(2.0);
        result.five_hour_refresh_at = Some(now + 3_600);
        result.weekly_used_percent = Some(10.0);

        let reason = prime_candidate(&result, now, 5.0, false).unwrap_err();

        assert_eq!(reason, "5h window already active (used 2.0%, refresh now)");
        assert!(prime_candidate(&result, now, 5.0, true).is_ok());
    }

    #[test]
    fn full_length_one_percent_window_is_still_primeable() {
        let now = 1_000;
        let mut result = SlotResult::new("slot", 0, SlotStatus::Available, 99.0, "usage");
        result.five_hour_used_percent = Some(1.0);
        result.five_hour_refresh_at = Some(now + FIVE_HOUR_WINDOW_SECONDS);
        result.weekly_used_percent = Some(0.0);

        assert!(prime_candidate(&result, now, 5.0, false).is_ok());
    }
}
