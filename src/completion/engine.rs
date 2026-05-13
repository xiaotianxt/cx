use std::path::PathBuf;

use anyhow::Result;
use clap::Arg;
use clap::Command;
use clap::CommandFactory;

use super::candidates;
use super::candidates::Candidate;
use crate::cli::Cli;
use crate::cli::CompleteArgs;
use crate::cli::CompletionShell;
use crate::paths::ManagerPaths;
use crate::run;

pub(super) fn word_candidates(args: &CompleteArgs) -> Result<Vec<Candidate>> {
    let current = args.current.as_deref().unwrap_or_default();
    let shell = args.shell.unwrap_or(CompletionShell::Bash);
    let before = words_before_current(shell, args.cursor, current, &args.words);
    let manager_dir = args
        .manager_dir
        .clone()
        .or_else(|| manager_dir_from(&before));
    let paths = ManagerPaths::new(manager_dir)?;
    let user_words = strip_program_name(&before);

    if user_words.iter().any(|word| word == "--") {
        return Ok(Vec::new());
    }
    if let Some(candidates) = complete_attached_value(current, &user_words, &paths)? {
        return Ok(candidates);
    }
    if let Some(candidates) = complete_value_after_previous(current, &user_words, &paths)? {
        return Ok(candidates);
    }

    let candidates = if is_management_mode(&user_words) {
        let root = Cli::command();
        management_candidates(&root, current, &user_words, &paths)?
    } else {
        launcher_candidates(current)
    };
    Ok(candidates::filter_candidates(candidates, current))
}

fn words_before_current(
    shell: CompletionShell,
    cursor: Option<usize>,
    current: &str,
    words: &[String],
) -> Vec<String> {
    match shell {
        CompletionShell::Bash => {
            let end = cursor.unwrap_or(words.len()).min(words.len());
            words[..end].to_vec()
        }
        CompletionShell::Zsh => {
            let end = cursor
                .and_then(|value| value.checked_sub(1))
                .unwrap_or(words.len())
                .min(words.len());
            words[..end].to_vec()
        }
        CompletionShell::Fish => {
            if !current.is_empty() && words.last().is_some_and(|word| word == current) {
                words[..words.len() - 1].to_vec()
            } else {
                words.to_vec()
            }
        }
    }
}

fn strip_program_name(words: &[String]) -> Vec<String> {
    if words.first().is_some_and(|word| word == "cx") {
        words[1..].to_vec()
    } else {
        words.to_vec()
    }
}

fn manager_dir_from(words: &[String]) -> Option<PathBuf> {
    let mut iter = words.iter().peekable();
    while let Some(word) = iter.next() {
        if word == "--manager-dir" {
            return iter.peek().map(|value| PathBuf::from(value.as_str()));
        }
        if let Some(value) = word.strip_prefix("--manager-dir=") {
            return Some(PathBuf::from(value));
        }
    }
    None
}

fn is_management_mode(user_words: &[String]) -> bool {
    let Some(first) = user_words.first() else {
        return false;
    };
    Cli::command()
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .any(|command| command.get_name() == first)
}

fn launcher_candidates(current: &str) -> Vec<Candidate> {
    if current.starts_with('-') {
        candidates::option_candidates(&run::launcher_command())
    } else {
        candidates::command_candidates(&Cli::command())
    }
}

fn management_candidates(
    root: &Command,
    current: &str,
    user_words: &[String],
    paths: &ManagerPaths,
) -> Result<Vec<Candidate>> {
    let context = command_context(root, user_words);
    if current.starts_with('-') {
        return Ok(candidates::option_candidates(context.command));
    }
    if context
        .command
        .get_subcommands()
        .any(|command| !command.is_hide_set())
    {
        return Ok(candidates::command_candidates(context.command));
    }
    positional_candidates(&context, paths)
}

#[derive(Debug)]
struct CompletionContext<'a> {
    command: &'a Command,
    positionals: Vec<String>,
}

fn command_context<'a>(root: &'a Command, user_words: &[String]) -> CompletionContext<'a> {
    let mut command = root;
    let mut positionals = Vec::new();
    let mut expecting_value = false;

    for word in user_words {
        if expecting_value {
            expecting_value = false;
            continue;
        }
        if word == "--" {
            break;
        }
        if let Some(arg) = long_arg_from_word(command, word) {
            expecting_value = option_takes_value(arg) && !word.contains('=');
            continue;
        }
        if let Some(arg) = short_arg_from_word(command, word) {
            expecting_value = option_takes_value(arg);
            continue;
        }
        if word.starts_with('-') {
            continue;
        }
        if let Some(subcommand) = visible_subcommand(command, word) {
            command = subcommand;
            positionals.clear();
            continue;
        }
        positionals.push(word.clone());
    }

    CompletionContext {
        command,
        positionals,
    }
}

