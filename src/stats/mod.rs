use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use rusqlite::Connection;
use serde::Deserialize;
use serde::Serialize;
use time::OffsetDateTime;

use crate::cli::StatsArgs;
use crate::cli::StatsRange;
use crate::paths::ManagerPaths;

mod calibration;
mod db;
mod pricing;
mod rollout;

pub use calibration::calibrate_mix;

use pricing::PriceBook;
use pricing::StatsPricePolicy;

const STATE_DB: &str = "state_5.sqlite";
const CALIBRATION_FILE: &str = "stats-calibration.json";
const CALIBRATION_SCHEMA_VERSION: u64 = 2;

pub const STATS_JSON_SCHEMA_VERSION: u64 = 2;

// Codex state only stores a thread-level total. This mix keeps estimates useful
// for Codex's cache-heavy workload without pretending to be exact billing.
const FALLBACK_TOKEN_MIX: TokenMix = TokenMix {
    uncached_input_share: 0.05,
    cached_input_share: 0.945,
    output_share: 0.005,
};

#[derive(Debug, Clone)]
pub struct StatsReport {
    pub json: bool,
    pub by_slot: bool,
    pub range: StatsRange,
    pub source_databases: Vec<String>,
    pub period_basis: String,
    pub price_source: Option<String>,
    pub price_note: Option<String>,
    pub token_mix: Option<TokenMix>,
    pub token_mix_source: Option<String>,
    pub periods: Vec<PeriodUsage>,
    pub daily: Vec<DailyUsage>,
}

