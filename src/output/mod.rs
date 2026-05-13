use std::collections::BTreeMap;
use std::io::IsTerminal;
#[cfg(unix)]
use std::os::fd::AsRawFd;

use anyhow::Result;
use serde::Serialize;
use time::macros::format_description;
use time::Date;
use time::Duration;
use time::OffsetDateTime;

use crate::cli::StatsRange;
use crate::cli::StatsRangeKind;
use crate::paths::ManagerPaths;
use crate::stats;
use crate::stats::CalibrationReport;
use crate::stats::DailyModelUsage;
use crate::stats::DailyUsage;
use crate::stats::ModelUsage;
use crate::stats::NamedUsage;
use crate::stats::PeriodUsage;
use crate::stats::StatsReport;
use crate::stats::TokenMix;
use crate::target::TargetSpec;
use crate::usage::format_refresh_in;

const BAR_WIDTH: usize = 20;
const CHART_PREFIX_WIDTH: usize = 9;
const DAILY_CHART_MODEL_LIMIT: usize = 5;
const DAILY_LINE_CHART_HEIGHT: usize = 8;
const MAX_DAILY_CHART_WIDTH: usize = 60;
const MAX_SELECTED_DAYS: usize = 3_660;
const MODEL_MIX_LIMIT: usize = 6;

mod status;

pub use status::print_no_available;
pub use status::print_report;

#[derive(Debug, Serialize)]
struct TargetListReport<'a> {
    targets: &'a [String],
}

#[derive(Debug, Serialize)]
struct TargetReport<'a> {
    name: &'a str,
    slots: &'a [String],
    overrides: Vec<String>,
    #[serde(rename = "envKeys")]
    env_keys: Vec<&'a str>,
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
    range: &'a str,
    #[serde(rename = "priceEstimate", skip_serializing_if = "Option::is_none")]
    price_estimate: Option<StatsJsonPriceEstimate<'a>>,
    periods: Vec<StatsJsonPeriod<'a>>,
    daily: Vec<StatsJsonDaily<'a>>,
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
enum StatsJsonDaily<'a> {
    Tokens(TokenDailyJson<'a>),
    TokensAndCost(PricedDailyJson<'a>),
}

#[derive(Debug, Serialize)]
struct TokenDailyJson<'a> {
    date: &'a str,
    threads: u64,
    tokens: u64,
    #[serde(rename = "inputTokens")]
    input_tokens: u64,
    #[serde(rename = "cachedInputTokens")]
    cached_input_tokens: u64,
    #[serde(rename = "outputTokens")]
    output_tokens: u64,
    #[serde(rename = "reasoningOutputTokens")]
    reasoning_output_tokens: u64,
    #[serde(rename = "uncategorizedTokens")]
    uncategorized_tokens: u64,
    models: Vec<TokenDailyModelJson<'a>>,
}

#[derive(Debug, Serialize)]
struct PricedDailyJson<'a> {
    date: &'a str,
    threads: u64,
    tokens: u64,
    #[serde(rename = "inputTokens")]
    input_tokens: u64,
    #[serde(rename = "cachedInputTokens")]
    cached_input_tokens: u64,
    #[serde(rename = "outputTokens")]
    output_tokens: u64,
    #[serde(rename = "reasoningOutputTokens")]
    reasoning_output_tokens: u64,
    #[serde(rename = "uncategorizedTokens")]
    uncategorized_tokens: u64,
    #[serde(rename = "estimatedCostUsd")]
    estimated_cost_usd: Option<f64>,
    #[serde(rename = "pricedTokens")]
    priced_tokens: u64,
    #[serde(rename = "unpricedTokens")]
    unpriced_tokens: u64,
    models: Vec<PricedDailyModelJson<'a>>,
}

#[derive(Debug, Serialize)]
struct TokenDailyModelJson<'a> {
    provider: &'a str,
    model: &'a str,
    threads: u64,
    tokens: u64,
    #[serde(rename = "inputTokens")]
    input_tokens: u64,
    #[serde(rename = "cachedInputTokens")]
    cached_input_tokens: u64,
    #[serde(rename = "outputTokens")]
    output_tokens: u64,
    #[serde(rename = "reasoningOutputTokens")]
    reasoning_output_tokens: u64,
    #[serde(rename = "uncategorizedTokens")]
    uncategorized_tokens: u64,
}

#[derive(Debug, Serialize)]
struct PricedDailyModelJson<'a> {
    provider: &'a str,
    model: &'a str,
    threads: u64,
    tokens: u64,
    #[serde(rename = "inputTokens")]
    input_tokens: u64,
    #[serde(rename = "cachedInputTokens")]
    cached_input_tokens: u64,
    #[serde(rename = "outputTokens")]
    output_tokens: u64,
    #[serde(rename = "reasoningOutputTokens")]
    reasoning_output_tokens: u64,
    #[serde(rename = "uncategorizedTokens")]
    uncategorized_tokens: u64,
    #[serde(rename = "estimatedCostUsd")]
    estimated_cost_usd: Option<f64>,
    #[serde(rename = "pricedTokens")]
    priced_tokens: u64,
    #[serde(rename = "unpricedTokens")]
    unpriced_tokens: u64,
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
        let audit = crate::slot::audit_slot_layout(paths, slot)?;
        let status = if audit.issues.is_empty() {
            "ok"
        } else if !audit.home_exists {
            "missing home"
        } else if !audit.auth_exists {
            "missing auth"
        } else {
            "layout issues"
        };
        println!("  {slot}: {status}");
        for issue in audit.issues {
            let relative_path = issue.path.strip_prefix(&slot_home).unwrap_or(&issue.path);
            println!(
                "    warning: {}: {}",
                relative_path.display(),
                issue.message
            );
        }
    }
    Ok(())
}

