use anyhow::Result;
use serde::Serialize;

use crate::paths::ManagerPaths;
use crate::usage::SlotResult;

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

    println!(
        "{:<2} {:<16} {:<18} {:>7} {:>9} {:>9}  {}",
        "", "slot", "status", "score", "primary", "secondary", "summary"
    );
    for result in results {
        let mark = if selected == Some(result.slot.as_str()) {
            "*"
        } else {
            "-"
        };
        println!(
            "{:<2} {:<16} {:<18} {:>6.1}% {:>8} {:>9}  {}",
            mark,
            result.slot,
            result.status.as_str(),
            result.score,
            percent(result.primary_used_percent),
            percent(result.secondary_used_percent),
            result.summary
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

fn percent(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.1}%"))
        .unwrap_or_else(|| "-".to_string())
}