impl StatsReport {
    pub fn includes_price_estimates(&self) -> bool {
        self.price_source.is_some()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CalibrationReport {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u64,
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

#[derive(Debug, Clone)]
pub struct PeriodUsage {
    pub period: String,
    pub since_unix: i64,
    pub threads: u64,
    pub tokens: u64,
    pub estimated_cost_usd: Option<f64>,
    pub priced_tokens: u64,
    pub unpriced_tokens: u64,
    pub slots: Vec<NamedUsage>,
    pub models: Vec<ModelUsage>,
}

#[derive(Debug, Clone)]
pub struct DailyUsage {
    pub date: String,
    pub threads: u64,
    pub tokens: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub uncategorized_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
    pub priced_tokens: u64,
    pub unpriced_tokens: u64,
    pub models: Vec<DailyModelUsage>,
}

#[derive(Debug, Clone)]
pub struct DailyModelUsage {
    pub provider: String,
    pub model: String,
    pub threads: u64,
    pub tokens: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub uncategorized_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
    pub priced_tokens: u64,
    pub unpriced_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct NamedUsage {
    pub name: String,
    pub threads: u64,
    pub tokens: u64,
    pub estimated_cost_usd: Option<f64>,
    pub priced_tokens: u64,
    pub unpriced_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct ModelUsage {
    pub provider: String,
    pub model: String,
    pub threads: u64,
    pub tokens: u64,
    pub estimated_cost_usd: Option<f64>,
    pub priced_tokens: u64,
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
    rollout_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
struct TokenTotals {
    samples: u64,
    total_tokens: u64,
    uncached_input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
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

#[derive(Debug, Clone, Default)]
struct DailyReportAccumulator {
    min_since: i64,
    days: BTreeMap<String, DailyAccumulator>,
}

#[derive(Debug, Clone, Default)]
struct DailyAccumulator {
    total: DailyTokenAccumulator,
    models: BTreeMap<ModelKey, DailyTokenAccumulator>,
}

#[derive(Debug, Clone, Default)]
struct DailyTokenAccumulator {
    threads: u64,
    totals: TokenTotals,
    priced_tokens: u64,
    unpriced_tokens: u64,
    estimated_cost_usd: Option<f64>,
}

pub fn collect_report(paths: &ManagerPaths, args: StatsArgs) -> Result<StatsReport> {
    let price_policy = StatsPricePolicy::from_args(&args);
    let slot_filters = args.slots.iter().cloned().collect::<BTreeSet<_>>();
    let db_paths = db::state_db_paths(paths, &slot_filters)?;
    if db_paths.is_empty() {
        anyhow::bail!("no Codex {STATE_DB} database found");
    }

    let periods = current_periods()?;
    let min_since = periods
        .iter()
        .map(|period| period.since_unix)
        .min()
        .unwrap_or(0);
    let (price_book, token_mix, token_mix_source) = match price_policy {
        StatsPricePolicy::Disabled => (None, None, None),
        StatsPricePolicy::Enabled {
            price_url,
            cache_policy,
        } => {
            let (mix, source) = calibration::load_token_mix(paths);
            let price_book = pricing::load_price_book(paths, price_url, cache_policy, mix, &source);
            (Some(price_book), Some(mix), Some(source))
        }
    };
    let mut accumulators = periods
        .into_iter()
        .map(PeriodAccumulator::new)
        .collect::<Vec<_>>();
    let mut daily = DailyReportAccumulator::new(min_since);
    let mut seen_threads = HashSet::new();

    for db_path in &db_paths {
        for usage in db::read_threads(db_path, paths, min_since)? {
            if !slot_filters.is_empty() && !slot_filters.contains(&usage.slot) {
                continue;
            }
            if !seen_threads.insert(usage.id.clone()) {
                continue;
            }
            add_thread_usage(&mut accumulators, &mut daily, price_book.as_ref(), &usage);
        }
    }

    let price_source = price_book.as_ref().map(|book| book.source.clone());
    let price_note = price_book.as_ref().map(|book| book.note.clone());

    Ok(StatsReport {
        json: args.json,
        by_slot: args.by_slot,
        range: args.range,
        source_databases: db_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        period_basis:
            "rollout token_count deltas by timestamp; fallback threads.tokens_used bucketed by threads.updated_at"
                .to_string(),
        price_source,
        price_note,
        token_mix,
        token_mix_source,
        periods: accumulators
            .into_iter()
            .map(PeriodAccumulator::into_usage)
            .collect(),
        daily: daily.into_usage(),
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

    fn add(&mut self, usage: &ThreadUsage, tokens: u64, cost: Option<f64>) {
        self.total.add(tokens, cost);
        self.slots
            .entry(usage.slot.clone())
            .or_default()
            .add(tokens, cost);
        self.models
            .entry(ModelKey {
                provider: usage.provider.clone(),
                model: usage.model.clone(),
            })
            .or_default()
            .add(tokens, cost);
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

fn add_thread_usage(
    accumulators: &mut [PeriodAccumulator],
    daily: &mut DailyReportAccumulator,
    price_book: Option<&PriceBook>,
    usage: &ThreadUsage,
) {
    if usage.rollout_path.exists() {
        if let Ok(events) = rollout::read_token_usage_events(&usage.rollout_path) {
            if !events.is_empty() {
                daily.add_events(usage, price_book, &events);
                for accumulator in accumulators.iter_mut() {
                    let mut totals = TokenTotals::default();
                    for event in &events {
                        if event.timestamp_unix >= accumulator.period.since_unix {
                            totals.add(event.totals.clone());
                        }
                    }
                    if totals.total_tokens > 0 {
                        let cost = price_book.and_then(|book| {
                            book.estimate_token_totals_cost(&usage.provider, &usage.model, &totals)
                        });
                        accumulator.add(usage, totals.total_tokens, cost);
                    }
                }
                return;
            }
        }
    }

    let cost = price_book.and_then(|book| estimate_thread_cost(book, usage));
    daily.add_fallback_thread(usage, cost);
    for accumulator in accumulators.iter_mut() {
        if usage.updated_at >= accumulator.period.since_unix {
            accumulator.add(usage, usage.tokens, cost);
        }
    }
}

fn estimate_thread_cost(price_book: &PriceBook, usage: &ThreadUsage) -> Option<f64> {
    if usage.rollout_path.exists() {
        if let Ok(Some(totals)) = rollout::read_final_token_usage(&usage.rollout_path) {
            if totals.total_tokens == usage.tokens {
                return price_book.estimate_token_totals_cost(
                    &usage.provider,
                    &usage.model,
                    &totals,
                );
            }
        }
    }
    price_book.estimate_cost(&usage.provider, &usage.model, usage.tokens)
}

impl TokenTotals {
    fn delta_from(&self, previous: &TokenTotals) -> TokenTotals {
        if self.total_tokens < previous.total_tokens {
            return TokenTotals {
                samples: 1,
                ..self.clone()
            };
        }
        TokenTotals {
            samples: 1,
            total_tokens: self.total_tokens - previous.total_tokens,
            uncached_input_tokens: self
                .uncached_input_tokens
                .saturating_sub(previous.uncached_input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(previous.cached_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(previous.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_sub(previous.reasoning_output_tokens),
        }
    }

    fn add(&mut self, usage: TokenTotals) {
        self.samples += usage.samples;
        self.total_tokens += usage.total_tokens;
        self.uncached_input_tokens += usage.uncached_input_tokens;
        self.cached_input_tokens += usage.cached_input_tokens;
        self.output_tokens += usage.output_tokens;
        self.reasoning_output_tokens += usage.reasoning_output_tokens;
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

impl DailyReportAccumulator {
    fn new(min_since: i64) -> Self {
        Self {
            min_since,
            days: BTreeMap::new(),
        }
    }

    fn add_events(
        &mut self,
        usage: &ThreadUsage,
        price_book: Option<&PriceBook>,
        events: &[rollout::TokenUsageEvent],
    ) {
        let mut by_day = BTreeMap::<String, TokenTotals>::new();
        for event in events {
            if event.timestamp_unix < self.min_since {
                continue;
            }
            if let Some(date) = utc_date_key(event.timestamp_unix) {
                by_day.entry(date).or_default().add(event.totals.clone());
            }
        }

        for (date, totals) in by_day {
            if totals.total_tokens == 0 {
                continue;
            }
            let cost = price_book.and_then(|book| {
                book.estimate_token_totals_cost(&usage.provider, &usage.model, &totals)
            });
            self.add_totals(&date, usage, &totals, cost);
        }
    }

    fn add_fallback_thread(&mut self, usage: &ThreadUsage, cost: Option<f64>) {
        if usage.tokens == 0 || usage.updated_at < self.min_since {
            return;
        }
        let totals = TokenTotals {
            samples: 1,
            total_tokens: usage.tokens,
            ..TokenTotals::default()
        };
        if let Some(date) = utc_date_key(usage.updated_at) {
            self.add_totals(&date, usage, &totals, cost);
        }
    }

    fn add_totals(
        &mut self,
        date: &str,
        usage: &ThreadUsage,
        totals: &TokenTotals,
        cost: Option<f64>,
    ) {
        let day = self.days.entry(date.to_string()).or_default();
        day.total.add(totals, cost);
        day.models
            .entry(ModelKey {
                provider: usage.provider.clone(),
                model: usage.model.clone(),
            })
            .or_default()
            .add(totals, cost);
    }

    fn into_usage(self) -> Vec<DailyUsage> {
        self.days
            .into_iter()
            .map(|(date, day)| day.into_usage(date))
            .collect()
    }
}

impl DailyAccumulator {
    fn into_usage(self, date: String) -> DailyUsage {
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

        let usage = self.total.into_parts();
        DailyUsage {
            date,
            threads: usage.threads,
            tokens: usage.tokens,
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: usage.reasoning_output_tokens,
            uncategorized_tokens: usage.uncategorized_tokens,
            estimated_cost_usd: usage.estimated_cost_usd,
            priced_tokens: usage.priced_tokens,
            unpriced_tokens: usage.unpriced_tokens,
            models,
        }
    }
}

struct DailyUsageParts {
    threads: u64,
    tokens: u64,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    uncategorized_tokens: u64,
    estimated_cost_usd: Option<f64>,
    priced_tokens: u64,
    unpriced_tokens: u64,
}

impl DailyTokenAccumulator {
    fn add(&mut self, totals: &TokenTotals, cost: Option<f64>) {
        self.threads += 1;
        self.totals.add(totals.clone());
        if let Some(cost) = cost {
            self.priced_tokens += totals.total_tokens;
            self.estimated_cost_usd = Some(self.estimated_cost_usd.unwrap_or(0.0) + cost);
        } else {
            self.unpriced_tokens += totals.total_tokens;
        }
    }

    fn into_parts(self) -> DailyUsageParts {
        let input_tokens = self.totals.uncached_input_tokens + self.totals.cached_input_tokens;
        let categorized_tokens = input_tokens + self.totals.output_tokens;
        DailyUsageParts {
            threads: self.threads,
            tokens: self.totals.total_tokens,
            input_tokens,
            cached_input_tokens: self.totals.cached_input_tokens,
            output_tokens: self.totals.output_tokens,
            reasoning_output_tokens: self.totals.reasoning_output_tokens,
            uncategorized_tokens: self.totals.total_tokens.saturating_sub(categorized_tokens),
            estimated_cost_usd: self.estimated_cost_usd,
            priced_tokens: self.priced_tokens,
            unpriced_tokens: self.unpriced_tokens,
        }
    }

    fn into_model(self, key: ModelKey) -> DailyModelUsage {
        let usage = self.into_parts();
        DailyModelUsage {
            provider: key.provider,
            model: key.model,
            threads: usage.threads,
            tokens: usage.tokens,
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: usage.reasoning_output_tokens,
            uncategorized_tokens: usage.uncategorized_tokens,
            estimated_cost_usd: usage.estimated_cost_usd,
            priced_tokens: usage.priced_tokens,
            unpriced_tokens: usage.unpriced_tokens,
        }
    }
}

fn utc_date_key(timestamp_unix: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp(timestamp_unix)
        .ok()
        .map(|timestamp| timestamp.date().to_string())
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
    use clap::Parser;

    use crate::cli::Cli;
    use crate::cli::Command;

    use super::calibration::parse_mix_calibration;
    use super::db::infer_slot_from_rollout_path;
    use super::pricing::parse_price_cache;
    use super::pricing::parse_pricing_page;
    use super::pricing::PriceCachePolicy;
    use super::pricing::StatsPricePolicy;
    use super::pricing::DEFAULT_PRICE_URL;
    use super::rollout::parse_token_count_line;
    use super::*;

    fn parse_stats_args(args: &[&str]) -> StatsArgs {
        let mut raw_args = vec!["cx", "stats"];
        raw_args.extend_from_slice(args);
        let cli = Cli::parse_from(raw_args);
        let Command::Stats(args) = cli.command else {
            panic!("expected stats command");
        };
        args
    }

    #[test]
    fn stats_price_policy_defaults_to_local_only() {
        let args = parse_stats_args(&[]);

        assert_eq!(
            StatsPricePolicy::from_args(&args),
            StatsPricePolicy::Enabled {
                price_url: DEFAULT_PRICE_URL,
                cache_policy: PriceCachePolicy::UseCacheOrFallback
            }
        );
    }

    #[test]
    fn stats_price_policy_json_defaults_to_token_only() {
        let args = parse_stats_args(&["--json"]);

        assert_eq!(
            StatsPricePolicy::from_args(&args),
            StatsPricePolicy::Disabled
        );
    }

    #[test]
    fn stats_price_policy_explicitly_enables_default_pricing() {
        let args = parse_stats_args(&["--price"]);

        assert_eq!(
            StatsPricePolicy::from_args(&args),
            StatsPricePolicy::Enabled {
                price_url: DEFAULT_PRICE_URL,
                cache_policy: PriceCachePolicy::UseFreshCacheIfAvailable
            }
        );
    }

    #[test]
    fn stats_price_policy_refresh_enables_and_refreshes() {
        let args = parse_stats_args(&["--refresh-prices"]);

        assert_eq!(
            StatsPricePolicy::from_args(&args),
            StatsPricePolicy::Enabled {
                price_url: DEFAULT_PRICE_URL,
                cache_policy: PriceCachePolicy::Refresh
            }
        );
    }

    #[test]
    fn stats_price_policy_uses_custom_price_url() {
        let args = parse_stats_args(&["--price-url", "https://example.test/pricing"]);

        assert_eq!(
            StatsPricePolicy::from_args(&args),
            StatsPricePolicy::Enabled {
                price_url: "https://example.test/pricing",
                cache_policy: PriceCachePolicy::UseFreshCacheIfAvailable
            }
        );
    }

    #[test]
    fn stats_price_policy_no_price_overrides_price_flags() {
        let args = parse_stats_args(&["--price", "--refresh-prices", "--no-price"]);

        assert_eq!(
            StatsPricePolicy::from_args(&args),
            StatsPricePolicy::Disabled
        );
    }

    #[test]
    fn owned_file_schema_versions_are_required() {
        let price_cache = r#"{
          "fetchedAt": 123,
          "sourceUrl": "https://example.test/pricing",
          "prices": {}
        }"#;
        let calibration = r#"{
          "calibratedAt": 123,
          "samples": 1,
          "sourceRollouts": 1,
          "totalTokens": 1,
          "tokenMix": {
            "uncachedInputShare": 1.0,
            "cachedInputShare": 0.0,
            "outputShare": 0.0
          }
        }"#;

        assert!(parse_price_cache(price_cache).is_none());
        assert!(parse_mix_calibration(calibration).is_none());
    }

    #[test]
    fn unsupported_owned_file_versions_are_rejected() {
        let price_cache = r#"{
          "schemaVersion": 99,
          "fetchedAt": 123,
          "sourceUrl": "https://example.test/pricing",
          "prices": {}
        }"#;
        let calibration = r#"{
          "schemaVersion": 99,
          "calibratedAt": 123,
          "samples": 1,
          "sourceRollouts": 1,
          "totalTokens": 1,
          "tokenMix": {
            "uncachedInputShare": 1.0,
            "cachedInputShare": 0.0,
            "outputShare": 0.0
          }
        }"#;

        assert!(parse_price_cache(price_cache).is_none());
        assert!(parse_mix_calibration(calibration).is_none());
    }

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
            targets_dir: PathBuf::from("/Users/me/.codex/profile-manager/targets"),
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
            reasoning_output_tokens: 0,
        });

        let mix = totals.token_mix();

        assert!((mix.uncached_input_share - 0.095238).abs() < 0.00001);
        assert!((mix.cached_input_share - 0.857143).abs() < 0.00001);
        assert!((mix.output_share - 0.047619).abs() < 0.00001);
    }
}
