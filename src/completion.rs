use std::io;

use anyhow::Result;
use clap_complete::generate;
use clap_complete::Shell;

use crate::cli::CompletionShell;
use crate::run;

pub fn print_script(shell: CompletionShell) -> Result<()> {
    let shell = match shell {
        CompletionShell::Fish => Shell::Fish,
        CompletionShell::Zsh => Shell::Zsh,
        CompletionShell::Bash => Shell::Bash,
    };
    let mut command = run::launcher_command();
    generate(shell, &mut command, "cx", &mut io::stdout());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(shell: CompletionShell) -> String {
        let shell = match shell {
            CompletionShell::Fish => Shell::Fish,
            CompletionShell::Zsh => Shell::Zsh,
            CompletionShell::Bash => Shell::Bash,
        };
        let mut command = run::launcher_command();
        let mut output = Vec::new();
        generate(shell, &mut command, "cx", &mut output);
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn generated_scripts_are_launcher_only() {
        for script in [
            script(CompletionShell::Fish),
            script(CompletionShell::Zsh),
            script(CompletionShell::Bash),
        ] {
            assert!(script.contains("--slot") || script.contains("-l slot"));
            assert!(script.contains("--target") || script.contains("-l target"));
            assert!(script.contains("--manager-dir") || script.contains("-l manager-dir"));
            assert!(!script.contains("__complete"));
            assert!(!script.contains("models_cache"));
            assert!(!script.contains("--delete-files"));
            assert!(!script.contains("Query every slot"));
            assert!(!script.contains("target show"));
        }
    }
}
