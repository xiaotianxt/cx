use anyhow::Result;
use serde::Serialize;

use crate::paths::ManagerPaths;
use crate::stats;
use crate::stats::CalibrationReport;
use crate::stats::StatsReport;
use crate::usage::format_refresh_in;
use crate::usage::SlotResult;

const BAR_WIDTH: usize = 20;

#[derive(Debug, Serialize)]
struct Report<'a> {
    selected: Option<&'a str>,
    results: &'a [SlotResult],
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
            "{mark} {:<18} {:<18} score {:>5.1}%",
            truncate(&result.slot, 18),
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
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }

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
    println!(
        "{:<8} {:>8} {:>12} {:>12} {:>12}",
        "period", "threads", "tokens", "raw", "est. cost"
    );
    for period in &report.periods {
        println!(
            "{:<8} {:>8} {:>12} {:>12} {:>12}",
            period.period,
            period.threads,
            stats::human_tokens(period.tokens),
            period.tokens,
            format_cost(period.estimated_cost_usd, period.unpriced_tokens)
        );
        if report.by_slot {
            for slot in &period.slots {
                println!(
                    "  {:<18} {:>8} {:>12} {:>12}",
                    truncate(&slot.name, 18),
                    slot.threads,
                    stats::human_tokens(slot.tokens),
                    format_cost(slot.estimated_cost_usd, slot.unpriced_tokens)
                );
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
