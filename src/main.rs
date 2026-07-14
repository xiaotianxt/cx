mod auth;
mod cache;
mod cli;
mod completion;
mod cx;
mod daemon;
mod desktop;
mod desktop_proxy;
mod envfile;
mod install;
mod keychain;
mod output;
mod paths;
mod prime;
mod run;
mod runtime_provider;
mod selector;
mod slot;
mod sqlite_merge;
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
use cli::PatCommand;
use cli::PrimeCommand;
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
    if desktop_proxy::enabled() {
        return desktop_proxy::run(raw_args.into_iter().skip(1).collect());
    }
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
            let options = selector::SlotQueryOptions::new(
                args.query.timeout,
                args.query.jobs,
                args.query.retries,
            )
            .with_no_cache(args.query.no_cache);
            let command_progress =
                output::CommandProgress::for_human_output(args.query.json || args.no_progress);
            let mut progress = command_progress.slot_query("checking slots");
            let mut results =
                selector::query_slots_with_progress(&paths, &slots, options, &mut progress)?;
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
            let command_progress = output::CommandProgress::for_human_output(args.json);
            let mut progress = command_progress.spinner(if args.calibrate {
                "calibrating token mix"
            } else {
                "collecting stats"
            });
            if args.calibrate {
                let report = stats::calibrate_mix(&paths, args)?;
                progress.finish_and_clear();
                output::print_stats_calibration(&report)?;
            } else {
                let report = stats::collect_report(&paths, args)?;
                progress.finish_and_clear();
                output::print_stats(&report)?;
            }
        }
        Command::Prime(args) => match args.command {
            PrimeCommand::Plan(args) => {
                let paths = paths::ManagerPaths::new(args.schedule.manager_dir.clone())?;
                upgrade::run_startup(&paths)?;
                prime::plan(&paths, args)?;
            }
            PrimeCommand::Install(args) => {
                let paths = paths::ManagerPaths::new(args.schedule.manager_dir.clone())?;
                upgrade::run_startup(&paths)?;
                prime::install(&paths, args)?;
            }
            PrimeCommand::Run(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
                upgrade::run_startup(&paths)?;
                prime::run(&paths, args)?;
            }
            PrimeCommand::Status(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
                upgrade::run_startup(&paths)?;
                prime::status(&paths, args)?;
            }
            PrimeCommand::Uninstall(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
                upgrade::run_startup(&paths)?;
                prime::uninstall(&paths, args)?;
            }
        },
        Command::Select(args) => {
            let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
            upgrade::run_startup(&paths)?;
            let slots = args.slots_or_rotation(&paths)?;
            let options = selector::SlotQueryOptions::new(args.timeout, args.jobs, args.retries)
                .with_no_cache(args.no_cache);
            let command_progress = output::CommandProgress::for_human_output(args.json);
            let mut progress = command_progress.slot_query("checking slots");
            let results =
                selector::query_slots_with_progress(&paths, &slots, options, &mut progress)?;
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
        Command::Pat(args) => {
            let paths =
                paths::ManagerPaths::new(args.command.manager_dir().map(std::path::PathBuf::from))?;
            upgrade::run_startup(&paths)?;
            match args.command {
                PatCommand::Add(args) => keychain::pat_add(&paths, &args)?,
                PatCommand::Check(args) => keychain::pat_check(&paths, &args)?,
                PatCommand::Remove(args) => keychain::pat_remove(&paths, &args)?,
                PatCommand::Refresh(args) => keychain::pat_refresh(&paths, &args)?,
            }
        }
        Command::Desktop(args) => {
            desktop::launch(args)?;
        }
        Command::Transfer(args) => match args.command {
            TransferCommand::Export(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
                upgrade::run_startup(&paths)?;
                let command_progress = output::CommandProgress::for_human_output(false);
                let mut progress = command_progress.spinner("exporting transfer bundle");
                transfer::export_with_paths(&paths, args)?;
                progress.finish_and_clear();
            }
            TransferCommand::Import(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
                upgrade::run_startup(&paths)?;
                let command_progress = output::CommandProgress::for_human_output(false);
                let mut progress = command_progress.spinner("importing transfer bundle");
                transfer::import_with_paths(&paths, args)?;
                progress.finish_and_clear();
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
                let options =
                    selector::SlotQueryOptions::new(args.timeout, args.jobs, args.retries);
                let command_progress = output::CommandProgress::for_human_output(false);
                let mut progress = command_progress.slot_query("checking slots");
                let results =
                    selector::query_slots_with_progress(&paths, &slots, options, &mut progress)?;
                let selected = selector::choose_result(&results).map(|result| result.slot.clone());
                output::print_report(&results, selected.as_deref(), false)?;
            }
        }
        Command::MergeSqlite(args) => {
            let paths = paths::ManagerPaths::new(args.manager_dir.clone())?;
            let report = sqlite_merge::merge_slot_databases(&paths, args.dry_run)?;
            if report.dry_run {
                println!(
                    "dry run: would merge {} slot database(s) into {}",
                    report.sources.len(),
                    report.shared_sqlite_home.display()
                );
                for slot in &report.sources {
                    println!("  {slot}");
                }
                println!("source thread rows: {}", report.source_threads);
                println!("current shared threads: {}", report.shared_threads);
            } else {
                println!(
                    "merged {} slot database(s) into {}",
                    report.sources.len(),
                    report.shared_sqlite_home.display()
                );
                for slot in &report.sources {
                    println!("  {slot}");
                }
                println!("source thread rows: {}", report.source_threads);
                println!("shared unique threads: {}", report.shared_threads);
                println!(
                    "removed legacy SQLite files: {}",
                    report.removed_legacy_files
                );
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
