#[cfg(feature = "service")]
mod app_server;
#[cfg(feature = "service")]
mod approval;
mod auth;
#[cfg(feature = "service")]
mod broker;
#[cfg(feature = "service")]
mod channel;
mod cli;
mod completion;
#[cfg(feature = "service")]
mod control;
mod cx;
mod desktop;
mod envfile;
mod install;
mod output;
mod paths;
mod protocol_export;
#[cfg(feature = "service")]
mod rate_limit;
mod resume_id;
#[cfg(feature = "service")]
mod router;
mod run;
mod selector;
#[cfg(feature = "service")]
mod serve;
#[cfg(feature = "service")]
mod service;
#[cfg(feature = "service")]
mod session;
mod slot;
mod stats;
#[cfg(feature = "service")]
mod subscription;
mod target;
#[cfg(feature = "service")]
mod thread_directory;
#[cfg(feature = "service")]
mod thread_resolver;
mod transfer;
mod usage;
#[cfg(feature = "service")]
mod worker_pool;

use std::process::ExitCode;

use anyhow::Context;
use anyhow::Result;
use clap::CommandFactory;
use clap::Parser;
#[cfg(feature = "service")]
use cli::ChannelCommand;
use cli::Cli;
use cli::Command;
use cli::ProtocolCommand;
#[cfg(feature = "service")]
use cli::ServeCommand;
#[cfg(feature = "service")]
use cli::ServiceCommand;
use cli::StatusSort;
use cli::TargetCommand;
#[cfg(feature = "service")]
use cli::TelegramCommand;
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
    #[cfg(not(feature = "service"))]
    if let Some(command) = service_feature_entry_arg(first_arg) {
        anyhow::bail!(
            "`cx {command}` is not compiled into this build. Rebuild cx with `--features service` to use service, serve, or Telegram channel commands."
        );
    }
    if !is_management_entry_arg(first_arg) {
        return cx::run_from_args(raw_args.into_iter().skip(1).collect());
    }

    let cli = Cli::parse();
    match cli.command {
        Command::Status(args) => {
            let paths = paths::ManagerPaths::new(args.query.manager_dir.clone())?;
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
        Command::Desktop(args) => {
            desktop::launch(args)?;
        }
        #[cfg(feature = "service")]
        Command::Channel(args) => match args.command {
            ChannelCommand::Telegram(args) => match args.command {
                TelegramCommand::Run(args) => channel::telegram::run(args)?,
                TelegramCommand::Bind(args) => channel::telegram::bind(args)?,
                TelegramCommand::Menu(args) => channel::telegram::menu(args)?,
                TelegramCommand::Status(args) => channel::telegram::status(args)?,
            },
        },
        #[cfg(feature = "service")]
        Command::Serve(args) => match args.command {
            ServeCommand::Daemon(args) => control::daemon(args)?,
            ServeCommand::Ping(args) => control::ping(args)?,
            ServeCommand::Shutdown(args) => control::shutdown(args)?,
            ServeCommand::Session(args) => control::session(args)?,
            ServeCommand::Lease(args) => control::lease(args)?,
            ServeCommand::Event(args) => control::event(args)?,
            ServeCommand::Start(args) => serve::start(args)?,
            ServeCommand::Stop(args) => serve::stop(args)?,
            ServeCommand::Status(args) => serve::status(args)?,
            ServeCommand::Probe(args) => serve::probe(args)?,
            ServeCommand::Threads(args) => serve::threads(args)?,
        },
        #[cfg(feature = "service")]
        Command::Service(args) => match args.command {
            ServiceCommand::Start(args) => service::start(args)?,
            ServiceCommand::Run(args) => service::run(args)?,
            ServiceCommand::Stop(args) => service::stop(args)?,
            ServiceCommand::Status(args) => service::status(args)?,
            ServiceCommand::Logs(args) => service::logs(args)?,
            ServiceCommand::Token(args) => service::token(args)?,
            ServiceCommand::Install(args) => service::install(args)?,
            ServiceCommand::Uninstall(args) => service::uninstall(args)?,
        },
        Command::Managed(_) => {
            anyhow::bail!("{}", removed_managed_message());
        }
        Command::Protocol(args) => match args.command {
            ProtocolCommand::Export(args) => protocol_export::export(args)?,
        },
        Command::Transfer(args) => match args.command {
            TransferCommand::Export(args) => transfer::export(args)?,
            TransferCommand::Import(args) => transfer::import(args)?,
        },
        Command::Target(args) => match args.command {
            TargetCommand::List(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir)?;
                let targets = target::list_targets(&paths)?;
                output::print_targets(&targets, args.json)?;
            }
            TargetCommand::Show(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir)?;
                let target = target::load_target(&paths, &args.target)?;
                output::print_target(&target, args.json)?;
            }
            TargetCommand::Add(args) => {
                let paths = paths::ManagerPaths::new(args.manager_dir)?;
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
                if target::remove_target(&paths, &args.target)? {
                    println!("removed target: {}", args.target);
                } else {
                    println!("target not found: {}", args.target);
                }
            }
        },
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
        Command::Completions(args) => {
            completion::print_script(args.shell)?;
        }
        Command::Complete(args) => {
            completion::print_candidates(args)?;
        }
    }
    Ok(())
}

#[cfg(not(feature = "service"))]
fn service_feature_entry_arg(first_arg: Option<&str>) -> Option<&'static str> {
    match first_arg {
        Some("service") => Some("service"),
        Some("serve") => Some("serve"),
        Some("channel") => Some("channel"),
        _ => None,
    }
}

fn removed_managed_message() -> &'static str {
    #[cfg(feature = "service")]
    {
        "`cx managed` was removed; service remote is experimental. Start the service with `cx service start --no-telegram`, then opt in with `cx --cx-service-remote`."
    }
    #[cfg(not(feature = "service"))]
    {
        "`cx managed` was removed; service remote is not compiled into this build. Rebuild cx with `--features service` to use service remote."
    }
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

    #[cfg(not(feature = "service"))]
    #[test]
    fn service_feature_entry_args_report_missing_feature() {
        assert_eq!(service_feature_entry_arg(Some("service")), Some("service"));
        assert_eq!(service_feature_entry_arg(Some("serve")), Some("serve"));
        assert_eq!(service_feature_entry_arg(Some("channel")), Some("channel"));
        assert_eq!(service_feature_entry_arg(Some("status")), None);
    }
}
