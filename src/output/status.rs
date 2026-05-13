use anyhow::Result;
use serde::Serialize;

use crate::usage::SlotResult;

#[derive(Debug, Serialize)]
struct Report<'a> {
    selected: Option<&'a str>,
    complete: bool,
    #[serde(rename = "transientFailures")]
    transient_failures: usize,
    results: &'a [SlotResult],
}

pub fn print_report(results: &[SlotResult], selected: Option<&str>, json: bool) -> Result<()> {
    let transient_failures = transient_failure_count(results);
    let complete = transient_failures == 0;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&Report {
                selected,
                complete,
                transient_failures,
                results
            })?
        );
        return Ok(());
    }

    if let Some(selected) = selected {
        let unverified = results
            .iter()
            .find(|result| result.slot == selected)
            .is_some_and(|result| result.is_transient());
        if unverified {
            println!("selected: {selected} (unverified)");
        } else {
            println!("selected: {selected}");
        }
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
            super::truncate(&result.slot, 18),
            super::truncate(result.account_label.as_deref().unwrap_or("-"), 30),
            result.status.as_str(),
            result.score
        );
        super::print_window(
            "5h",
            result.five_hour_used_percent,
            result.five_hour_refresh_at,
        );
        super::print_window(
            "weekly",
            result.weekly_used_percent,
            result.weekly_refresh_at,
        );
        if result.five_hour_used_percent.is_none() && result.weekly_used_percent.is_none() {
            println!("  note    {}", result.summary);
        }
        println!();
    }
    if transient_failures > 0 {
        println!(
            "warning: {transient_failures}/{} slots could not be refreshed; no cached status was used",
            results.len()
        );
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

fn transient_failure_count(results: &[SlotResult]) -> usize {
    results
        .iter()
        .filter(|result| result.is_transient())
        .count()
}