pub fn print_targets(targets: &[String], json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&TargetListReport { targets })?
        );
        return Ok(());
    }
    for target in targets {
        println!("{target}");
    }
    Ok(())
}

pub fn print_target(target: &TargetSpec, json: bool) -> Result<()> {
    let env_keys = target.env().keys().map(String::as_str).collect::<Vec<_>>();
    let overrides = target
        .overrides()
        .iter()
        .map(|line| redact_override(line))
        .collect::<Vec<_>>();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&TargetReport {
                name: target.name(),
                slots: target.slots(),
                overrides,
                env_keys,
            })?
        );
        return Ok(());
    }

    println!("target: {}", target.name());
    if target.slots().is_empty() {
        println!("slots: rotation.txt");
    } else {
        println!("slots: {}", target.slots().join(", "));
    }
    if !overrides.is_empty() {
        println!("set:");
        for line in overrides {
            println!("  {line}");
        }
    }
    if !env_keys.is_empty() {
        println!("env: {}", env_keys.join(", "));
    }
    Ok(())
}

pub fn print_stats(report: &StatsReport) -> Result<()> {
    if report.json {
        let json_report = StatsJsonReport::from_report(report);
        println!("{}", serde_json::to_string_pretty(&json_report)?);
        return Ok(());
    }

    print_daily_chart(report);
    print_model_mix(report);

    if report.by_slot {
        let columns = StatsColumns::from_report(report);
        println!("Slot Windows");
        println!("{}", columns.header());
        for period in &report.periods {
            println!("{}", columns.period_row(period));
            for slot in &period.slots {
                println!("{}", columns.slot_row(slot));
            }
        }
        println!();
    }

    if report
        .periods
        .iter()
        .any(|period| period.estimated_cost_usd.is_some() && period.unpriced_tokens > 0)
    {
        print_wrapped_text(
            "* est. cost excludes tokens for models without known OpenAI pricing.",
            0,
        );
    }
    if let Some(source) = &report.price_source {
        println!("Price estimates:");
        print_wrapped_text(&price_source_label(source), 2);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DisplayModelKey {
    provider: String,
    model: String,
}

#[derive(Debug, Clone, Default)]
struct DisplayModelUsage {
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

#[derive(Debug, Clone)]
struct ChartPoint {
    date: String,
    tokens: u64,
    models: BTreeMap<DisplayModelKey, u64>,
}

#[derive(Debug, Clone, Copy)]
struct ChartCell {
    glyph: char,
    color_index: Option<usize>,
}

fn print_daily_chart(report: &StatsReport) {
    let days = selected_daily_days(report);
    if days.is_empty() || days.iter().all(|day| day.tokens == 0) {
        return;
    }

    let width = daily_chart_width();
    let use_color = color_enabled();
    let points = chart_points(&days, width);
    let max_tokens = points.iter().map(|point| point.tokens).max().unwrap_or(0);
    let top_models = top_models_for_chart(&points, max_tokens, DAILY_CHART_MODEL_LIMIT);

    println!("Tokens per Day");
    print_line_chart(&points, &top_models, max_tokens, use_color);

    if !top_models.is_empty() {
        let legend = top_models
            .iter()
            .enumerate()
            .map(|(index, key)| {
                format!(
                    "{} {}",
                    model_marker(index, use_color),
                    truncate(&model_label(key), legend_label_width())
                )
            })
            .collect::<Vec<_>>();
        print_chart_legend(&legend);
    }
    println!();
    println!("{}", range_label(report.range.label(), use_color));
    print_wrapped_items(
        &range_total_items(&days, report.includes_price_estimates()),
        0,
    );
    println!();
}

fn print_model_mix(report: &StatsReport) {
    let days = selected_daily_days(report);
    let mut models = aggregate_model_usage(&days);
    let total_tokens = models.iter().map(|(_, usage)| usage.tokens).sum::<u64>();
    if total_tokens == 0 {
        return;
    }

    let use_color = color_enabled();
    println!("Model Mix");
    let other = if models.len() > MODEL_MIX_LIMIT {
        Some(combine_model_usage(&models.split_off(MODEL_MIX_LIMIT)))
    } else {
        None
    };

    for (index, (key, usage)) in models.iter().enumerate() {
        print_model_mix_row(index, key, usage, total_tokens, use_color);
    }

    if let Some(usage) = other {
        print_model_mix_row(
            MODEL_MIX_LIMIT,
            &DisplayModelKey {
                provider: "other".to_string(),
                model: "other".to_string(),
            },
            &usage,
            total_tokens,
            use_color,
        );
    }
    println!();
}

fn print_model_mix_row(
    index: usize,
    key: &DisplayModelKey,
    usage: &DisplayModelUsage,
    total_tokens: u64,
    use_color: bool,
) {
    let label_width = text_wrap_width().saturating_sub(4).clamp(8, 48);
    println!(
        "  {} {}",
        model_marker(index, use_color),
        truncate(&model_label(key), label_width)
    );
    print_wrapped_items(&model_mix_primary_parts(usage, total_tokens), 4);
    print_wrapped_items(&model_usage_detail_parts(usage), 4);
}

fn selected_daily_days(report: &StatsReport) -> Vec<DailyUsage> {
    let Some((start, end)) = range_bounds(&report.range, &report.daily) else {
        return Vec::new();
    };
    let by_date = report
        .daily
        .iter()
        .filter_map(|day| parse_date_key(&day.date).map(|date| (date, day)))
        .collect::<BTreeMap<_, _>>();

    let mut current = start;
    let mut days = Vec::new();
    while current <= end && days.len() < MAX_SELECTED_DAYS {
        let date = current.to_string();
        days.push(
            by_date
                .get(&current)
                .map(|day| (*day).clone())
                .unwrap_or_else(|| empty_daily_usage(date)),
        );
        let Some(next) = current.next_day() else {
            break;
        };
        current = next;
    }
    days
}

fn range_bounds(range: &StatsRange, days: &[DailyUsage]) -> Option<(Date, Date)> {
    let first = days
        .iter()
        .filter_map(|day| parse_date_key(&day.date))
        .min()?;
    let last_data = days
        .iter()
        .filter_map(|day| parse_date_key(&day.date))
        .max()?;
    let today = OffsetDateTime::now_utc().date();

    match range.kind() {
        StatsRangeKind::All => Some(clamp_selected_range(first, last_data)),
        StatsRangeKind::LastDays(days) => {
            let end = today.max(last_data);
            let span = Duration::days(i64::from(days.saturating_sub(1)));
            let start = end.checked_sub(span).unwrap_or(Date::MIN);
            Some(clamp_selected_range(start, end))
        }
        StatsRangeKind::Since(date) => {
            let start = *date;
            Some(clamp_selected_range(start, last_data.max(start)))
        }
        StatsRangeKind::Between { start, end } => {
            Some(clamp_selected_range(*start.min(end), *start.max(end)))
        }
    }
}

fn clamp_selected_range(start: Date, end: Date) -> (Date, Date) {
    let span_days = (end - start).whole_days().saturating_add(1);
    if span_days <= MAX_SELECTED_DAYS as i64 {
        return (start, end);
    }
    let start = end
        .checked_sub(Duration::days((MAX_SELECTED_DAYS - 1) as i64))
        .unwrap_or(start);
    (start, end)
}

fn empty_daily_usage(date: String) -> DailyUsage {
    DailyUsage {
        date,
        threads: 0,
        tokens: 0,
        input_tokens: 0,
        cached_input_tokens: 0,
        output_tokens: 0,
        reasoning_output_tokens: 0,
        uncategorized_tokens: 0,
        estimated_cost_usd: None,
        priced_tokens: 0,
        unpriced_tokens: 0,
        models: Vec::new(),
    }
}

fn top_models_for_chart(
    points: &[ChartPoint],
    max_tokens: u64,
    limit: usize,
) -> Vec<DisplayModelKey> {
    let mut usage_by_model = BTreeMap::<DisplayModelKey, (u64, u64)>::new();
    for point in points {
        for (key, tokens) in &point.models {
            let (total, peak) = usage_by_model.entry(key.clone()).or_default();
            *total += *tokens;
            *peak = (*peak).max(*tokens);
        }
    }

    let mut models = usage_by_model.into_iter().collect::<Vec<_>>();
    models.sort_by(
        |(left_key, (left_tokens, _)), (right_key, (right_tokens, _))| {
            right_tokens
                .cmp(left_tokens)
                .then_with(|| left_key.provider.cmp(&right_key.provider))
                .then_with(|| left_key.model.cmp(&right_key.model))
        },
    );

    let mut visible = models
        .iter()
        .filter(|(_, (_, peak))| chart_y(*peak, max_tokens) < DAILY_LINE_CHART_HEIGHT - 1)
        .take(limit)
        .map(|(key, _usage)| key.clone())
        .collect::<Vec<_>>();
    if visible.is_empty() {
        visible = models
            .into_iter()
            .take(1)
            .map(|(key, _usage)| key)
            .collect();
    }
    visible
}

fn aggregate_model_usage(days: &[DailyUsage]) -> Vec<(DisplayModelKey, DisplayModelUsage)> {
    let mut usage_by_model = BTreeMap::<DisplayModelKey, DisplayModelUsage>::new();
    for day in days {
        for model in &day.models {
            usage_by_model
                .entry(DisplayModelKey::from_daily_model(model))
                .or_default()
                .add_daily_model(model);
        }
    }

    let mut models = usage_by_model.into_iter().collect::<Vec<_>>();
    models.sort_by(|(left_key, left_usage), (right_key, right_usage)| {
        right_usage
            .tokens
            .cmp(&left_usage.tokens)
            .then_with(|| left_key.provider.cmp(&right_key.provider))
            .then_with(|| left_key.model.cmp(&right_key.model))
    });
    models
}

fn combine_model_usage(models: &[(DisplayModelKey, DisplayModelUsage)]) -> DisplayModelUsage {
    let mut combined = DisplayModelUsage::default();
    for (_key, usage) in models {
        combined.add_usage(usage);
    }
    combined
}

fn chart_points(days: &[DailyUsage], width: usize) -> Vec<ChartPoint> {
    if days.is_empty() || width == 0 {
        return Vec::new();
    }

    let bucket_count = if days.len() > width {
        width
    } else {
        days.len()
    };
    let mut compacted = Vec::with_capacity(bucket_count);
    for index in 0..bucket_count {
        let start = index * days.len() / bucket_count;
        let end = ((index + 1) * days.len() / bucket_count).max(start + 1);
        compacted.push(combine_days(&days[start..end]));
    }

    if compacted.len() >= width {
        return compacted;
    }

    (0..width)
        .map(|index| {
            let source = index * compacted.len() / width;
            compacted[source].clone()
        })
        .collect()
}

fn combine_days(days: &[DailyUsage]) -> ChartPoint {
    let mut point = ChartPoint {
        date: days
            .first()
            .map(|day| day.date.clone())
            .unwrap_or_else(|| "0000-00-00".to_string()),
        tokens: 0,
        models: BTreeMap::new(),
    };
    for day in days {
        point.tokens += day.tokens;
        for model in &day.models {
            *point
                .models
                .entry(DisplayModelKey::from_daily_model(model))
                .or_default() += model.tokens;
        }
    }
    point
}

fn print_line_chart(
    points: &[ChartPoint],
    top_models: &[DisplayModelKey],
    max_tokens: u64,
    use_color: bool,
) {
    if points.is_empty() {
        return;
    }

    let width = points.len();
    let mut grid = vec![vec![ChartCell::blank(); width]; DAILY_LINE_CHART_HEIGHT];
    for (index, key) in top_models.iter().enumerate() {
        let values = points
            .iter()
            .map(|point| point.models.get(key).copied().unwrap_or(0))
            .collect::<Vec<_>>();
        draw_series(&mut grid, &values, max_tokens, index);
    }

    for (row_index, row) in grid.iter().enumerate() {
        let label = y_axis_label(row_index, max_tokens);
        println!(
            "{:>7} │{}",
            label,
            row.iter()
                .map(|cell| render_chart_cell(*cell, use_color))
                .collect::<String>()
        );
    }
    println!("{:>7} └{}", "", "─".repeat(width));
    println!("{}", x_axis_labels(points));
}

fn draw_series(grid: &mut [Vec<ChartCell>], values: &[u64], max_tokens: u64, color_index: usize) {
    if values.is_empty() || grid.is_empty() {
        return;
    }

    let Some(first_positive) = values.iter().position(|value| *value > 0) else {
        return;
    };
    let last_positive = values
        .iter()
        .rposition(|value| *value > 0)
        .unwrap_or(first_positive);

    let bottom_y = DAILY_LINE_CHART_HEIGHT.saturating_sub(1);
    let first_y = chart_y(values[first_positive], max_tokens);
    if first_positive == 0 {
        put_chart_cell(grid, 0, first_y, '─', color_index);
    } else {
        draw_series_start(grid, first_positive, bottom_y, first_y, color_index);
    }

    let final_x = last_positive
        .checked_add(1)
        .filter(|index| *index < values.len())
        .unwrap_or(last_positive);
    for x in first_positive.saturating_add(1)..=final_x {
        let previous_y = chart_y(values[x - 1], max_tokens);
        let y = if x > last_positive {
            bottom_y
        } else {
            chart_y(values[x], max_tokens)
        };
        draw_series_step(grid, x, previous_y, y, color_index);
    }
}

fn draw_series_start(
    grid: &mut [Vec<ChartCell>],
    x: usize,
    bottom_y: usize,
    y: usize,
    color_index: usize,
) {
    if y == bottom_y {
        put_chart_cell(grid, x, y, '─', color_index);
        return;
    }
    draw_vertical(grid, x, y, bottom_y, color_index);
    put_chart_cell(grid, x, y, '╭', color_index);
}

fn draw_series_step(
    grid: &mut [Vec<ChartCell>],
    x: usize,
    previous_y: usize,
    y: usize,
    color_index: usize,
) {
    if y == previous_y {
        put_chart_cell(grid, x, y, '─', color_index);
    } else if y < previous_y {
        put_chart_cell(grid, x, previous_y, '╯', color_index);
        draw_vertical(grid, x, y, previous_y, color_index);
        put_chart_cell(grid, x, y, '╭', color_index);
    } else {
        put_chart_cell(grid, x, previous_y, '╮', color_index);
        draw_vertical(grid, x, previous_y, y, color_index);
        put_chart_cell(grid, x, y, '╰', color_index);
    }
}

fn draw_vertical(
    grid: &mut [Vec<ChartCell>],
    x: usize,
    top: usize,
    bottom: usize,
    color_index: usize,
) {
    let start = top.min(bottom) + 1;
    let end = top.max(bottom);
    for row in start..end {
        put_chart_cell(grid, x, row, '│', color_index);
    }
}

fn chart_y(value: u64, max_tokens: u64) -> usize {
    if max_tokens == 0 || DAILY_LINE_CHART_HEIGHT <= 1 {
        return DAILY_LINE_CHART_HEIGHT.saturating_sub(1);
    }
    let ratio = value as f64 / max_tokens as f64;
    let y = (1.0 - ratio.clamp(0.0, 1.0)) * (DAILY_LINE_CHART_HEIGHT - 1) as f64;
    y.round() as usize
}

fn put_chart_cell(
    grid: &mut [Vec<ChartCell>],
    x: usize,
    y: usize,
    glyph: char,
    color_index: usize,
) {
    let Some(row) = grid.get_mut(y) else {
        return;
    };
    let Some(cell) = row.get_mut(x) else {
        return;
    };
    if cell.glyph == ' ' || cell.color_index == Some(color_index) {
        cell.glyph = glyph;
        cell.color_index = Some(color_index);
    }
}

fn render_chart_cell(cell: ChartCell, use_color: bool) -> String {
    if cell.glyph == ' ' {
        return " ".to_string();
    }
    let value = cell.glyph.to_string();
    match cell.color_index {
        Some(index) => colorize(&value, index, use_color),
        None => value,
    }
}

fn y_axis_label(row_index: usize, max_tokens: u64) -> String {
    if row_index + 1 == DAILY_LINE_CHART_HEIGHT {
        return "0".to_string();
    }
    if max_tokens == 0 || DAILY_LINE_CHART_HEIGHT <= 1 {
        return String::new();
    }
    let numerator = (DAILY_LINE_CHART_HEIGHT - 1 - row_index) as u64;
    let denominator = (DAILY_LINE_CHART_HEIGHT - 1) as u64;
    stats::human_tokens(max_tokens.saturating_mul(numerator) / denominator)
}

fn x_axis_labels(points: &[ChartPoint]) -> String {
    let width = points.len();
    let mut line = vec![' '; width + CHART_PREFIX_WIDTH];
    if let Some(point) = points.first() {
        place_label(
            &mut line,
            CHART_PREFIX_WIDTH,
            &short_date_label(&point.date),
        );
    }
    if let Some(point) = points.get(width / 2) {
        let label = short_date_label(&point.date);
        let start = CHART_PREFIX_WIDTH + (width / 2).saturating_sub(label.chars().count() / 2);
        place_label(&mut line, start, &label);
    }
    if let Some(point) = points.last() {
        let label = short_date_label(&point.date);
        let start = CHART_PREFIX_WIDTH + width.saturating_sub(label.chars().count());
        place_label(&mut line, start, &label);
    }
    line.into_iter().collect()
}

fn place_label(line: &mut [char], start: usize, label: &str) {
    if start >= line.len() {
        return;
    }
    for (offset, ch) in label.chars().enumerate() {
        let index = start + offset;
        if index >= line.len() || line[index] != ' ' {
            break;
        }
        line[index] = ch;
    }
}

fn daily_chart_width() -> usize {
    daily_chart_width_for_terminal(terminal_width())
}

fn daily_chart_width_for_terminal(columns: usize) -> usize {
    columns
        .saturating_sub(CHART_PREFIX_WIDTH)
        .clamp(1, MAX_DAILY_CHART_WIDTH)
}

fn terminal_width() -> usize {
    terminal_width_from_ioctl()
        .or_else(terminal_width_from_env)
        .unwrap_or(100)
}

#[cfg(unix)]
fn terminal_width_from_ioctl() -> Option<usize> {
    let stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return None;
    }

    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: ioctl writes a winsize into a valid out pointer for stdout's file descriptor.
    let result = unsafe { libc::ioctl(stdout.as_raw_fd(), libc::TIOCGWINSZ, size.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    // SAFETY: a successful TIOCGWINSZ call initialized the winsize struct.
    let size = unsafe { size.assume_init() };
    (size.ws_col > 0).then_some(size.ws_col as usize)
}

#[cfg(not(unix))]
fn terminal_width_from_ioctl() -> Option<usize> {
    None
}

fn terminal_width_from_env() -> Option<usize> {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|columns| columns.parse::<usize>().ok())
        .filter(|columns| *columns > 0)
}

fn text_wrap_width() -> usize {
    terminal_width().clamp(1, 96)
}

fn legend_label_width() -> usize {
    text_wrap_width().saturating_sub(4).clamp(4, 24)
}

fn range_label(label: String, use_color: bool) -> String {
    format!("Range: {}", accent(&label, use_color))
}

fn price_source_label(source: &str) -> String {
    source.replace(
        "https://developers.openai.com/api/docs/pricing",
        "OpenAI pricing docs",
    )
}

fn range_total_items(days: &[DailyUsage], includes_price_estimates: bool) -> Vec<String> {
    let mut total = DisplayModelUsage::default();
    for day in days {
        total.tokens += day.tokens;
        total.input_tokens += day.input_tokens;
        total.cached_input_tokens += day.cached_input_tokens;
        total.output_tokens += day.output_tokens;
        total.reasoning_output_tokens += day.reasoning_output_tokens;
        total.uncategorized_tokens += day.uncategorized_tokens;
        total.priced_tokens += day.priced_tokens;
        total.unpriced_tokens += day.unpriced_tokens;
        if let Some(cost) = day.estimated_cost_usd {
            total.estimated_cost_usd = Some(total.estimated_cost_usd.unwrap_or(0.0) + cost);
        }
    }

    if !includes_price_estimates {
        total.estimated_cost_usd = None;
    }

    let mut parts = vec![format!("Total {}", stats::human_tokens(total.tokens))];
    if let Some(cost) = total.estimated_cost_usd {
        parts.push(format!(
            "Cost {}",
            format_cost(Some(cost), total.unpriced_tokens)
        ));
    }
    parts.extend(model_usage_detail_parts(&total));
    parts
}

fn print_wrapped_items(items: &[String], indent: usize) {
    if items.is_empty() {
        return;
    }

    let width = text_wrap_width().saturating_sub(indent).max(1);
    let indent = " ".repeat(indent);
    let mut line = String::new();
    for item in items {
        let next_len = if line.is_empty() {
            item.chars().count()
        } else {
            line.chars().count() + 3 + item.chars().count()
        };
        if !line.is_empty() && next_len > width {
            println!("{indent}{line}");
            line.clear();
        }
        if !line.is_empty() {
            line.push_str(" · ");
        }
        line.push_str(item);
    }
    if !line.is_empty() {
        println!("{indent}{line}");
    }
}

fn print_chart_legend(items: &[String]) {
    let width = text_wrap_width().saturating_sub(2).max(1);
    for row in balanced_item_rows(items, width) {
        println!("  {}", row.join(" · "));
    }
}

fn balanced_item_rows(items: &[String], width: usize) -> Vec<Vec<String>> {
    if items.is_empty() {
        return Vec::new();
    }

    for row_count in 1..=items.len() {
        let sizes = balanced_row_sizes(items.len(), row_count);
        if row_count > 1 && sizes.last() == Some(&1) {
            continue;
        }
        let rows = copy_item_rows(items, &sizes);
        if rows.iter().all(|row| joined_items_len(row) <= width) {
            return rows;
        }
    }

    items.iter().map(|item| vec![item.clone()]).collect()
}

fn balanced_row_sizes(item_count: usize, row_count: usize) -> Vec<usize> {
    let base = item_count / row_count;
    let extra = item_count % row_count;
    (0..row_count)
        .map(|index| base + usize::from(index < extra))
        .filter(|size| *size > 0)
        .collect()
}

fn copy_item_rows(items: &[String], sizes: &[usize]) -> Vec<Vec<String>> {
    let mut rows = Vec::with_capacity(sizes.len());
    let mut start = 0;
    for size in sizes {
        let end = start + size;
        rows.push(items[start..end].to_vec());
        start = end;
    }
    rows
}

fn joined_items_len(items: &[String]) -> usize {
    let item_len = items.iter().map(|item| item.chars().count()).sum::<usize>();
    item_len + items.len().saturating_sub(1) * 3
}

fn print_wrapped_text(text: &str, indent: usize) {
    let width = text_wrap_width().saturating_sub(indent).max(1);
    let indent = " ".repeat(indent);
    let mut line = String::new();
    for word in text.split_whitespace() {
        append_wrapped_word(&indent, width, &mut line, word);
    }
    if !line.is_empty() {
        println!("{indent}{line}");
    }
}

fn append_wrapped_word(indent: &str, width: usize, line: &mut String, word: &str) {
    let mut pending = word;
    loop {
        let word_len = pending.chars().count();
        let next_len = if line.is_empty() {
            word_len
        } else {
            line.chars().count() + 1 + word_len
        };
        if next_len <= width {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(pending);
            return;
        }
        if !line.is_empty() {
            println!("{indent}{line}");
            line.clear();
            continue;
        }

        let (head, tail) = split_word_at(pending, width);
        println!("{indent}{head}");
        if tail.is_empty() {
            return;
        }
        pending = tail;
    }
}

fn split_word_at(value: &str, max_chars: usize) -> (&str, &str) {
    if max_chars == 0 {
        return ("", value);
    }
    let mut end = value.len();
    for (count, (index, ch)) in value.char_indices().enumerate() {
        if count == max_chars {
            end = index;
            break;
        }
        end = index + ch.len_utf8();
    }
    value.split_at(end)
}

fn accent(value: &str, use_color: bool) -> String {
    if use_color {
        format!("\x1b[38;5;202m{value}\x1b[0m")
    } else {
        value.to_string()
    }
}

fn color_enabled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn colorize(value: &str, index: usize, use_color: bool) -> String {
    if !use_color {
        return value.to_string();
    }
    const COLORS: [u8; 7] = [147, 42, 214, 202, 45, 81, 246];
    format!(
        "\x1b[38;5;{}m{}\x1b[0m",
        COLORS[index % COLORS.len()],
        value
    )
}

fn model_marker(index: usize, use_color: bool) -> String {
    const MARKERS: [&str; 7] = ["●", "◆", "■", "▲", "◇", "□", "○"];
    let marker = MARKERS[index % MARKERS.len()];
    if use_color {
        return colorize(marker, index, use_color);
    }
    marker.to_string()
}

fn format_ratio(tokens: u64, total_tokens: u64) -> String {
    if total_tokens == 0 {
        return "0.0%".to_string();
    }
    format!("{:.1}%", tokens as f64 * 100.0 / total_tokens as f64)
}

fn model_mix_primary_parts(usage: &DisplayModelUsage, total_tokens: u64) -> Vec<String> {
    let mut parts = vec![
        format!("Share {}", format_ratio(usage.tokens, total_tokens)),
        format!("Total {}", stats::human_tokens(usage.tokens)),
    ];
    if let Some(cost) = usage.estimated_cost_usd {
        parts.push(format!(
            "Cost {}",
            format_cost(Some(cost), usage.unpriced_tokens)
        ));
    }
    parts
}

fn model_usage_detail_parts(usage: &DisplayModelUsage) -> Vec<String> {
    let mut parts = Vec::new();
    if usage.input_tokens > 0 {
        parts.push(format!("In {}", stats::human_tokens(usage.input_tokens)));
    }
    if usage.cached_input_tokens > 0 {
        parts.push(format!(
            "Cache {}",
            stats::human_tokens(usage.cached_input_tokens)
        ));
    }
    if usage.output_tokens > 0 {
        parts.push(format!("Out {}", stats::human_tokens(usage.output_tokens)));
    }
    if usage.reasoning_output_tokens > 0 {
        parts.push(format!(
            "Reason {}",
            stats::human_tokens(usage.reasoning_output_tokens)
        ));
    }
    if usage.uncategorized_tokens > 0 || parts.is_empty() {
        parts.push(format!(
            "Raw {}",
            stats::human_tokens(usage.uncategorized_tokens)
        ));
    }
    parts
}

fn model_label(key: &DisplayModelKey) -> String {
    if key.provider == "openai" || key.provider == "unknown" || key.provider == "other" {
        key.model.clone()
    } else {
        format!("{}/{}", key.provider, key.model)
    }
}

fn short_date_label(date: &str) -> String {
    let Some(date) = parse_date_key(date) else {
        return date.to_string();
    };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS
        .get((u8::from(date.month()).saturating_sub(1)) as usize)
        .copied()
        .unwrap_or("???");
    format!("{month} {}", date.day())
}

fn parse_date_key(date: &str) -> Option<Date> {
    Date::parse(date, format_description!("[year]-[month]-[day]")).ok()
}

impl ChartCell {
    fn blank() -> Self {
        Self {
            glyph: ' ',
            color_index: None,
        }
    }
}

impl DisplayModelKey {
    fn from_daily_model(model: &DailyModelUsage) -> Self {
        Self {
            provider: model.provider.clone(),
            model: model.model.clone(),
        }
    }
}

impl DisplayModelUsage {
    fn add_daily_model(&mut self, model: &DailyModelUsage) {
        self.tokens += model.tokens;
        self.input_tokens += model.input_tokens;
        self.cached_input_tokens += model.cached_input_tokens;
        self.output_tokens += model.output_tokens;
        self.reasoning_output_tokens += model.reasoning_output_tokens;
        self.uncategorized_tokens += model.uncategorized_tokens;
        self.priced_tokens += model.priced_tokens;
        self.unpriced_tokens += model.unpriced_tokens;
        if let Some(cost) = model.estimated_cost_usd {
            self.estimated_cost_usd = Some(self.estimated_cost_usd.unwrap_or(0.0) + cost);
        }
    }

    fn add_usage(&mut self, usage: &DisplayModelUsage) {
        self.tokens += usage.tokens;
        self.input_tokens += usage.input_tokens;
        self.cached_input_tokens += usage.cached_input_tokens;
        self.output_tokens += usage.output_tokens;
        self.reasoning_output_tokens += usage.reasoning_output_tokens;
        self.uncategorized_tokens += usage.uncategorized_tokens;
        self.priced_tokens += usage.priced_tokens;
        self.unpriced_tokens += usage.unpriced_tokens;
        if let Some(cost) = usage.estimated_cost_usd {
            self.estimated_cost_usd = Some(self.estimated_cost_usd.unwrap_or(0.0) + cost);
        }
    }
}

impl<'a> StatsJsonReport<'a> {
    fn from_report(report: &'a StatsReport) -> Self {
        let includes_price_estimates = report.includes_price_estimates();
        Self {
            schema_version: stats::STATS_JSON_SCHEMA_VERSION,
            source_databases: &report.source_databases,
            period_basis: &report.period_basis,
            range: report.range.key(),
            price_estimate: StatsJsonPriceEstimate::from_report(report),
            periods: report
                .periods
                .iter()
                .map(|period| StatsJsonPeriod::from_usage(period, includes_price_estimates))
                .collect(),
            daily: report
                .daily
                .iter()
                .map(|day| StatsJsonDaily::from_usage(day, includes_price_estimates))
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

impl<'a> StatsJsonDaily<'a> {
    fn from_usage(day: &'a DailyUsage, includes_price_estimates: bool) -> Self {
        if includes_price_estimates {
            Self::TokensAndCost(PricedDailyJson::from_usage(day))
        } else {
            Self::Tokens(TokenDailyJson::from_usage(day))
        }
    }
}

impl<'a> TokenDailyJson<'a> {
    fn from_usage(day: &'a DailyUsage) -> Self {
        Self {
            date: &day.date,
            threads: day.threads,
            tokens: day.tokens,
            input_tokens: day.input_tokens,
            cached_input_tokens: day.cached_input_tokens,
            output_tokens: day.output_tokens,
            reasoning_output_tokens: day.reasoning_output_tokens,
            uncategorized_tokens: day.uncategorized_tokens,
            models: day
                .models
                .iter()
                .map(TokenDailyModelJson::from_usage)
                .collect(),
        }
    }
}

impl<'a> PricedDailyJson<'a> {
    fn from_usage(day: &'a DailyUsage) -> Self {
        Self {
            date: &day.date,
            threads: day.threads,
            tokens: day.tokens,
            input_tokens: day.input_tokens,
            cached_input_tokens: day.cached_input_tokens,
            output_tokens: day.output_tokens,
            reasoning_output_tokens: day.reasoning_output_tokens,
            uncategorized_tokens: day.uncategorized_tokens,
            estimated_cost_usd: day.estimated_cost_usd,
            priced_tokens: day.priced_tokens,
            unpriced_tokens: day.unpriced_tokens,
            models: day
                .models
                .iter()
                .map(PricedDailyModelJson::from_usage)
                .collect(),
        }
    }
}

impl<'a> TokenDailyModelJson<'a> {
    fn from_usage(usage: &'a DailyModelUsage) -> Self {
        Self {
            provider: &usage.provider,
            model: &usage.model,
            threads: usage.threads,
            tokens: usage.tokens,
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: usage.reasoning_output_tokens,
            uncategorized_tokens: usage.uncategorized_tokens,
        }
    }
}

impl<'a> PricedDailyModelJson<'a> {
    fn from_usage(usage: &'a DailyModelUsage) -> Self {
        Self {
            provider: &usage.provider,
            model: &usage.model,
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

fn redact_override(line: &str) -> String {
    let Some((key, _value)) = line.split_once('=') else {
        return line.to_string();
    };
    let key_lower = key.to_ascii_lowercase();
    let sensitive = ["api_key", "credential", "password", "secret", "token"]
        .iter()
        .any(|needle| key_lower.contains(needle));
    if sensitive {
        format!("{}=<redacted>", key.trim())
    } else {
        line.to_string()
    }
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

    fn daily_usage(estimated_cost_usd: Option<f64>) -> DailyUsage {
        DailyUsage {
            date: "2026-05-12".to_string(),
            threads: 2,
            tokens: 12_500,
            input_tokens: 10_000,
            cached_input_tokens: 8_000,
            output_tokens: 2_500,
            reasoning_output_tokens: 500,
            uncategorized_tokens: 0,
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
            models: vec![DailyModelUsage {
                provider: "openai".to_string(),
                model: "gpt-5.5".to_string(),
                threads: 2,
                tokens: 12_500,
                input_tokens: 10_000,
                cached_input_tokens: 8_000,
                output_tokens: 2_500,
                reasoning_output_tokens: 500,
                uncategorized_tokens: 0,
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
            }],
        }
    }

    fn stats_report(estimated_cost_usd: Option<f64>) -> StatsReport {
        let mut period = period_usage(estimated_cost_usd);
        period.slots.push(named_usage(estimated_cost_usd));
        period.models.push(model_usage(estimated_cost_usd));

        StatsReport {
            json: true,
            by_slot: false,
            range: "all".parse().expect("stats range"),
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
            daily: vec![daily_usage(estimated_cost_usd)],
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
        let day = &value["daily"][0];
        let daily_model = &day["models"][0];

        assert_eq!(value["schemaVersion"], serde_json::json!(2));
        assert_eq!(value["range"], serde_json::json!("all"));
        assert!(value.get("bySlot").is_none());
        assert!(value.get("priceEstimate").is_none());
        assert!(value.get("priceSource").is_none());
        assert!(value.get("priceNote").is_none());
        assert!(value.get("tokenMix").is_none());
        assert!(value.get("tokenMixSource").is_none());
        assert_eq!(day["date"], serde_json::json!("2026-05-12"));
        assert_eq!(day["inputTokens"], serde_json::json!(10_000));
        assert_eq!(day["cachedInputTokens"], serde_json::json!(8_000));
        assert_eq!(day["outputTokens"], serde_json::json!(2_500));
        assert_eq!(day["reasoningOutputTokens"], serde_json::json!(500));
        for usage in [period, slot, model, day, daily_model] {
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
        let day = &value["daily"][0];
        let daily_model = &day["models"][0];

        assert_eq!(value["schemaVersion"], serde_json::json!(2));
        assert_eq!(value["range"], serde_json::json!("all"));
        assert_eq!(
            value["priceEstimate"]["source"],
            serde_json::json!("cache: https://example.test/pricing")
        );
        assert!(value.get("priceSource").is_none());
        assert!(value.get("priceNote").is_none());
        assert_eq!(day["inputTokens"], serde_json::json!(10_000));
        assert_eq!(daily_model["cachedInputTokens"], serde_json::json!(8_000));
        for usage in [period, slot, model, day, daily_model] {
            assert_eq!(usage["estimatedCostUsd"], serde_json::json!(1.25));
            assert_eq!(usage["pricedTokens"], serde_json::json!(12_500));
            assert_eq!(usage["unpricedTokens"], serde_json::json!(0));
        }
    }

    #[test]
    fn chart_points_stretch_short_ranges_horizontally() {
        let mut first = daily_usage(None);
        first.date = "2026-05-10".to_string();
        first.tokens = 100;
        first.models[0].tokens = 100;
        let mut second = daily_usage(None);
        second.date = "2026-05-11".to_string();
        second.tokens = 200;
        second.models[0].tokens = 200;

        let points = chart_points(&[first, second], 6);

        assert_eq!(points.len(), 6);
        assert_eq!(points[0].date, "2026-05-10");
        assert_eq!(points[2].tokens, 100);
        assert_eq!(points[3].date, "2026-05-11");
        assert_eq!(points[5].tokens, 200);
    }

    #[test]
    fn daily_chart_width_accounts_for_axis_prefix() {
        assert_eq!(daily_chart_width_for_terminal(40) + CHART_PREFIX_WIDTH, 40);
        assert_eq!(daily_chart_width_for_terminal(80), MAX_DAILY_CHART_WIDTH);
        assert_eq!(daily_chart_width_for_terminal(140), MAX_DAILY_CHART_WIDTH);
        assert_eq!(daily_chart_width_for_terminal(20) + CHART_PREFIX_WIDTH, 20);
    }

    #[test]
    fn chart_legend_balances_four_items_in_two_rows() {
        let items = vec![
            "● gpt-5.5".to_string(),
            "◆ gpt-5.4".to_string(),
            "■ gpt-5.3-codex".to_string(),
            "▲ gpt-5.4-mini".to_string(),
        ];

        let rows = balanced_item_rows(&items, 32);

        assert_eq!(
            rows,
            vec![
                vec!["● gpt-5.5".to_string(), "◆ gpt-5.4".to_string()],
                vec!["■ gpt-5.3-codex".to_string(), "▲ gpt-5.4-mini".to_string()],
            ]
        );
        assert!(rows.iter().all(|row| joined_items_len(row) <= 32));
    }

    #[test]
    fn draw_series_keeps_step_turns_on_one_column() {
        let mut grid = vec![vec![ChartCell::blank(); 3]; DAILY_LINE_CHART_HEIGHT];

        draw_series(&mut grid, &[100, 80], 100, 0);

        assert_eq!(grid[0][0].glyph, '─');
        assert_eq!(grid[0][1].glyph, '╮');
        assert_eq!(grid[1][1].glyph, '╰');
    }

    #[test]
    fn model_markers_keep_shape_without_color() {
        assert_eq!(model_marker(0, false), "●");
        assert_eq!(model_marker(1, false), "◆");
        assert_eq!(model_marker(2, false), "■");
    }

    #[test]
    fn model_mix_primary_parts_keep_cost_out_of_detail_line() {
        let usage = DisplayModelUsage {
            tokens: 100,
            input_tokens: 80,
            output_tokens: 20,
            estimated_cost_usd: Some(1.25),
            ..DisplayModelUsage::default()
        };

        assert_eq!(
            model_mix_primary_parts(&usage, 200),
            vec![
                "Share 50.0%".to_string(),
                "Total 100".to_string(),
                "Cost $1.25".to_string(),
            ]
        );
        assert_eq!(
            model_usage_detail_parts(&usage),
            vec!["In 80".to_string(), "Out 20".to_string()]
        );
    }

    #[test]
    fn selected_daily_days_fills_missing_dates() {
        let mut report = stats_report(None);
        let mut second = daily_usage(None);
        second.date = "2026-05-14".to_string();
        report.daily.push(second);

        let days = selected_daily_days(&report);

        assert_eq!(days.len(), 3);
        assert_eq!(days[0].date, "2026-05-12");
        assert_eq!(days[1].date, "2026-05-13");
        assert_eq!(days[1].tokens, 0);
        assert_eq!(days[2].date, "2026-05-14");
    }

    #[test]
    fn target_override_display_redacts_sensitive_values() {
        assert_eq!(redact_override("api_key=\"sk-test\""), "api_key=<redacted>");
        assert_eq!(redact_override("model=\"gpt-5.5\""), "model=\"gpt-5.5\"");
    }
}
