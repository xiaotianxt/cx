use anyhow::Result;
use serde::Serialize;

use crate::paths::ManagerPaths;
use crate::stats;
use crate::stats::CalibrationReport;
use crate::stats::ModelUsage;
use crate::stats::NamedUsage;
use crate::stats::PeriodUsage;
use crate::stats::StatsReport;
use crate::stats::TokenMix;
use crate::usage::format_refresh_in;
use crate::usage::SlotResult;

const BAR_WIDTH: usize = 20;

#[derive(Debug, Serialize)]
struct Report<'a> {
    selected: Option<&'a str>,
    results: &'a [SlotResult],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatsColumns {
    Tokens,
    TokensAndCost,
}

#[derive(Debug, Serialize)]
struct StatsJsonReport<'a> {
    #[serde(rename = "schemaVersion")]
    schema_version: u64,
    #[serde(rename = "sourceDatabases")]
    source_databases: &'a [String],
    #[serde(rename = "periodBasis")]
    period_basis: &'a str,
    #[serde(rename = "priceEstimate", skip_serializing_if = "Option::is_none")]
    price_estimate: Option<StatsJsonPriceEstimate<'a>>,
    periods: Vec<StatsJsonPeriod<'a>>,
}

#[derive(Debug, Serialize)]
struct StatsJsonPriceEstimate<'a> {
    source: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'a str>,
    #[serde(rename = "tokenMix", skip_serializing_if = "Option::is_none")]
    token_mix: Option<&'a TokenMix>,
    #[serde(rename = "tokenMixSource", skip_serializing_if = "Option::is_none")]
    token_mix_source: Option<&'a str>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum StatsJsonPeriod<'a> {
    Tokens(TokenPeriodJson<'a>),
    TokensAndCost(PricedPeriodJson<'a>),
}

#[derive(Debug, Serialize)]
struct TokenPeriodJson<'a> {
    period: &'a str,
    #[serde(rename = "sinceUnix")]
    since_unix: i64,
    threads: u64,
    tokens: u64,
    slots: Vec<TokenNamedUsageJson<'a>>,
    models: Vec<TokenModelUsageJson<'a>>,
}

#[derive(Debug, Serialize)]
struct PricedPeriodJson<'a> {
    period: &'a str,
    #[serde(rename = "sinceUnix")]
    since_unix: i64,
    threads: u64,
    tokens: u64,
    #[serde(rename = "estimatedCostUsd")]
    estimated_cost_usd: Option<f64>,
    #[serde(rename = "pricedTokens")]
    priced_tokens: u64,
    #[serde(rename = "unpricedTokens")]
    unpriced_tokens: u64,
    slots: Vec<PricedNamedUsageJson<'a>>,
    models: Vec<PricedModelUsageJson<'a>>,
}

#[derive(Debug, Serialize)]
struct TokenNamedUsageJson<'a> {
    name: &'a str,
    threads: u64,
    tokens: u64,
}

#[derive(Debug, Serialize)]
struct PricedNamedUsageJson<'a> {
    name: &'a str,
    threads: u64,
    tokens: u64,
    #[serde(rename = "estimatedCostUsd")]
    estimated_cost_usd: Option<f64>,
    #[serde(rename = "pricedTokens")]
    priced_tokens: u64,
    #[serde(rename = "unpricedTokens")]
    unpriced_tokens: u64,
}

#[derive(Debug, Serialize)]
struct TokenModelUsageJson<'a> {
    provider: &'a str,
    model: &'a str,
    threads: u64,
    tokens: u64,
}

#[derive(Debug, Serialize)]
struct PricedModelUsageJson<'a> {
    provider: &'a str,
    model: &'a str,
    threads: u64,
    tokens: u64,
    #[serde(rename = "estimatedCostUsd")]
    estimated_cost_usd: Option<f64>,
    #[serde(rename = "pricedTokens")]
    priced_tokens: u64,
    #[serde(rename = "unpricedTokens")]
    unpriced_tokens: u64,
}

