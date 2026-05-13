mod candidates;
mod engine;
mod shell;

use anyhow::Result;

use crate::cli::CompleteArgs;
use crate::cli::CompleteKind;
use crate::cli::CompletionShell;
use crate::paths::ManagerPaths;

pub fn print_script(shell: CompletionShell) -> Result<()> {
    shell::print_script(shell)
}

pub fn print_candidates(args: CompleteArgs) -> Result<()> {
    match args.kind {
        CompleteKind::Slots => {
            let paths = ManagerPaths::new(args.manager_dir)?;
            candidates::print_plain_values(candidates::slot_candidates(&paths)?)
        }
        CompleteKind::Targets => {
            let paths = ManagerPaths::new(args.manager_dir)?;
            candidates::print_plain_values(candidates::target_candidates(&paths)?)
        }
        CompleteKind::Words => {
            candidates::print_described_candidates(engine::word_candidates(&args)?)
        }
    }
}
