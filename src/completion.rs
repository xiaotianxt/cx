use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::io::Write;

use anyhow::Context;
use anyhow::Result;
use clap_complete::generate;
use clap_complete::Shell;

use crate::cli::CompleteArgs;
use crate::cli::CompleteKind;
use crate::cli::CompletionShell;
use crate::paths::ManagerPaths;
use crate::run;
use crate::slot;

const FISH_SLOT_COMPLETION: &str = r#"
# cx dynamic slot names
complete -c cx -s s -l slot -r -f -a "(cx __complete slots 2>/dev/null)"
"#;

const ZSH_SLOT_COMPLETION: &str = r#"
# cx dynamic slot names
_cx_with_slots() {
    local previous="${words[$((CURRENT - 1))]}"
    if [[ "${previous}" == "--slot" || "${previous}" == "-s" ]]; then
        local -a slots
        slots=("${(@f)$(cx __complete slots 2>/dev/null)}")
        _describe -t slots 'cx slots' slots
        return
    fi
    _cx "$@"
}

if [ "$funcstack[1]" = "_cx_with_slots" ]; then
    _cx_with_slots "$@"
else
    compdef _cx_with_slots cx
fi
"#;

const BASH_SLOT_COMPLETION: &str = r#"
# cx dynamic slot names
_cx_slot_names() {
    cx __complete slots 2>/dev/null
}

_cx_with_slots() {
    local cur prev
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"

    case "${cur}" in
        --slot=*)
            local prefix="${cur#--slot=}"
            COMPREPLY=( $(compgen -P "--slot=" -W "$(_cx_slot_names)" -- "${prefix}") )
            return 0
            ;;
    esac

    case "${prev}" in
        --slot|-s)
            COMPREPLY=( $(compgen -W "$(_cx_slot_names)" -- "${cur}") )
            return 0
            ;;
    esac

    _cx "$@"
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _cx_with_slots -o nosort -o bashdefault -o default cx
else
    complete -F _cx_with_slots -o bashdefault -o default cx
fi
"#;

pub fn print_script(shell: CompletionShell) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    write_script(shell, &mut out)
}

pub fn print_candidates(args: CompleteArgs) -> Result<()> {
    let candidates = match args.kind {
        CompleteKind::Slots => {
            let paths = ManagerPaths::new(args.manager_dir)?;
            slot_names(&paths)?
        }
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for candidate in candidates {
        writeln!(out, "{candidate}").context("write completion candidate")?;
    }
    Ok(())
}

fn write_script(shell: CompletionShell, out: &mut dyn Write) -> Result<()> {
    let shell = match shell {
        CompletionShell::Fish => Shell::Fish,
        CompletionShell::Zsh => Shell::Zsh,
        CompletionShell::Bash => Shell::Bash,
    };
    let mut command = run::launcher_command();
    generate(shell, &mut command, "cx", out);
    out.write_all(slot_completion_supplement(shell).as_bytes())
        .context("write slot completion supplement")?;
    Ok(())
}

fn slot_completion_supplement(shell: Shell) -> &'static str {
    match shell {
        Shell::Fish => FISH_SLOT_COMPLETION,
        Shell::Zsh => ZSH_SLOT_COMPLETION,
        Shell::Bash => BASH_SLOT_COMPLETION,
        _ => "",
    }
}

fn slot_names(paths: &ManagerPaths) -> Result<Vec<String>> {
    let mut candidates = BTreeSet::<String>::new();

    for slot in slot::load_rotation(paths).unwrap_or_default() {
        if slot::validate_slot_name(&slot).is_ok() {
            candidates.insert(slot);
        }
    }

    if paths.slots_dir.is_dir() {
        for entry in fs::read_dir(&paths.slots_dir)
            .with_context(|| format!("read {}", paths.slots_dir.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let slot = entry.file_name().to_string_lossy().to_string();
            if slot::validate_slot_name(&slot).is_ok() {
                candidates.insert(slot);
            }
        }
    }

    Ok(candidates.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use super::*;

    fn script(shell: CompletionShell) -> String {
        let mut output = Vec::new();
        write_script(shell, &mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn generated_scripts_are_launcher_only_with_dynamic_slots() {
        for script in [
            script(CompletionShell::Fish),
            script(CompletionShell::Zsh),
            script(CompletionShell::Bash),
        ] {
            assert!(script.contains("--slot") || script.contains("-l slot"));
            assert!(script.contains("--target") || script.contains("-l target"));
            assert!(script.contains("--manager-dir") || script.contains("-l manager-dir"));
            assert!(script.contains("__complete slots"));
            assert!(!script.contains("models_cache"));
            assert!(!script.contains("--delete-files"));
            assert!(!script.contains("Query every slot"));
            assert!(!script.contains("target show"));
        }
    }

    #[test]
    fn slot_names_come_from_rotation_and_slot_dirs() {
        let paths = temp_paths("slots");
        fs::create_dir_all(paths.slot_dir("dia1")).unwrap();
        fs::create_dir_all(paths.slot_dir("bus1")).unwrap();
        fs::create_dir_all(paths.slot_dir("bad slot")).unwrap();
        fs::create_dir_all(&paths.manager_dir).unwrap();
        fs::write(&paths.rotation_file, "bus1\nmissing\nbad slot\n").unwrap();

        assert_eq!(
            slot_names(&paths).unwrap(),
            vec![
                "bus1".to_string(),
                "dia1".to_string(),
                "missing".to_string()
            ]
        );

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    fn temp_paths(name: &str) -> ManagerPaths {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cx-completion-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
    }
}