pub fn print_report(results: &[SlotResult], selected: Option<&str>, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&Report { selected, results })?
        );
        return Ok(());
    }

    if let Some(selected) = selected {
        println!("selected: {selected}");
        println!();
    }

    for result in results {
        let mark = if selected == Some(result.slot.as_str()) {
            "*"
        } else {
            "-"
        };

        println!(
            "{mark} {:<18} {:<30} {:<18} score {:>5.1}%",
            truncate(&result.slot, 18),
            truncate(result.account_label.as_deref().unwrap_or("-"), 30),
            result.status.as_str(),
            result.score
        );
        print_window(
            "5h",
            result.five_hour_used_percent,
            result.five_hour_refresh_at,
        );
        print_window(
            "weekly",
            result.weekly_used_percent,
            result.weekly_refresh_at,
        );
        if result.five_hour_used_percent.is_none() && result.weekly_used_percent.is_none() {
            println!("  note    {}", result.summary);
        }
        println!();
    }
    Ok(())
}

pub fn print_no_available(results: &[SlotResult]) {
    eprintln!("cx: no available slots");
    for result in results {
        eprintln!(
            "  {}: {}; {}",
            result.slot,
            result.status.as_str(),
            result.summary
        );
    }
}

pub fn print_doctor(paths: &ManagerPaths, slots: &[String]) -> Result<()> {
    println!("manager: {}", paths.manager_dir.display());
    println!("slots: {}", paths.slots_dir.display());
    println!("rotation: {}", paths.rotation_file.display());
    println!("configured slots: {}", slots.len());
    if slots.is_empty() {
        println!("warning: rotation.txt is empty or missing");
    }
    for slot in slots {
        let slot_home = paths.slot_home(slot);
        let auth_path = slot_home.join("auth.json");
        let status = if slot_home.is_dir() {
            if auth_path.exists() {
                "ok"
            } else {
                "missing auth"
            }
        } else {
            "missing home"
        };
        println!("  {slot}: {status}");
    }
    Ok(())
}

pub fn print_stats(report: &StatsReport) -> Result<()> {
    if report.json {
        let json_report = StatsJsonReport::from_report(report);
        println!("{}", serde_json::to_string_pretty(&json_report)?);
        return Ok(());
    }

    let columns = StatsColumns::from_report(report);
    match report.source_databases.as_slice() {
        [single] => println!("state db: {single}"),
        databases => println!("state dbs: {}", databases.len()),
    }
    println!("basis: {}", report.period_basis);
    if let Some(source) = &report.price_source {
        println!("prices: {source}");
    }
    if let Some(note) = &report.price_note {
        println!("note: {note}");
    }
    println!();
    println!("{}", columns.header());
    for period in &report.periods {
        println!("{}", columns.period_row(period));
        if report.by_slot {
            for slot in &period.slots {
                println!("{}", columns.slot_row(slot));
            }
        }
    }

    if report
        .periods
        .iter()
        .any(|period| period.estimated_cost_usd.is_some() && period.unpriced_tokens > 0)
    {
        println!();
        println!("* est. cost excludes tokens for models without known OpenAI pricing.");
    }
    Ok(())
}

impl<'a> StatsJsonReport<'a> {
    fn from_report(report: &'a StatsReport) -> Self {
        let includes_price_estimates = report.includes_price_estimates();
        Self {
            schema_version: stats::STATS_JSON_SCHEMA_VERSION,
            source_databases: &report.source_databases,
            period_basis: &report.period_basis,
            price_estimate: StatsJsonPriceEstimate::from_report(report),
            periods: report
                .periods
                .iter()
                .map(|period| StatsJsonPeriod::from_usage(period, includes_price_estimates))
                .collect(),
        }
    }
}

impl<'a> StatsJsonPriceEstimate<'a> {
    fn from_report(report: &'a StatsReport) -> Option<Self> {
        let source = report.price_source.as_deref()?;
        Some(Self {
            source,
            note: report.price_note.as_deref(),
            token_mix: report.token_mix.as_ref(),
            token_mix_source: report.token_mix_source.as_deref(),
        })
    }
}

