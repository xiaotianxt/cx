use std::io;
use std::io::Write;

use anyhow::Context;
use anyhow::Result;

use crate::cli::CompletionShell;

const FISH_COMPLETIONS: &str = r#"# cx fish completions
function __cx_complete
    set -l current (commandline -ct)
    set -l words (commandline -opc)
    cx __complete words --shell fish "--current=$current" -- $words 2>/dev/null
end

complete -c cx -f -a "(__cx_complete)"
"#;

const ZSH_COMPLETIONS: &str = r#"#compdef cx

_cx() {
  local -a candidates
  local value description
  while IFS=$'\t' read -r value description; do
    [[ -n "$value" ]] && candidates+=("${value}:${description}")
  done < <(cx __complete words --shell zsh --cursor "$CURRENT" "--current=${words[CURRENT]}" -- "${words[@]}" 2>/dev/null)
  _describe -t values 'cx completions' candidates
}

_cx "$@"
"#;

const BASH_COMPLETIONS: &str = r#"# cx bash completions
_cx() {
  local cur value description
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  while IFS=$'\t' read -r value description; do
    [[ -n "$value" ]] && COMPREPLY+=("$value")
  done < <(cx __complete words --shell bash --cursor "$COMP_CWORD" "--current=$cur" -- "${COMP_WORDS[@]}" 2>/dev/null)
}

complete -F _cx cx
"#;

pub(super) fn print_script(shell: CompletionShell) -> Result<()> {
    let script = match shell {
        CompletionShell::Fish => FISH_COMPLETIONS,
        CompletionShell::Zsh => ZSH_COMPLETIONS,
        CompletionShell::Bash => BASH_COMPLETIONS,
    };
    io::stdout()
        .write_all(script.as_bytes())
        .context("write completion script")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_shell_scripts_are_protocol_shims() {
        for script in [FISH_COMPLETIONS, ZSH_COMPLETIONS, BASH_COMPLETIONS] {
            assert!(script.contains("__complete words"));
            assert!(!script.contains("login remove"));
            assert!(!script.contains("target show"));
            assert!(!script.contains("--delete-files"));
            assert!(!script.contains("Query every slot"));
        }
    }
}
