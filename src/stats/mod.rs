use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
#[cfg(test)]
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use rusqlite::Connection;
use serde::Deserialize;
use serde::Serialize;

use crate::cli::StatsArgs;
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
const LEGACY_FILE_SCHEMA_VERSION: u64 = 1;

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
    pub source_databases: Vec<String>,
    pub period_basis: String,
    pub price_source: Option<String>,
    pub price_note: Option<String>,
    pub token_mix: Option<TokenMix>,
    pub token_mix_source: Option<String>,
    pub periods: Vec<PeriodUsage>,
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
    let mut seen_threads = HashSet::new();

    for db_path in &db_paths {
        for usage in db::read_threads(db_path, paths, min_since)? {
            if !slot_filters.is_empty() && !slot_filters.contains(&usage.slot) {
                continue;
            }
            if !seen_threads.insert(usage.id.clone()) {
                continue;
            }
            add_thread_usage(&mut accumulators, price_book.as_ref(), &usage);
        }
    }

    let price_source = price_book.as_ref().map(|book| book.source.clone());
    let price_note = price_book.as_ref().map(|book| book.note.clone());

    Ok(StatsReport {
        json: args.json,
        by_slot: args.by_slot,
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
    price_book: Option<&PriceBook>,
    usage: &ThreadUsage,
) {
    if usage.rollout_path.exists() {
        if let Ok(events) = rollout::read_token_usage_events(&usage.rollout_path) {
            if !events.is_empty() {
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
        }
    }

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

fn legacy_file_schema_version() -> u64 {
    LEGACY_FILE_SCHEMA_VERSION
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
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use clap::Parser;

    use crate::cli::Cli;
    use crate::cli::Command;

    use super::calibration::parse_mix_calibration;
    use super::calibration::read_mix_calibration;
    use super::db::infer_slot_from_rollout_path;
    use super::pricing::parse_price_cache;
    use super::pricing::parse_pricing_page;
    use super::pricing::read_price_cache;
    use super::pricing::PriceCachePolicy;
    use super::pricing::StatsPricePolicy;
    use super::pricing::DEFAULT_PRICE_URL;
    use super::pricing::PRICE_CACHE_FILE;
    use super::pricing::PRICE_CACHE_SCHEMA_VERSION;
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

    fn temp_paths(name: &str) -> ManagerPaths {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cx-stats-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths {
            base_codex_home: root.join("codex"),
            manager_dir: root.join("profile-manager"),
            slots_dir: root.join("profile-manager/slots"),
            targets_dir: root.join("profile-manager/targets"),
            rotation_file: root.join("profile-manager/rotation.txt"),
        }
    }

    #[test]
    fn stats_price_policy_defaults_to_local_only() {
        let args = parse_stats_args(&[]);

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
    fn legacy_price_cache_normalizes_on_read() {
        let paths = temp_paths("legacy-price-cache");
        fs::create_dir_all(&paths.manager_dir).expect("create manager dir");
        let path = paths.manager_dir.join(PRICE_CACHE_FILE);
        fs::write(
            &path,
            r#"{
              "fetchedAt": 123,
              "sourceUrl": "https://example.test/pricing",
              "prices": {
                "gpt-5.5": {
                  "inputPerMillion": 1.0,
                  "cachedInputPerMillion": 0.1,
                  "outputPerMillion": 2.0
                }
              }
            }"#,
        )
        .expect("write legacy price cache");

        let cache = read_price_cache(&paths).expect("read price cache");
        let persisted = fs::read_to_string(&path).expect("read normalized price cache");
        let persisted: serde_json::Value =
            serde_json::from_str(&persisted).expect("parse normalized price cache");

        assert_eq!(cache.schema_version, PRICE_CACHE_SCHEMA_VERSION);
        assert_eq!(
            persisted["schemaVersion"],
            serde_json::json!(PRICE_CACHE_SCHEMA_VERSION)
        );
    }

    #[test]
    fn legacy_mix_calibration_normalizes_on_read() {
        let paths = temp_paths("legacy-mix-calibration");
        fs::create_dir_all(&paths.manager_dir).expect("create manager dir");
        let path = paths.manager_dir.join(CALIBRATION_FILE);
        fs::write(
            &path,
            r#"{
              "calibratedAt": 123,
              "samples": 1,
              "sourceRollouts": 1,
              "totalTokens": 1050,
              "tokenMix": {
                "uncachedInputShare": 0.1,
                "cachedInputShare": 0.85,
                "outputShare": 0.05
              }
            }"#,
        )
        .expect("write legacy calibration");

        let calibration = read_mix_calibration(&path).expect("read calibration");
        let persisted = fs::read_to_string(&path).expect("read normalized calibration");
        let persisted: serde_json::Value =
            serde_json::from_str(&persisted).expect("parse normalized calibration");

        assert_eq!(calibration.schema_version, CALIBRATION_SCHEMA_VERSION);
        assert_eq!(
            persisted["schemaVersion"],
            serde_json::json!(CALIBRATION_SCHEMA_VERSION)
        );
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
        });

        let mix = totals.token_mix();

        assert!((mix.uncached_input_share - 0.095238).abs() < 0.00001);
        assert!((mix.cached_input_share - 0.857143).abs() < 0.00001);
        assert!((mix.output_share - 0.047619).abs() < 0.00001);
    }
}