impl<'a> StatsJsonPeriod<'a> {
    fn from_usage(period: &'a PeriodUsage, includes_price_estimates: bool) -> Self {
        if includes_price_estimates {
            Self::TokensAndCost(PricedPeriodJson::from_usage(period))
        } else {
            Self::Tokens(TokenPeriodJson::from_usage(period))
        }
    }
}

impl<'a> TokenPeriodJson<'a> {
    fn from_usage(period: &'a PeriodUsage) -> Self {
        Self {
            period: &period.period,
            since_unix: period.since_unix,
            threads: period.threads,
            tokens: period.tokens,
            slots: period
                .slots
                .iter()
                .map(TokenNamedUsageJson::from_usage)
                .collect(),
            models: period
                .models
                .iter()
                .map(TokenModelUsageJson::from_usage)
                .collect(),
        }
    }
}

impl<'a> PricedPeriodJson<'a> {
    fn from_usage(period: &'a PeriodUsage) -> Self {
        Self {
            period: &period.period,
            since_unix: period.since_unix,
            threads: period.threads,
            tokens: period.tokens,
            estimated_cost_usd: period.estimated_cost_usd,
            priced_tokens: period.priced_tokens,
            unpriced_tokens: period.unpriced_tokens,
            slots: period
                .slots
                .iter()
                .map(PricedNamedUsageJson::from_usage)
                .collect(),
            models: period
                .models
                .iter()
                .map(PricedModelUsageJson::from_usage)
                .collect(),
        }
    }
}

impl<'a> TokenNamedUsageJson<'a> {
    fn from_usage(usage: &'a NamedUsage) -> Self {
        Self {
            name: &usage.name,
            threads: usage.threads,
            tokens: usage.tokens,
        }
    }
}

impl<'a> PricedNamedUsageJson<'a> {
    fn from_usage(usage: &'a NamedUsage) -> Self {
        Self {
            name: &usage.name,
            threads: usage.threads,
            tokens: usage.tokens,
            estimated_cost_usd: usage.estimated_cost_usd,
            priced_tokens: usage.priced_tokens,
            unpriced_tokens: usage.unpriced_tokens,
        }
    }
}

impl<'a> TokenModelUsageJson<'a> {
    fn from_usage(usage: &'a ModelUsage) -> Self {
        Self {
            provider: &usage.provider,
            model: &usage.model,
            threads: usage.threads,
            tokens: usage.tokens,
        }
    }
}

impl<'a> PricedModelUsageJson<'a> {
    fn from_usage(usage: &'a ModelUsage) -> Self {
        Self {
            provider: &usage.provider,
            model: &usage.model,
            threads: usage.threads,
            tokens: usage.tokens,
            estimated_cost_usd: usage.estimated_cost_usd,
            priced_tokens: usage.priced_tokens,
            unpriced_tokens: usage.unpriced_tokens,
        }
    }
}

impl StatsColumns {
    fn from_report(report: &StatsReport) -> Self {
        if report.includes_price_estimates() {
            Self::TokensAndCost
        } else {
            Self::Tokens
        }
    }

    fn header(self) -> String {
        match self {
            Self::Tokens => format!(
                "{:<8} {:>8} {:>12} {:>12}",
                "period", "threads", "tokens", "raw"
            ),
            Self::TokensAndCost => format!(
                "{:<8} {:>8} {:>12} {:>12} {:>12}",
                "period", "threads", "tokens", "raw", "est. cost"
            ),
        }
    }

    fn period_row(self, period: &PeriodUsage) -> String {
        match self {
            Self::Tokens => format!(
                "{:<8} {:>8} {:>12} {:>12}",
                period.period,
                period.threads,
                stats::human_tokens(period.tokens),
                period.tokens
            ),
            Self::TokensAndCost => format!(
                "{:<8} {:>8} {:>12} {:>12} {:>12}",
                period.period,
                period.threads,
                stats::human_tokens(period.tokens),
                period.tokens,
                format_cost(period.estimated_cost_usd, period.unpriced_tokens)
            ),
        }
    }

