use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use reqwest::blocking::Client;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use serde::Deserialize;
use serde::Serialize;

use crate::cli::StatsArgs;
use crate::paths::ManagerPaths;

const STATE_DB: &str = "state_5.sqlite";
const DEFAULT_PRICE_URL: &str = "https://developers.openai.com/api/docs/pricing";
const PRICE_CACHE_FILE: &str = "price-cache.json";
const CALIBRATION_FILE: &str = "stats-calibration.json";
const PRICE_CACHE_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;

// Codex state only stores a thread-level total. This mix keeps estimates useful
// for Codex's cache-heavy workload without pretending to be exact billing.
const FALLBACK_TOKEN_MIX: TokenMix = TokenMix {
    uncached_input_share: 0.05,
    cached_input_share: 0.945,
    output_share: 0.005,
};

#[derive(Debug, Clone, Serialize)]
pub struct StatsReport {
    #[serde(skip)]
    pub json: bool,
    #[serde(rename = "bySlot")]
    pub by_slot: bool,
    #[serde(rename = "sourceDatabases")]
    pub source_databases: Vec<String>,
    #[serde(rename = "periodBasis")]
    pub period_basis: String,
    #[serde(rename = "priceSource")]
    pub price_source: Option<String>,
    #[serde(rename = "priceNote")]
    pub price_note: Option<String>,
    #[serde(rename = "tokenMix")]
    pub token_mix: Option<TokenMix>,
    #[serde(rename = "tokenMixSource")]
    pub token_mix_source: Option<String>,
    pub periods: Vec<PeriodUsage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalibrationReport {
    #[serde(skip)]
    pub json: bool,
    #[serde(rename = "savedTo")]
    pub saved_to: String,
    #[serde(rename = "sourceDatabases")]
    pub source_databases: Vec<String>,
    #[serde(rename = "sourceRollouts")]
    pub source_rollouts: u64,
    pub samples: u64,
    #[serde(rename = "totalTokens")]
    pub total_tokens: u64,
    #[serde(rename = "uncachedInputTokens")]
    pub uncached_input_tokens: u64,
    #[serde(rename = "cachedInputTokens")]
    pub cached_input_tokens: u64,
    #[serde(rename = "outputTokens")]
    pub output_tokens: u64,
    #[serde(rename = "tokenMix")]
    pub token_mix: TokenMix,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TokenMix {
    #[serde(rename = "uncachedInputShare")]
    pub uncached_input_share: f64,
    #[serde(rename = "cachedInputShare")]
    pub cached_input_share: f64,
    #[serde(rename = "outputShare")]
    pub output_share: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PeriodUsage {
    pub period: String,
    #[serde(rename = "sinceUnix")]
    pub since_unix: i64,
    pub threads: u64,
    pub tokens: u64,
    #[serde(rename = "estimatedCostUsd")]
    pub estimated_cost_usd: Option<f64>,
    #[serde(rename = "pricedTokens")]
    pub priced_tokens: u64,
    #[serde(rename = "unpricedTokens")]
    pub unpriced_tokens: u64,
    pub slots: Vec<NamedUsage>,
    pub models: Vec<ModelUsage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NamedUsage {
    pub name: String,
    pub threads: u64,
    pub tokens: u64,
    #[serde(rename = "estimatedCostUsd")]
    pub estimated_cost_usd: Option<f64>,
    #[serde(rename = "pricedTokens")]
    pub priced_tokens: u64,
    #[serde(rename = "unpricedTokens")]
    pub unpriced_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelUsage {
    pub provider: String,
    pub model: String,
    pub threads: u64,
    pub tokens: u64,
    #[serde(rename = "estimatedCostUsd")]
    pub estimated_cost_usd: Option<f64>,
    #[serde(rename = "pricedTokens")]
    pub priced_tokens: u64,
    #[serde(rename = "unpricedTokens")]
    pub unpriced_tokens: u64,
}

#[derive(Debug, Clone)]
struct Period {
    label: &'static str,
    since_unix: i64,
}

#[derive(Debug, Clone)]
struct ThreadUsage {
    id: String,
    updated_at: i64,
    tokens: u64,
    provider: String,
    model: String,
    slot: String,
}

#[derive(Debug, Clone, Default)]
struct TokenTotals {
    samples: u64,
    total_tokens: u64,
    uncached_input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
}

#[derive(Debug, Clone, Default)]
struct UsageAccumulator {
    threads: u64,
    tokens: u64,
    priced_tokens: u64,
    unpriced_tokens: u64,
    estimated_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ModelKey {
    provider: String,
    model: String,
}

#[derive(Debug, Clone)]
struct PeriodAccumulator {
    period: Period,
    total: UsageAccumulator,
    slots: BTreeMap<String, UsageAccumulator>,
    models: BTreeMap<ModelKey, UsageAccumulator>,
}

#[derive(Debug, Clone)]
struct PriceBook {
    prices: BTreeMap<String, ModelPrice>,
    token_mix: TokenMix,
    source: String,
    note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PriceCache {
    #[serde(rename = "fetchedAt")]
    fetched_at: i64,
    #[serde(rename = "sourceUrl")]
    source_url: String,
    prices: BTreeMap<String, ModelPrice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MixCalibration {
    #[serde(rename = "calibratedAt")]
    calibrated_at: i64,
    samples: u64,
    #[serde(rename = "sourceRollouts")]
    source_rollouts: u64,
    #[serde(rename = "totalTokens")]
    total_tokens: u64,
    #[serde(rename = "tokenMix")]
    token_mix: TokenMix,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct ModelPrice {
    #[serde(rename = "inputPerMillion")]
    input_per_million: f64,
    #[serde(rename = "cachedInputPerMillion")]
    cached_input_per_million: Option<f64>,
    #[serde(rename = "outputPerMillion")]
    output_per_million: f64,
}

pub fn collect_report(paths: &ManagerPaths, args: StatsArgs) -> Result<StatsReport> {
    let slot_filters = args.slots.iter().cloned().collect::<BTreeSet<_>>();
    let db_paths = state_db_paths(paths, &slot_filters)?;
    if db_paths.is_empty() {
        anyhow::bail!("no Codex {STATE_DB} database found");
    }

    let periods = current_periods()?;
    let min_since = periods
        .iter()
        .map(|period| period.since_unix)
        .min()
        .unwrap_or(0);
    let (token_mix, token_mix_source) = if args.no_price {
        (None, None)
    } else {
        let (mix, source) = load_token_mix(paths);
        (Some(mix), Some(source))
    };
    let price_book = if args.no_price {
        None
    } else {
        let mix = token_mix.expect("token mix exists when price estimates are enabled");
        Some(load_price_book(
            paths,
            args.price_url.as_deref().unwrap_or(DEFAULT_PRICE_URL),
            args.refresh_prices,
            mix,
            token_mix_source.as_deref().unwrap_or("unknown"),
        ))
    };
    let mut accumulators = periods
        .into_iter()
        .map(PeriodAccumulator::new)
        .collect::<Vec<_>>();
    let mut seen_threads = HashSet::new();

    for db_path in &db_paths {
        for usage in read_threads(db_path, paths, min_since)? {
            if !slot_filters.is_empty() && !slot_filters.contains(&usage.slot) {
                continue;
            }
            if !seen_threads.insert(usage.id.clone()) {
                continue;
            }
            for accumulator in &mut accumulators {
                if usage.updated_at >= accumulator.period.since_unix {
                    accumulator.add(&usage, price_book.as_ref());
                }
            }
        }
    }

    let price_source = price_book.as_ref().map(|book| book.source.clone());
    let price_note = price_book
        .as_ref()
        .map(|book| book.note.clone())
        .or_else(|| Some("price estimates disabled".to_string()));

    Ok(StatsReport {
        json: args.json,
        by_slot: args.by_slot,
        source_databases: db_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        period_basis: "threads.tokens_used bucketed by threads.updated_at".to_string(),
        price_source,
        price_note,
        token_mix,
        token_mix_source,
        periods: accumulators
            .into_iter()
            .map(PeriodAccumulator::into_usage)
            .collect(),
    })
}

pub fn calibrate_mix(paths: &ManagerPaths, args: StatsArgs) -> Result<CalibrationReport> {
    let slot_filters = args.slots.iter().cloned().collect::<BTreeSet<_>>();
    let db_paths = state_db_paths(paths, &slot_filters)?;
    if db_paths.is_empty() {
        anyhow::bail!("no Codex {STATE_DB} database found");
    }

    let mut rollout_paths = BTreeMap::new();
    for db_path in &db_paths {
        for rollout_path in read_rollout_paths(db_path, paths, &slot_filters)? {
            if !rollout_path.exists() {
                continue;
            }
            let canonical = fs::canonicalize(&rollout_path)
                .with_context(|| format!("resolve {}", rollout_path.display()))?;
            rollout_paths.entry(canonical).or_insert(rollout_path);
        }
    }

    let mut totals = TokenTotals::default();
    for rollout_path in rollout_paths.values() {
        if let Some(usage) = read_final_token_usage(rollout_path)? {
            totals.add(usage);
        }
    }
    if totals.samples == 0 {
        anyhow::bail!("no rollout token_count samples found");
    }

    let token_mix = totals.token_mix();
    let calibration = MixCalibration {
        calibrated_at: unix_now(),
        samples: totals.samples,
        source_rollouts: rollout_paths.len() as u64,
        total_tokens: totals.total_tokens,
        token_mix,
    };
    let saved_to = write_mix_calibration(paths, &calibration)?;

    Ok(CalibrationReport {
        json: args.json,
        saved_to: saved_to.display().to_string(),
        source_databases: db_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        source_rollouts: rollout_paths.len() as u64,
        samples: totals.samples,
        total_tokens: totals.total_tokens,
        uncached_input_tokens: totals.uncached_input_tokens,
        cached_input_tokens: totals.cached_input_tokens,
        output_tokens: totals.output_tokens,
        token_mix,
    })
}

impl PeriodAccumulator {
    fn new(period: Period) -> Self {
        Self {
            period,
            total: UsageAccumulator::default(),
            slots: BTreeMap::new(),
            models: BTreeMap::new(),
        }
    }

    fn add(&mut self, usage: &ThreadUsage, price_book: Option<&PriceBook>) {
        let cost = price_book
            .and_then(|book| book.estimate_cost(&usage.provider, &usage.model, usage.tokens));
        self.total.add(usage.tokens, cost);
        self.slots
            .entry(usage.slot.clone())
            .or_default()
            .add(usage.tokens, cost);
        self.models
            .entry(ModelKey {
                provider: usage.provider.clone(),
                model: usage.model.clone(),
            })
            .or_default()
            .add(usage.tokens, cost);
    }

    fn into_usage(self) -> PeriodUsage {
        let mut slots = self
            .slots
            .into_iter()
            .map(|(name, usage)| usage.into_named(name))
            .collect::<Vec<_>>();
        slots.sort_by(|left, right| {
            right
                .tokens
                .cmp(&left.tokens)
                .then_with(|| left.name.cmp(&right.name))
        });

        let mut models = self
            .models
            .into_iter()
            .map(|(key, usage)| usage.into_model(key))
            .collect::<Vec<_>>();
        models.sort_by(|left, right| {
            right
                .tokens
                .cmp(&left.tokens)
                .then_with(|| left.provider.cmp(&right.provider))
                .then_with(|| left.model.cmp(&right.model))
        });

        PeriodUsage {
            period: self.period.label.to_string(),
            since_unix: self.period.since_unix,
            threads: self.total.threads,
            tokens: self.total.tokens,
            estimated_cost_usd: self.total.estimated_cost_usd,
            priced_tokens: self.total.priced_tokens,
            unpriced_tokens: self.total.unpriced_tokens,
            slots,
            models,
        }
    }
}

impl TokenTotals {
    fn add(&mut self, usage: TokenTotals) {
        self.samples += usage.samples;
        self.total_tokens += usage.total_tokens;
        self.uncached_input_tokens += usage.uncached_input_tokens;
        self.cached_input_tokens += usage.cached_input_tokens;
        self.output_tokens += usage.output_tokens;
    }

    fn token_mix(&self) -> TokenMix {
        let denominator =
            self.uncached_input_tokens + self.cached_input_tokens + self.output_tokens;
        if denominator == 0 {
            return FALLBACK_TOKEN_MIX;
        }
        TokenMix {
            uncached_input_share: self.uncached_input_tokens as f64 / denominator as f64,
            cached_input_share: self.cached_input_tokens as f64 / denominator as f64,
            output_share: self.output_tokens as f64 / denominator as f64,
        }
    }
}

impl TokenMix {
    fn valid(self) -> bool {
        let sum = self.uncached_input_share + self.cached_input_share + self.output_share;
        self.uncached_input_share.is_finite()
            && self.cached_input_share.is_finite()
            && self.output_share.is_finite()
            && self.uncached_input_share >= 0.0
            && self.cached_input_share >= 0.0
            && self.output_share >= 0.0
            && (0.99..=1.01).contains(&sum)
    }
}

impl UsageAccumulator {
    fn add(&mut self, tokens: u64, cost: Option<f64>) {
        self.threads += 1;
        self.tokens += tokens;
        if let Some(cost) = cost {
            self.priced_tokens += tokens;
            self.estimated_cost_usd = Some(self.estimated_cost_usd.unwrap_or(0.0) + cost);
        } else {
            self.unpriced_tokens += tokens;
        }
    }

    fn into_named(self, name: String) -> NamedUsage {
        NamedUsage {
            name,
            threads: self.threads,
            tokens: self.tokens,
            estimated_cost_usd: self.estimated_cost_usd,
            priced_tokens: self.priced_tokens,
            unpriced_tokens: self.unpriced_tokens,
        }
    }

    fn into_model(self, key: ModelKey) -> ModelUsage {
        ModelUsage {
            provider: key.provider,
            model: key.model,
            threads: self.threads,
            tokens: self.tokens,
            estimated_cost_usd: self.estimated_cost_usd,
            priced_tokens: self.priced_tokens,
            unpriced_tokens: self.unpriced_tokens,
        }
    }
}

impl PriceBook {
    fn estimate_cost(&self, provider: &str, model: &str, tokens: u64) -> Option<f64> {
        if provider != "openai" {
            return None;
        }
        let price = self.prices.get(&normalize_model_key(model))?;
        let cached_rate = price
            .cached_input_per_million
            .unwrap_or(price.input_per_million);
        let rate = (self.token_mix.uncached_input_share * price.input_per_million)
            + (self.token_mix.cached_input_share * cached_rate)
            + (self.token_mix.output_share * price.output_per_million);
        Some(tokens as f64 * rate / 1_000_000.0)
    }
}

fn state_db_paths(paths: &ManagerPaths, slot_filters: &BTreeSet<String>) -> Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    candidates.push(paths.base_codex_home.join(STATE_DB));

    if slot_filters.is_empty() {
        if paths.slots_dir.is_dir() {
            for entry in fs::read_dir(&paths.slots_dir)
                .with_context(|| format!("read {}", paths.slots_dir.display()))?
            {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    candidates.push(entry.path().join("home").join(STATE_DB));
                }
            }
        }
    } else {
        for slot in slot_filters {
            candidates.push(paths.slot_home(slot).join(STATE_DB));
        }
    }

    let mut seen = BTreeSet::new();
    let mut db_paths = Vec::new();
    for candidate in candidates {
        if !candidate.exists() {
            continue;
        }
        let canonical = fs::canonicalize(&candidate)
            .with_context(|| format!("resolve {}", candidate.display()))?;
        if seen.insert(canonical.clone()) {
            db_paths.push(canonical);
        }
    }
    db_paths.sort();
    Ok(db_paths)
}

fn read_threads(db_path: &Path, paths: &ManagerPaths, min_since: i64) -> Result<Vec<ThreadUsage>> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", db_path.display()))?;
    let columns = thread_columns(&conn)?;
    if !columns.contains("updated_at") || !columns.contains("tokens_used") {
        return Ok(Vec::new());
    }

    let model_provider_expr = optional_column(&columns, "model_provider");
    let model_expr = optional_column(&columns, "model");
    let rollout_path_expr = optional_column(&columns, "rollout_path");
    let sql = format!(
        "SELECT id, updated_at, tokens_used, {model_provider_expr}, {model_expr}, {rollout_path_expr} \
         FROM threads \
         WHERE tokens_used > 0 AND updated_at >= ?1"
    );
    let mut statement = conn
        .prepare(&sql)
        .with_context(|| format!("prepare stats query for {}", db_path.display()))?;
    let rows = statement.query_map(params![min_since], |row| {
        let rollout_path: String = row.get(5)?;
        Ok(ThreadUsage {
            id: row.get(0)?,
            updated_at: row.get(1)?,
            tokens: row.get::<_, i64>(2)?.max(0) as u64,
            provider: empty_as_unknown(row.get::<_, String>(3)?),
            model: empty_as_unknown(row.get::<_, String>(4)?),
            slot: infer_slot_from_rollout_path(&rollout_path, paths),
        })
    })?;

    let mut usages = Vec::new();
    for row in rows {
        usages.push(row?);
    }
    Ok(usages)
}

fn read_rollout_paths(
    db_path: &Path,
    paths: &ManagerPaths,
    slot_filters: &BTreeSet<String>,
) -> Result<Vec<PathBuf>> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", db_path.display()))?;
    let columns = thread_columns(&conn)?;
    if !columns.contains("rollout_path") {
        return Ok(Vec::new());
    }

    let mut statement = conn
        .prepare("SELECT rollout_path FROM threads WHERE rollout_path <> ''")
        .with_context(|| format!("prepare calibration query for {}", db_path.display()))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut paths_out = Vec::new();
    for row in rows {
        let rollout_path = row?;
        if !slot_filters.is_empty()
            && !slot_filters.contains(&infer_slot_from_rollout_path(&rollout_path, paths))
        {
            continue;
        }
        paths_out.push(PathBuf::from(rollout_path));
    }
    Ok(paths_out)
}

fn read_final_token_usage(path: &Path) -> Result<Option<TokenTotals>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut latest = None;
    for line in reader.lines() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        if !line.contains("\"token_count\"") {
            continue;
        }
        if let Some(usage) = parse_token_count_line(&line) {
            latest = Some(usage);
        }
    }
    Ok(latest)
}

fn parse_token_count_line(line: &str) -> Option<TokenTotals> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let usage = value
        .get("payload")?
        .get("info")?
        .get("total_token_usage")?;
    let input_tokens = usage.get("input_tokens")?.as_u64()?;
    let cached_input_tokens = usage
        .get("cached_input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
        .min(input_tokens);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(input_tokens + output_tokens);

    Some(TokenTotals {
        samples: 1,
        total_tokens,
        uncached_input_tokens: input_tokens - cached_input_tokens,
        cached_input_tokens,
        output_tokens,
    })
}

fn thread_columns(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut statement = conn.prepare("PRAGMA table_info(threads)")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    let mut columns = BTreeSet::new();
    for row in rows {
        columns.insert(row?);
    }
    Ok(columns)
}

fn optional_column(columns: &BTreeSet<String>, name: &str) -> String {
    if columns.contains(name) {
        name.to_string()
    } else {
        format!("'' AS {name}")
    }
}

fn empty_as_unknown(value: String) -> String {
    if value.trim().is_empty() {
        "unknown".to_string()
    } else {
        value
    }
}

fn infer_slot_from_rollout_path(rollout_path: &str, paths: &ManagerPaths) -> String {
    let normalized = rollout_path.replace('\\', "/");
    let manager_dir = paths.manager_dir.display().to_string().replace('\\', "/");
    let marker = format!("{}/slots/", manager_dir.trim_end_matches('/'));
    if let Some(index) = normalized.find(&marker) {
        let rest = &normalized[index + marker.len()..];
        if let Some(slot) = rest.split('/').next().filter(|slot| !slot.is_empty()) {
            return slot.to_string();
        }
    }
    "base".to_string()
}

fn current_periods() -> Result<Vec<Period>> {
    let now = unix_now();
    let conn = Connection::open_in_memory()?;
    let (today, week, month, year): (i64, i64, i64, i64) = conn.query_row(
        "SELECT \
         unixepoch(datetime('now','localtime','start of day'),'utc'), \
         unixepoch(datetime('now','localtime','start of day', printf('-%d days', (CAST(strftime('%w','now','localtime') AS INTEGER)+6)%7)),'utc'), \
         unixepoch(datetime('now','localtime','start of month'),'utc'), \
         unixepoch(datetime('now','localtime','start of year'),'utc')",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;

    Ok(vec![
        Period {
            label: "1h",
            since_unix: now.saturating_sub(60 * 60),
        },
        Period {
            label: "24h",
            since_unix: now.saturating_sub(24 * 60 * 60),
        },
        Period {
            label: "today",
            since_unix: today,
        },
        Period {
            label: "week",
            since_unix: week,
        },
        Period {
            label: "month",
            since_unix: month,
        },
        Period {
            label: "year",
            since_unix: year,
        },
    ])
}

fn load_price_book(
    paths: &ManagerPaths,
    price_url: &str,
    refresh: bool,
    token_mix: TokenMix,
    token_mix_source: &str,
) -> PriceBook {
    if !refresh {
        if let Some(cache) = read_price_cache(paths) {
            if cache.source_url == price_url
                && unix_now().saturating_sub(cache.fetched_at) < PRICE_CACHE_TTL_SECONDS
                && !cache.prices.is_empty()
            {
                return PriceBook {
                    prices: cache.prices,
                    token_mix,
                    source: format!("cache: {price_url}"),
                    note: price_estimate_note(token_mix, token_mix_source),
                };
            }
        }
    }

    match fetch_prices(price_url) {
        Ok(prices) if !prices.is_empty() => {
            let cache = PriceCache {
                fetched_at: unix_now(),
                source_url: price_url.to_string(),
                prices: prices.clone(),
            };
            let _ = write_price_cache(paths, &cache);
            PriceBook {
                prices,
                token_mix,
                source: price_url.to_string(),
                note: price_estimate_note(token_mix, token_mix_source),
            }
        }
        Ok(_) => fallback_price_book(
            token_mix,
            token_mix_source,
            "pricing page had no parseable model rows",
        ),
        Err(err) => fallback_price_book(token_mix, token_mix_source, &format!("{err:#}")),
    }
}

fn fetch_prices(price_url: &str) -> Result<BTreeMap<String, ModelPrice>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(3))
        .user_agent("cx")
        .build()?;
    let body = client
        .get(price_url)
        .send()
        .with_context(|| format!("fetch {price_url}"))?
        .error_for_status()
        .with_context(|| format!("fetch {price_url}"))?
        .text()
        .with_context(|| format!("read {price_url}"))?;
    Ok(parse_pricing_page(&body))
}

fn read_price_cache(paths: &ManagerPaths) -> Option<PriceCache> {
    let path = paths.manager_dir.join(PRICE_CACHE_FILE);
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_price_cache(paths: &ManagerPaths, cache: &PriceCache) -> Result<()> {
    fs::create_dir_all(&paths.manager_dir)
        .with_context(|| format!("create {}", paths.manager_dir.display()))?;
    let path = paths.manager_dir.join(PRICE_CACHE_FILE);
    let content = serde_json::to_string_pretty(cache)?;
    fs::write(&path, content).with_context(|| format!("write {}", path.display()))
}

fn load_token_mix(paths: &ManagerPaths) -> (TokenMix, String) {
    let path = paths.manager_dir.join(CALIBRATION_FILE);
    let Some(calibration) = read_mix_calibration(&path) else {
        return (FALLBACK_TOKEN_MIX, "built-in fallback".to_string());
    };
    if calibration.samples == 0 || !calibration.token_mix.valid() {
        return (FALLBACK_TOKEN_MIX, "built-in fallback".to_string());
    }
    (
        calibration.token_mix,
        format!(
            "calibration: {} ({} samples)",
            path.display(),
            calibration.samples
        ),
    )
}

fn read_mix_calibration(path: &Path) -> Option<MixCalibration> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_mix_calibration(paths: &ManagerPaths, calibration: &MixCalibration) -> Result<PathBuf> {
    fs::create_dir_all(&paths.manager_dir)
        .with_context(|| format!("create {}", paths.manager_dir.display()))?;
    let path = paths.manager_dir.join(CALIBRATION_FILE);
    let content = serde_json::to_string_pretty(calibration)?;
    fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn fallback_price_book(token_mix: TokenMix, token_mix_source: &str, reason: &str) -> PriceBook {
    PriceBook {
        prices: fallback_prices(),
        token_mix,
        source: "built-in fallback".to_string(),
        note: format!(
            "{}; pricing fetch fallback reason: {reason}",
            price_estimate_note(token_mix, token_mix_source)
        ),
    }
}

fn price_estimate_note(token_mix: TokenMix, token_mix_source: &str) -> String {
    format!(
        "estimate uses {:.2}% uncached input, {:.2}% cached input, {:.2}% output from {token_mix_source}; Codex state stores total tokens only",
        token_mix.uncached_input_share * 100.0,
        token_mix.cached_input_share * 100.0,
        token_mix.output_share * 100.0
    )
}

fn parse_pricing_page(body: &str) -> BTreeMap<String, ModelPrice> {
    let decoded = decode_html(body);
    let mut prices = BTreeMap::new();
    let marker = "[[0,\"";
    let mut offset = 0;
    while let Some(relative) = decoded[offset..].find(marker) {
        let start = offset + relative + marker.len();
        let Some(end_relative) = decoded[start..].find('"') else {
            break;
        };
        let end = start + end_relative;
        let raw_model = &decoded[start..end];
        let after_end = (end + 240).min(decoded.len());
        if let Some(model) = normalize_price_model_name(raw_model) {
            if let Some(price) = parse_price_values(&decoded[end..after_end]) {
                prices.entry(model).or_insert(price);
            }
        }
        offset = end + 1;
    }
    prices
}

fn parse_price_values(text: &str) -> Option<ModelPrice> {
    let mut values = Vec::new();
    let mut offset = 0;
    while values.len() < 3 {
        let Some(relative) = text[offset..].find("[0,") else {
            break;
        };
        let start = offset + relative + 3;
        let Some(end_relative) = text[start..].find(']') else {
            break;
        };
        let end = start + end_relative;
        let raw = text[start..end].trim().trim_matches('"');
        values.push(raw.parse::<f64>().ok());
        offset = end + 1;
    }

    if values.len() < 3 {
        return None;
    }
    Some(ModelPrice {
        input_per_million: values[0]?,
        cached_input_per_million: values[1],
        output_per_million: values[2]?,
    })
}

fn normalize_price_model_name(raw: &str) -> Option<String> {
    let model = normalize_model_key(raw.split(" (").next().unwrap_or(raw).trim());
    if model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
        || model.starts_with("computer-use")
    {
        Some(model)
    } else {
        None
    }
}

fn normalize_model_key(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

fn decode_html(input: &str) -> String {
    input
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn fallback_prices() -> BTreeMap<String, ModelPrice> {
    [
        ("gpt-5.5", 5.0, Some(0.5), 30.0),
        ("gpt-5.4", 2.5, Some(0.25), 15.0),
        ("gpt-5.4-mini", 0.75, Some(0.075), 4.5),
        ("gpt-5.4-nano", 0.2, Some(0.02), 1.25),
        ("gpt-5.2", 1.75, Some(0.175), 14.0),
        ("gpt-5.1", 1.25, Some(0.125), 10.0),
        ("gpt-5", 1.25, Some(0.125), 10.0),
        ("gpt-5-mini", 0.25, Some(0.025), 2.0),
        ("gpt-5.3-codex", 1.75, Some(0.175), 14.0),
        ("gpt-5.2-codex", 3.5, Some(0.35), 28.0),
        ("gpt-5.1-codex-max", 2.5, Some(0.25), 20.0),
        ("gpt-5.1-codex", 2.5, Some(0.25), 20.0),
        ("gpt-5-codex", 2.5, Some(0.25), 20.0),
    ]
    .into_iter()
    .map(
        |(model, input_per_million, cached_input_per_million, output_per_million)| {
            (
                model.to_string(),
                ModelPrice {
                    input_per_million,
                    cached_input_per_million,
                    output_per_million,
                },
            )
        },
    )
    .collect()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn human_tokens(tokens: u64) -> String {
    let value = tokens as f64;
    if tokens >= 1_000_000_000 {
        format!("{:.2}B", value / 1_000_000_000.0)
    } else if tokens >= 10_000_000 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if tokens >= 1_000_000 {
        format!("{:.2}M", value / 1_000_000.0)
    } else if tokens >= 10_000 {
        format!("{:.1}K", value / 1_000.0)
    } else if tokens >= 1_000 {
        format!("{:.2}K", value / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pricing_props_rows() {
        let html = r#"
            props="{&quot;rows&quot;:[1,[[1,[[0,&quot;gpt-5.5 (&lt;272K context length)&quot;],[0,5],[0,0.5],[0,30]]],[1,[[0,&quot;gpt-5.4-mini&quot;],[0,0.75],[0,0.075],[0,4.5]]]]]}"
        "#;

        let prices = parse_pricing_page(html);

        assert_eq!(prices["gpt-5.5"].input_per_million, 5.0);
        assert_eq!(prices["gpt-5.5"].cached_input_per_million, Some(0.5));
        assert_eq!(prices["gpt-5.5"].output_per_million, 30.0);
        assert_eq!(prices["gpt-5.4-mini"].input_per_million, 0.75);
    }

    #[test]
    fn normalizes_slot_from_rollout_path() {
        let paths = ManagerPaths {
            base_codex_home: PathBuf::from("/Users/me/.codex"),
            manager_dir: PathBuf::from("/Users/me/.codex/profile-manager"),
            slots_dir: PathBuf::from("/Users/me/.codex/profile-manager/slots"),
            rotation_file: PathBuf::from("/Users/me/.codex/profile-manager/rotation.txt"),
        };

        assert_eq!(
            infer_slot_from_rollout_path(
                "/Users/me/.codex/profile-manager/slots/bus3/home/sessions/2026/04/29/rollout.jsonl",
                &paths
            ),
            "bus3"
        );
        assert_eq!(
            infer_slot_from_rollout_path("/Users/me/.codex/sessions/rollout.jsonl", &paths),
            "base"
        );
    }

    #[test]
    fn humanizes_token_counts() {
        assert_eq!(human_tokens(999), "999");
        assert_eq!(human_tokens(1_500), "1.50K");
        assert_eq!(human_tokens(12_500), "12.5K");
        assert_eq!(human_tokens(1_250_000), "1.25M");
        assert_eq!(human_tokens(12_500_000), "12.5M");
    }

    #[test]
    fn parses_token_count_line_for_calibration() {
        let line = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":900,"output_tokens":50,"total_tokens":1050}}}}"#;

        let usage = parse_token_count_line(line).expect("token_count line");

        assert_eq!(usage.total_tokens, 1050);
        assert_eq!(usage.uncached_input_tokens, 100);
        assert_eq!(usage.cached_input_tokens, 900);
        assert_eq!(usage.output_tokens, 50);
    }

    #[test]
    fn calibrated_mix_uses_token_categories() {
        let mut totals = TokenTotals::default();
        totals.add(TokenTotals {
            samples: 1,
            total_tokens: 1050,
            uncached_input_tokens: 100,
            cached_input_tokens: 900,
            output_tokens: 50,
        });

        let mix = totals.token_mix();

        assert!((mix.uncached_input_share - 0.095238).abs() < 0.00001);
        assert!((mix.cached_input_share - 0.857143).abs() < 0.00001);
        assert!((mix.output_share - 0.047619).abs() < 0.00001);
    }
}
