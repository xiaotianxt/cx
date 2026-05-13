mod auth;
mod cli;
mod completion;
mod cx;
mod desktop;
mod envfile;
mod install;
mod output;
mod paths;
mod resume_id;
mod run;
mod selector;
mod slot;
mod stats;
mod target;
mod transfer;
mod upgrade;
mod usage;

use std::process::ExitCode;

use anyhow::Context;
use anyhow::Result;
use clap::CommandFactory;
use clap::Parser;
use cli::Cli;
use cli::Command;
use cli::StatusSort;
use cli::TargetCommand;
use cli::TransferCommand;

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
    if !is_management_entry_arg(first_arg) {
        return cx::run_from_args(raw_args.into_iter().skip(1).collect());
    }

    let cli = Cli::parse();
    match cli.command {
        Command::Status(args) => {
            let paths = paths::ManagerPaths::new(args.query.manager_dir.clone())?;
            upgrade::run_startup(&paths)?;
            let slots = args.slots_or_rotation(&paths)?;
            let mut results = selector::query_slots(&paths, &slots, args.query.timeout)?;
            match args.sort {
                StatusSort::Score => usage::sort_by_score_desc(&mut results),
                StatusSort::Rotation => {}
            }
            let selected = selector::choose_result(&results).map(|result| result.slot.clone());
            output::print_report(&results, selected.as_deref(), args.query.json)?;
        }
        Command::Stats(args) => {
            let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
            upgrade::run_startup(&paths)?;
            let mut args = args;
            if let Some(target_name) = args.target.as_deref().filter(|_| args.slots.is_empty()) {
                args.slots = target::load_target(&paths, target_name)?.slots_or_rotation(&paths)?;
            }
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
            upgrade::run_startup(&paths)?;
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
            upgrade::run_startup(&paths)?;
            slot::add_slot(&paths, args)?;
        }
        Command::Remove(args) => {
            let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
            upgrade::run_startup(&paths)?;
            slot::remove_slot(&paths, args)?;
        }
        Command::Login(args) => {
            let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
            upgrade::run_startup(&paths)?;
            slot::ensure_slot_layout(&paths, &args.slot)?;
            run::exec_slot_login(&paths, args)?;
        }
        Command::Desktop(args) => {
            desktop::launch(args)?;
        }
        Command::Transfer(args) => match args.command {
            TransferCommand::Export(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
                upgrade::run_startup(&paths)?;
                transfer::export_with_paths(&paths, args)?
            }
            TransferCommand::Import(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
                upgrade::run_startup(&paths)?;
                transfer::import_with_paths(&paths, args)?
            }
        },
        Command::Target(args) => match args.command {
            TargetCommand::List(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir)?;
                upgrade::run_startup(&paths)?;
                let targets = target::list_targets(&paths)?;
                output::print_targets(&targets, args.json)?;
            }
            TargetCommand::Show(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir)?;
                upgrade::run_startup(&paths)?;
                let target = target::load_target(&paths, &args.target)?;
                output::print_target(&target, args.json)?;
            }
            TargetCommand::Add(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir)?;
                upgrade::run_startup(&paths)?;
                target::save_target(
                    &paths,
                    target::TargetInput {
                        name: args.target.clone(),
                        slots: args.slots,
                        overrides: args.sets,
                        envs: args.envs,
                    },
                )?;
                println!("updated target: {}", args.target);
                println!("target file: {}", paths.target_file(&args.target).display());
            }
            TargetCommand::Remove(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir)?;
                upgrade::run_startup(&paths)?;
                if target::remove_target(&paths, &args.target)? {
                    println!("removed target: {}", args.target);
                } else {
                    println!("target not found: {}", args.target);
                }
            }
        },
        Command::Doctor(args) => {
            let paths = paths::ManagerPaths::new(args.manager_dir)?;
            upgrade::run_startup(&paths)?;
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
        Command::Completions(args) => {
            completion::print_script(args.shell)?;
        }
        Command::Complete(args) => {
            completion::print_candidates(args)?;
        }
    }
    Ok(())
}

fn is_management_entry_arg(first_arg: Option<&str>) -> bool {
    let Some(first_arg) = first_arg else {
        return false;
    };
    if matches!(first_arg, "help" | "-h" | "--help") {
        return true;
    }
    Cli::command()
        .get_subcommands()
        .any(|command| command.get_name() == first_arg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn management_entry_args_come_from_clap_subcommands() {
        assert!(is_management_entry_arg(Some("status")));
        assert!(is_management_entry_arg(Some("__complete")));
        assert!(is_management_entry_arg(Some("--help")));
        assert!(!is_management_entry_arg(Some("prompt")));
        assert!(!is_management_entry_arg(None));
    }
}