    fn slot_row(self, slot: &NamedUsage) -> String {
        match self {
            Self::Tokens => format!(
                "  {:<18} {:>8} {:>12}",
                truncate(&slot.name, 18),
                slot.threads,
                stats::human_tokens(slot.tokens)
            ),
            Self::TokensAndCost => format!(
                "  {:<18} {:>8} {:>12} {:>12}",
                truncate(&slot.name, 18),
                slot.threads,
                stats::human_tokens(slot.tokens),
                format_cost(slot.estimated_cost_usd, slot.unpriced_tokens)
            ),
        }
    }
}

pub fn print_stats_calibration(report: &CalibrationReport) -> Result<()> {
    if report.json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }

    println!("saved: {}", report.saved_to);
    match report.source_databases.as_slice() {
        [single] => println!("state db: {single}"),
        databases => println!("state dbs: {}", databases.len()),
    }
    println!("rollouts: {}", report.source_rollouts);
    println!("samples: {}", report.samples);
    println!("tokens: {}", stats::human_tokens(report.total_tokens));
    println!(
        "mix: {} uncached input, {} cached input, {} output",
        format_percent(report.token_mix.uncached_input_share),
        format_percent(report.token_mix.cached_input_share),
        format_percent(report.token_mix.output_share)
    );
    Ok(())
}

fn print_window(label: &str, used_percent: Option<f64>, refresh_at: Option<i64>) {
    let Some(used_percent) = used_percent else {
        return;
    };

    let remaining_percent = 100.0 - used_percent.clamp(0.0, 100.0);
    let refresh = refresh_at
        .and_then(format_refresh_in)
        .map(|value| format!("refresh {value}"))
        .unwrap_or_else(|| "refresh unknown".to_string());

    println!(
        "  {:<6} [{}] {:>5.1}% left  {}",
        label,
        progress_bar(remaining_percent),
        remaining_percent,
        refresh
    );
}