fn complete_attached_value(
    current: &str,
    user_words: &[String],
    paths: &ManagerPaths,
) -> Result<Option<Vec<Candidate>>> {
    let Some((option_name, value_prefix)) = current.strip_prefix("--").and_then(|value| {
        let (name, value) = value.split_once('=')?;
        Some((name, value))
    }) else {
        return Ok(None);
    };

    let command = active_command(user_words);
    let Some(arg) = command
        .get_arguments()
        .find(|arg| !arg.is_hide_set() && arg.get_long() == Some(option_name))
    else {
        return Ok(None);
    };
    let Some(candidates) = value_candidates_for_arg(arg, value_prefix, paths)? else {
        return Ok(None);
    };
    let prefix = format!("--{option_name}=");
    Ok(Some(
        candidates
            .into_iter()
            .map(|candidate| Candidate {
                value: format!("{prefix}{}", candidate.value),
                description: candidate.description,
            })
            .collect(),
    ))
}

fn complete_value_after_previous(
    current: &str,
    user_words: &[String],
    paths: &ManagerPaths,
) -> Result<Option<Vec<Candidate>>> {
    let Some(previous) = user_words.last() else {
        return Ok(None);
    };
    let command_words = &user_words[..user_words.len() - 1];
    let command = active_command(command_words);

    option_from_token(&command, previous)
        .map(|arg| value_candidates_for_arg(arg, current, paths))
        .transpose()
        .map(Option::flatten)
}

fn active_command(user_words: &[String]) -> Command {
    if is_management_mode(user_words) {
        let root = Cli::command();
        command_context(&root, user_words).command.clone()
    } else {
        run::launcher_command()
    }
}

fn positional_candidates(
    context: &CompletionContext<'_>,
    paths: &ManagerPaths,
) -> Result<Vec<Candidate>> {
    let index = context.positionals.len();
    let Some(arg) = positional_arg(context.command, index) else {
        return Ok(Vec::new());
    };

    if let Some(completion) = candidates::completion_for_arg(arg) {
        return candidates::value_candidates(completion, "", paths);
    }

    Ok(candidates::filter_candidates(
        candidates::possible_value_candidates(arg),
        "",
    ))
}

fn positional_arg(command: &Command, index: usize) -> Option<&Arg> {
    let positionals = command
        .get_positionals()
        .filter(|arg| !arg.is_hide_set())
        .collect::<Vec<_>>();
    positionals
        .get(index)
        .copied()
        .or_else(|| positionals.last().copied())
}

fn value_candidates_for_arg(
    arg: &Arg,
    prefix: &str,
    paths: &ManagerPaths,
) -> Result<Option<Vec<Candidate>>> {
    if !option_takes_value(arg) {
        return Ok(None);
    }
    if let Some(completion) = candidates::completion_for_arg(arg) {
        return candidates::value_candidates(completion, prefix, paths).map(Some);
    }

    let candidates = candidates::possible_value_candidates(arg);
    if candidates.is_empty() {
        Ok(None)
    } else {
        Ok(Some(candidates::filter_candidates(candidates, prefix)))
    }
}

fn option_takes_value(arg: &Arg) -> bool {
    arg.get_action().takes_values()
}

fn option_from_token<'a>(command: &'a Command, token: &str) -> Option<&'a Arg> {
    long_arg_from_word(command, token).or_else(|| short_arg_from_word(command, token))
}

fn long_arg_from_word<'a>(command: &'a Command, word: &str) -> Option<&'a Arg> {
    let name = word.strip_prefix("--")?.split_once('=').map_or_else(
        || word.strip_prefix("--").unwrap_or_default(),
        |(name, _)| name,
    );
    command
        .get_arguments()
        .find(|arg| !arg.is_hide_set() && arg.get_long() == Some(name))
}

fn short_arg_from_word<'a>(command: &'a Command, word: &str) -> Option<&'a Arg> {
    let mut chars = word.strip_prefix('-')?.chars();
    let short = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    command
        .get_arguments()
        .find(|arg| !arg.is_hide_set() && arg.get_short() == Some(short))
}

