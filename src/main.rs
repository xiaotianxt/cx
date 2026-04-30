mod auth;
mod cli;
mod cx;
mod envfile;
mod install;
mod output;
mod paths;
mod run;
mod selector;
mod slot;
mod stats;
mod usage;

use std::process::ExitCode;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use cli::Cli;
use cli::Command;

fn main() -> ExitCode {
    match entry() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("cx: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn entry() -> Result<()> {
    let raw_args = std::env::args_os().collect::<Vec<_>>();
    let first_arg = raw_args.get(1).and_then(|arg| arg.to_str());
    if first_arg == Some("--") {
        return cx::run_from_args(raw_args.into_iter().skip(2).collect());
    }
    if !matches!(
        first_arg,
        Some(
            "status"
                | "stats"
                | "select"
                | "add"
                | "remove"
                | "login"
                | "doctor"
                | "install"
                | "help"
                | "-h"
                | "--help"
        )
    ) {
        return cx::run_from_args(raw_args.into_iter().skip(1).collect());
    }

    let cli = Cli::parse();
    match cli.command {
        Command::Status(args) => {
            let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
            let slots = args.slots_or_rotation(&paths)?;
            let results = selector::query_slots(&paths, &slots, args.timeout)?;
            let selected = selector::choose_result(&results).map(|result| result.slot.clone());
            output::print_report(&results, selected.as_deref(), args.json)?;
        }
        Command::Stats(args) => {
            let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
            if args.calibrate {
                let report = stats::calibrate_mix(&paths, args)?;
                output::print_stats_calibration(&report)?;
            } else {
                let report = stats::collect_report(&paths, args)?;
                output::print_stats(&report)?;
            }
        }
        Command::Select(args) => {
            let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
            let slots = args.slots_or_rotation(&paths)?;
            let results = selector::query_slots(&paths, &slots, args.timeout)?;
            let selected = selector::choose_result(&results);
            if args.json {
                output::print_report(&results, selected.map(|result| result.slot.as_str()), true)?;
            } else if let Some(result) = selected {
                println!("{}", result.slot);
            } else {
                output::print_no_available(&results);
                anyhow::bail!("no available slot");
            }
        }
        Command::Add(args) => {
            let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
            slot::add_slot(&paths, args)?;
        }
        Command::Remove(args) => {
            let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
            slot::remove_slot(&paths, args)?;
        }
        Command::Login(args) => {
            let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
            slot::ensure_slot_layout(&paths, &args.slot)?;
            run::exec_slot_login(&paths, args)?;
        }
        Command::Doctor(args) => {
            let paths = paths::ManagerPaths::new(args.manager_dir)?;
            let slots = slot::load_rotation(&paths)?;
            output::print_doctor(&paths, &slots)?;
            if args.online {
                let results = selector::query_slots(&paths, &slots, args.timeout)?;
                let selected = selector::choose_result(&results).map(|result| result.slot.clone());
                output::print_report(&results, selected.as_deref(), false)?;
            }
        }
        Command::Install(args) => {
            install::install(args).context("failed to install cx")?;
        }
    }
    Ok(())
}