fn progress_bar(percent_remaining: f64) -> String {
    let filled = ((percent_remaining.clamp(0.0, 100.0) / 100.0) * BAR_WIDTH as f64).round();
    let filled = (filled as usize).min(BAR_WIDTH);
    let empty = BAR_WIDTH - filled;
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!(
            "{}…",
            prefix
                .chars()
                .take(max_chars.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        prefix
    }
}

fn format_cost(cost: Option<f64>, unpriced_tokens: u64) -> String {
    let Some(cost) = cost else {
        return "-".to_string();
    };
    let suffix = if unpriced_tokens > 0 { "*" } else { "" };
    if cost > 0.0 && cost < 0.005 {
        format!("<$0.01{suffix}")
    } else {
        format!("${cost:.2}{suffix}")
    }
}

fn format_percent(value: f64) -> String {
    format!("{:.2}%", value * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn period_usage(estimated_cost_usd: Option<f64>) -> PeriodUsage {
        PeriodUsage {
            period: "24h".to_string(),
            since_unix: 0,
            threads: 2,
            tokens: 12_500,
            estimated_cost_usd,
            priced_tokens: if estimated_cost_usd.is_some() {
                12_500
            } else {
                0
            },
            unpriced_tokens: if estimated_cost_usd.is_some() {
                0
            } else {
                12_500
            },
            slots: Vec::new(),
            models: Vec::new(),
        }
    }

    fn named_usage(estimated_cost_usd: Option<f64>) -> NamedUsage {
        NamedUsage {
            name: "primary".to_string(),
            threads: 2,
            tokens: 12_500,
            estimated_cost_usd,
            priced_tokens: if estimated_cost_usd.is_some() {
                12_500
            } else {
                0
            },
            unpriced_tokens: if estimated_cost_usd.is_some() {
                0
            } else {
                12_500
            },
        }
    }

    fn model_usage(estimated_cost_usd: Option<f64>) -> ModelUsage {
        ModelUsage {
            provider: "openai".to_string(),
            model: "gpt-5.5".to_string(),
            threads: 2,
            tokens: 12_500,
            estimated_cost_usd,
            priced_tokens: if estimated_cost_usd.is_some() {
                12_500
            } else {
                0
            },
            unpriced_tokens: if estimated_cost_usd.is_some() {
                0
            } else {
                12_500
            },
        }
    }

    fn stats_report(estimated_cost_usd: Option<f64>) -> StatsReport {
        let mut period = period_usage(estimated_cost_usd);
        period.slots.push(named_usage(estimated_cost_usd));
        period.models.push(model_usage(estimated_cost_usd));

        StatsReport {
            json: true,
            by_slot: false,
            source_databases: vec!["/tmp/state_5.sqlite".to_string()],
            period_basis: "threads.tokens_used bucketed by threads.updated_at".to_string(),
            price_source: estimated_cost_usd
                .map(|_| "cache: https://example.test/pricing".to_string()),
            price_note: estimated_cost_usd.map(|_| "estimate note".to_string()),
            token_mix: estimated_cost_usd.map(|_| TokenMix {
                uncached_input_share: 0.05,
                cached_input_share: 0.94,
                output_share: 0.01,
            }),
            token_mix_source: estimated_cost_usd.map(|_| "test calibration".to_string()),
            periods: vec![period],
        }
    }

    #[test]
    fn token_only_stats_columns_do_not_emit_cost_fields() {
        let period = period_usage(None);
        let slot = named_usage(None);

        assert_eq!(
            StatsColumns::Tokens.header(),
            "period    threads       tokens          raw"
        );
        assert_eq!(
            StatsColumns::Tokens.period_row(&period),
            "24h             2        12.5K        12500"
        );
        assert_eq!(
            StatsColumns::Tokens.slot_row(&slot),
            "  primary                   2        12.5K"
        );
    }

    #[test]
    fn priced_stats_columns_emit_cost_fields() {
        let period = period_usage(Some(1.25));
        let slot = named_usage(Some(1.25));

        assert_eq!(
            StatsColumns::TokensAndCost.header(),
            "period    threads       tokens          raw    est. cost"
        );
        assert_eq!(
            StatsColumns::TokensAndCost.period_row(&period),
            "24h             2        12.5K        12500        $1.25"
        );
        assert_eq!(
            StatsColumns::TokensAndCost.slot_row(&slot),
            "  primary                   2        12.5K        $1.25"
        );
    }

    #[test]
    fn token_only_stats_json_uses_v2_schema_without_cost_fields() {
        let report = stats_report(None);
        let value = serde_json::to_value(StatsJsonReport::from_report(&report))
            .expect("serialize stats json");
        let period = &value["periods"][0];
        let slot = &period["slots"][0];
        let model = &period["models"][0];

        assert_eq!(value["schemaVersion"], serde_json::json!(2));
        assert!(value.get("bySlot").is_none());
        assert!(value.get("priceEstimate").is_none());
        assert!(value.get("priceSource").is_none());
        assert!(value.get("priceNote").is_none());
        assert!(value.get("tokenMix").is_none());
        assert!(value.get("tokenMixSource").is_none());
        for usage in [period, slot, model] {
            assert!(usage.get("estimatedCostUsd").is_none());
            assert!(usage.get("pricedTokens").is_none());
            assert!(usage.get("unpricedTokens").is_none());
        }
    }

    #[test]
    fn priced_stats_json_uses_v2_schema_with_cost_fields() {
        let report = stats_report(Some(1.25));
        let value = serde_json::to_value(StatsJsonReport::from_report(&report))
            .expect("serialize stats json");
        let period = &value["periods"][0];
        let slot = &period["slots"][0];
        let model = &period["models"][0];

        assert_eq!(value["schemaVersion"], serde_json::json!(2));
        assert_eq!(
            value["priceEstimate"]["source"],
            serde_json::json!("cache: https://example.test/pricing")
        );
        assert!(value.get("priceSource").is_none());
        assert!(value.get("priceNote").is_none());
        for usage in [period, slot, model] {
            assert_eq!(usage["estimatedCostUsd"], serde_json::json!(1.25));
            assert_eq!(usage["pricedTokens"], serde_json::json!(12_500));
            assert_eq!(usage["unpricedTokens"], serde_json::json!(0));
        }
    }
}
