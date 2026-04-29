use anyhow::Result;
use serde::Serialize;

use crate::paths::ManagerPaths;
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