fn visible_subcommand<'a>(command: &'a Command, name: &str) -> Option<&'a Command> {
    command
        .get_subcommands()
        .find(|command| !command.is_hide_set() && command.get_name() == name)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use super::*;
    use crate::cli::CompleteKind;

    fn complete(words: &[&str], current: &str, paths: &ManagerPaths) -> Vec<Candidate> {
        let args = CompleteArgs {
            kind: CompleteKind::Words,
            manager_dir: Some(paths.manager_dir.clone()),
            shell: Some(CompletionShell::Bash),
            cursor: Some(words.len()),
            current: Some(current.to_string()),
            words: words.iter().map(|word| (*word).to_string()).collect(),
        };
        word_candidates(&args).unwrap()
    }

    #[test]
    fn root_completion_comes_from_clap_commands() {
        let paths = temp_paths("root");

        let candidates = complete(&["cx"], "st", &paths);

        assert!(candidates.iter().any(|candidate| {
            candidate.value == "status"
                && candidate
                    .description
                    .starts_with("Query every slot and show current availability")
        }));

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn management_options_come_from_clap_command() {
        let paths = temp_paths("options");

        let candidates = complete(&["cx", "remove"], "--d", &paths);

        assert_eq!(
            candidates,
            vec![Candidate {
                value: "--delete-files".to_string(),
                description: "Also delete the slot directory and its auth files".to_string(),
            }]
        );

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn value_enum_options_complete_possible_values() {
        let paths = temp_paths("sort-values");

        let candidates = complete(&["cx", "status", "--sort"], "r", &paths);

        assert_eq!(
            candidates,
            vec![Candidate {
                value: "rotation".to_string(),
                description: "Preserve rotation.txt or explicit argument order".to_string(),
            }]
        );

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn login_positional_completes_slot_names_from_schema_arg_id() {
        let paths = temp_paths("login-slot");
        fs::create_dir_all(paths.slot_home("dia1")).unwrap();

        let candidates = complete(&["cx", "login"], "d", &paths);

        assert_eq!(
            candidates,
            vec![Candidate {
                value: "dia1".to_string(),
                description: "slot directory".to_string(),
            }]
        );

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn repeated_slot_positionals_complete_slot_names() {
        let paths = temp_paths("query-slots");
        fs::create_dir_all(paths.slot_home("bus1")).unwrap();
        fs::create_dir_all(paths.slot_home("dia1")).unwrap();

        let candidates = complete(&["cx", "status", "bus1"], "d", &paths);

        assert_eq!(
            candidates,
            vec![Candidate {
                value: "dia1".to_string(),
                description: "slot directory".to_string(),
            }]
        );

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn nested_slot_positionals_complete_slot_names() {
        let paths = temp_paths("nested-slots");
        fs::create_dir_all(paths.slot_home("bus1")).unwrap();

        let transfer = complete(
            &["cx", "transfer", "export", "--out", "/tmp/bundle"],
            "b",
            &paths,
        );
        let target = complete(&["cx", "target", "add", "work"], "b", &paths);

        assert_eq!(
            transfer,
            vec![Candidate {
                value: "bus1".to_string(),
                description: "slot directory".to_string(),
            }]
        );
        assert_eq!(target, transfer);

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn slot_options_complete_slot_names_in_launcher_and_management_commands() {
        let paths = temp_paths("slot-option");
        fs::create_dir_all(paths.slot_home("dia1")).unwrap();

        let launcher = complete(&["cx", "--slot"], "d", &paths);
        let desktop = complete(&["cx", "desktop", "--slot"], "d", &paths);

        assert_eq!(
            launcher,
            vec![Candidate {
                value: "dia1".to_string(),
                description: "slot directory".to_string(),
            }]
        );
        assert_eq!(desktop, launcher);

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn target_positionals_complete_target_names() {
        let paths = temp_paths("target-names");
        fs::create_dir_all(&paths.targets_dir).unwrap();
        fs::write(paths.target_file("research"), "").unwrap();

        let candidates = complete(&["cx", "target", "show"], "re", &paths);

        assert_eq!(
            candidates,
            vec![Candidate {
                value: "research".to_string(),
                description: "target config".to_string(),
            }]
        );

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn completion_stops_at_codex_arg_boundary() {
        let paths = temp_paths("codex-boundary");

        assert!(complete(&["cx", "--"], "-", &paths).is_empty());

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
