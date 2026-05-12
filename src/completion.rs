use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use clap::Arg;
use clap::Command;
use clap::CommandFactory;
use clap::ValueHint;
use serde_json::Value;

use crate::cli::Cli;
use crate::cli::CompleteArgs;
use crate::cli::CompleteKind;
use crate::cli::CompletionShell;
use crate::paths::ManagerPaths;
use crate::run;
use crate::slot;
use crate::target;

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    value: String,
    description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DynamicKind {
    Slots,
    Targets,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    Files,
    Directories,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueCompletion {
    Dynamic(DynamicKind),
    Path(PathKind),
}

pub fn print_script(shell: CompletionShell) -> Result<()> {
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

pub fn print_candidates(args: CompleteArgs) -> Result<()> {
    let candidates = match args.kind {
        CompleteKind::Slots => {
            let paths = ManagerPaths::new(args.manager_dir)?;
            slot_candidates(&paths)?
        }
        CompleteKind::Targets => {
            let paths = ManagerPaths::new(args.manager_dir)?;
            target_candidates(&paths)?
        }
        CompleteKind::Models => {
            let paths = ManagerPaths::new(args.manager_dir)?;
            model_candidates(&paths.base_codex_home.join("models_cache.json"))?
        }
        CompleteKind::Words => word_candidates(&args)?,
    };
    print_candidate_lines(candidates)
}

fn print_candidate_lines(candidates: Vec<Candidate>) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for candidate in candidates {
        writeln!(
            out,
            "{}\t{}",
            sanitize(&candidate.value),
            sanitize(&candidate.description)
        )
        .context("write completion candidate")?;
    }
    Ok(())
}

fn word_candidates(args: &CompleteArgs) -> Result<Vec<Candidate>> {
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
        launcher_candidates(current)?
    };
    Ok(filter_candidates(candidates, current))
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

fn launcher_candidates(current: &str) -> Result<Vec<Candidate>> {
    if current.starts_with('-') {
        return Ok(option_candidates(&run::launcher_command()));
    }
    let root = Cli::command();
    Ok(command_candidates(&root))
}

fn management_candidates(
    root: &Command,
    current: &str,
    user_words: &[String],
    paths: &ManagerPaths,
) -> Result<Vec<Candidate>> {
    let context = command_context(root, user_words);
    if current.starts_with('-') {
        return Ok(option_candidates(context.command));
    }
    if context
        .command
        .get_subcommands()
        .any(|command| !command.is_hide_set())
    {
        return Ok(command_candidates(context.command));
    }
    positional_candidates(&context, paths)
}

#[derive(Debug)]
struct CompletionContext<'a> {
    command: &'a Command,
    path: Vec<String>,
    positionals: Vec<String>,
}

fn command_context<'a>(root: &'a Command, user_words: &[String]) -> CompletionContext<'a> {
    let mut command = root;
    let mut path = Vec::new();
    let mut positionals = Vec::new();
    let mut expecting_value = false;

    for word in user_words {
        if expecting_value {
            expecting_value = false;
            continue;
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
            path.push(word.clone());
            positionals.clear();
            continue;
        }
        positionals.push(word.clone());
    }

    CompletionContext {
        command,
        path,
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

    if is_management_mode(user_words) {
        let root = Cli::command();
        let context = command_context(&root, user_words);
        let Some(arg) = context
            .command
            .get_arguments()
            .find(|arg| !arg.is_hide_set() && arg.get_long() == Some(option_name))
        else {
            return Ok(None);
        };
        let Some(candidates) = value_candidates_for_arg(arg, value_prefix, paths)? else {
            return Ok(None);
        };
        let prefix = format!("--{option_name}=");
        let candidates = candidates
            .into_iter()
            .map(|candidate| Candidate {
                value: format!("{prefix}{}", candidate.value),
                description: candidate.description,
            })
            .collect();
        Ok(Some(candidates))
    } else {
        let launcher = run::launcher_command();
        let Some(arg) = launcher
            .get_arguments()
            .find(|arg| !arg.is_hide_set() && arg.get_long() == Some(option_name))
        else {
            return Ok(None);
        };
        let Some(candidates) = value_candidates_for_arg(arg, value_prefix, paths)? else {
            return Ok(None);
        };
        let prefix = format!("--{option_name}=");
        let candidates = candidates
            .into_iter()
            .map(|candidate| Candidate {
                value: format!("{prefix}{}", candidate.value),
                description: candidate.description,
            })
            .collect();
        Ok(Some(candidates))
    }
}

fn complete_value_after_previous(
    current: &str,
    user_words: &[String],
    paths: &ManagerPaths,
) -> Result<Option<Vec<Candidate>>> {
    let Some(previous) = user_words.last() else {
        return Ok(None);
    };

    if is_management_mode(user_words) {
        let root = Cli::command();
        let context = command_context(&root, &user_words[..user_words.len() - 1]);
        option_from_token(context.command, previous)
            .map(|arg| value_candidates_for_arg(arg, current, paths))
            .transpose()
            .map(Option::flatten)
    } else {
        let launcher = run::launcher_command();
        option_from_token(&launcher, previous)
            .map(|arg| value_candidates_for_arg(arg, current, paths))
            .transpose()
            .map(Option::flatten)
    }
}

fn positional_candidates(
    context: &CompletionContext<'_>,
    paths: &ManagerPaths,
) -> Result<Vec<Candidate>> {
    let index = context.positionals.len();
    if let Some(kind) = positional_dynamic_kind(&context.path, index) {
        return dynamic_candidates(kind, paths);
    }
    if let Some(arg) = positional_arg(context.command, index) {
        let values = possible_value_candidates(arg);
        if !values.is_empty() {
            return Ok(values);
        }
    }
    Ok(Vec::new())
}

fn positional_dynamic_kind(path: &[String], index: usize) -> Option<DynamicKind> {
    match path {
        [command] if matches!(command.as_str(), "status" | "select" | "stats") => {
            Some(DynamicKind::Slots)
        }
        [command] if matches!(command.as_str(), "add" | "remove" | "login") && index == 0 => {
            Some(DynamicKind::Slots)
        }
        [target, command]
            if target == "target" && matches!(command.as_str(), "show" | "remove") =>
        {
            (index == 0).then_some(DynamicKind::Targets)
        }
        [target, command] if target == "target" && command == "add" => {
            if index == 0 {
                Some(DynamicKind::Targets)
            } else {
                Some(DynamicKind::Slots)
            }
        }
        _ => None,
    }
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

fn value_candidates(
    completion: ValueCompletion,
    prefix: &str,
    paths: &ManagerPaths,
) -> Result<Vec<Candidate>> {
    let candidates = match completion {
        ValueCompletion::Dynamic(kind) => dynamic_candidates(kind, paths)?,
        ValueCompletion::Path(kind) => path_candidates(prefix, kind)?,
    };
    Ok(filter_candidates(candidates, prefix))
}

fn value_candidates_for_arg(
    arg: &Arg,
    prefix: &str,
    paths: &ManagerPaths,
) -> Result<Option<Vec<Candidate>>> {
    if !option_takes_value(arg) {
        return Ok(None);
    }
    if let Some(completion) = value_hint_completion(arg.get_value_hint()) {
        return value_candidates(completion, prefix, paths).map(Some);
    }
    if let Some(long) = arg.get_long() {
        let completion = match long {
            "slot" => Some(ValueCompletion::Dynamic(DynamicKind::Slots)),
            "target" => Some(ValueCompletion::Dynamic(DynamicKind::Targets)),
            _ => None,
        };
        if let Some(completion) = completion {
            return value_candidates(completion, prefix, paths).map(Some);
        }
    }
    let possible_values = possible_value_candidates(arg);
    if possible_values.is_empty() {
        Ok(None)
    } else {
        Ok(Some(filter_candidates(possible_values, prefix)))
    }
}

fn value_hint_completion(value_hint: ValueHint) -> Option<ValueCompletion> {
    match value_hint {
        ValueHint::FilePath => Some(ValueCompletion::Path(PathKind::Files)),
        ValueHint::DirPath => Some(ValueCompletion::Path(PathKind::Directories)),
        _ => None,
    }
}

fn dynamic_candidates(kind: DynamicKind, paths: &ManagerPaths) -> Result<Vec<Candidate>> {
    match kind {
        DynamicKind::Slots => slot_candidates(paths),
        DynamicKind::Targets => target_candidates(paths),
    }
}

fn command_candidates(command: &Command) -> Vec<Candidate> {
    command
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .map(|command| Candidate {
            value: command.get_name().to_string(),
            description: command
                .get_about()
                .map(ToString::to_string)
                .unwrap_or_default(),
        })
        .collect()
}

fn option_candidates(command: &Command) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for arg in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
        let description = arg.get_help().map(ToString::to_string).unwrap_or_default();
        if let Some(long) = arg.get_long() {
            candidates.push(Candidate {
                value: format!("--{long}"),
                description: description.clone(),
            });
        }
        if let Some(short) = arg.get_short() {
            candidates.push(Candidate {
                value: format!("-{short}"),
                description: description.clone(),
            });
        }
    }
    if !candidates
        .iter()
        .any(|candidate| candidate.value == "--help")
    {
        candidates.push(Candidate {
            value: "--help".to_string(),
            description: "Print help".to_string(),
        });
        candidates.push(Candidate {
            value: "-h".to_string(),
            description: "Print help".to_string(),
        });
    }
    candidates
}

fn possible_value_candidates(arg: &Arg) -> Vec<Candidate> {
    arg.get_possible_values()
        .into_iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| Candidate {
            value: value.get_name().to_string(),
            description: value
                .get_help()
                .map(ToString::to_string)
                .unwrap_or_default(),
        })
        .collect()
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

fn filter_candidates(candidates: Vec<Candidate>, prefix: &str) -> Vec<Candidate> {
    let mut unique = BTreeMap::<String, String>::new();
    for candidate in candidates {
        if candidate.value.starts_with(prefix) {
            unique
                .entry(candidate.value)
                .or_insert(candidate.description);
        }
    }
    unique
        .into_iter()
        .map(|(value, description)| Candidate { value, description })
        .collect()
}

fn path_candidates(prefix: &str, kind: PathKind) -> Result<Vec<Candidate>> {
    let (dir_prefix, name_prefix) = split_path_prefix(prefix);
    let read_dir = expand_tilde(if dir_prefix.is_empty() {
        "."
    } else {
        dir_prefix
    })?;
    let Ok(entries) = fs::read_dir(&read_dir) else {
        return Ok(Vec::new());
    };

    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        if !name_prefix.starts_with('.') && file_name.starts_with('.') {
            continue;
        }
        if !file_name.starts_with(name_prefix) {
            continue;
        }
        let file_type = entry.file_type()?;
        let is_dir = file_type.is_dir();
        if kind == PathKind::Directories && !is_dir {
            continue;
        }
        let mut value = format!("{dir_prefix}{file_name}");
        if is_dir {
            value.push('/');
        }
        candidates.push(Candidate {
            value,
            description: if is_dir { "directory" } else { "file" }.to_string(),
        });
    }
    Ok(candidates)
}

fn split_path_prefix(prefix: &str) -> (&str, &str) {
    prefix
        .rfind('/')
        .map(|index| prefix.split_at(index + 1))
        .unwrap_or(("", prefix))
}

fn expand_tilde(path: &str) -> Result<PathBuf> {
    if path == "~" {
        return crate::paths::home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(crate::paths::home_dir()?.join(rest));
    }
    Ok(PathBuf::from(path))
}

fn slot_candidates(paths: &ManagerPaths) -> Result<Vec<Candidate>> {
    let rotation = slot::load_rotation(paths).unwrap_or_default();
    let rotation_set = rotation.iter().cloned().collect::<HashSet<_>>();
    let mut candidates = BTreeMap::<String, String>::new();

    for slot in rotation {
        let description = if paths.slot_home(&slot).is_dir() {
            "in rotation"
        } else {
            "in rotation, missing home"
        };
        candidates.insert(slot, description.to_string());
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
            candidates.entry(slot.clone()).or_insert_with(|| {
                if rotation_set.contains(&slot) {
                    "in rotation".to_string()
                } else {
                    "slot directory".to_string()
                }
            });
        }
    }

    Ok(candidates
        .into_iter()
        .map(|(value, description)| Candidate { value, description })
        .collect())
}

fn target_candidates(paths: &ManagerPaths) -> Result<Vec<Candidate>> {
    Ok(target::list_targets(paths)?
        .into_iter()
        .map(|value| Candidate {
            value,
            description: "target config".to_string(),
        })
        .collect())
}

fn model_candidates(path: &Path) -> Result<Vec<Candidate>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value = serde_json::from_str::<Value>(&content)
        .with_context(|| format!("parse {}", path.display()))?;
    let Some(models) = value.get("models").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut candidates = models
        .iter()
        .filter_map(|model| {
            let slug = model.get("slug").and_then(Value::as_str)?.to_string();
            let display = model
                .get("display_name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("model")
                .to_string();
            Some(Candidate {
                value: slug,
                description: display,
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.value.cmp(&right.value));
    candidates.dedup_by(|left, right| left.value == right.value);
    Ok(candidates)
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '\t' | '\n' | '\r' => ' ',
            _ => ch,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use super::*;

    fn temp_paths(name: &str) -> ManagerPaths {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cx-completion-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths {
            base_codex_home: root.join("codex"),
            manager_dir: root.join("profile-manager"),
            slots_dir: root.join("profile-manager/slots"),
            targets_dir: root.join("profile-manager/targets"),
            rotation_file: root.join("profile-manager/rotation.txt"),
        }
    }

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
    fn generated_shell_scripts_are_protocol_shims() {
        for script in [FISH_COMPLETIONS, ZSH_COMPLETIONS, BASH_COMPLETIONS] {
            assert!(script.contains("__complete words"));
            assert!(!script.contains("--delete-files"));
            assert!(!script.contains("--from-current"));
            assert!(!script.contains("status stats"));
            assert!(!script.contains("target show"));
        }
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

    #[cfg(feature = "service")]
    #[test]
    fn launcher_options_include_experimental_service_remote() {
        let paths = temp_paths("service-remote-option");

        let candidates = complete(&["cx"], "--cx-s", &paths);

        assert_eq!(
            candidates,
            vec![Candidate {
                value: "--cx-service-remote".to_string(),
                description: "Use experimental cx service remote".to_string(),
            }]
        );

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[cfg(not(feature = "service"))]
    #[test]
    fn launcher_options_omit_service_remote_without_feature() {
        let paths = temp_paths("no-service-remote-option");

        let candidates = complete(&["cx"], "--cx-s", &paths);

        assert!(candidates.is_empty());

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn clap_value_enum_options_complete_possible_values() {
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
    fn clap_command_carries_path_completion_metadata() {
        let root = Cli::command();
        let status = visible_subcommand(&root, "status").unwrap();
        let manager_dir = long_arg_from_word(status, "--manager-dir").unwrap();

        assert_eq!(manager_dir.get_value_hint(), ValueHint::DirPath);
    }

    #[test]
    fn target_show_completes_target_names() {
        let paths = temp_paths("targets");
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
    fn launcher_slot_option_completes_slot_names() {
        let paths = temp_paths("slot-option");
        fs::create_dir_all(paths.slot_home("dia1")).unwrap();

        assert_eq!(
            complete(&["cx", "--slot"], "d", &paths),
            vec![Candidate {
                value: "dia1".to_string(),
                description: "slot directory".to_string(),
            }]
        );

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn launcher_completion_stops_at_codex_arg_boundary() {
        let paths = temp_paths("codex-boundary");

        assert!(complete(&["cx", "--"], "-", &paths).is_empty());

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn slot_candidates_describe_rotation_and_extra_dirs() {
        let paths = temp_paths("slots");
        fs::create_dir_all(paths.slot_home("bus1")).unwrap();
        fs::create_dir_all(paths.slot_home("dia1")).unwrap();
        fs::write(&paths.rotation_file, "bus1\nmissing\n").unwrap();

        let candidates = slot_candidates(&paths).unwrap();

        assert!(candidates.contains(&Candidate {
            value: "bus1".to_string(),
            description: "in rotation".to_string(),
        }));
        assert!(candidates.contains(&Candidate {
            value: "dia1".to_string(),
            description: "slot directory".to_string(),
        }));
        assert!(candidates.contains(&Candidate {
            value: "missing".to_string(),
            description: "in rotation, missing home".to_string(),
        }));

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn model_candidates_read_models_cache() {
        let paths = temp_paths("models");
        fs::create_dir_all(&paths.base_codex_home).unwrap();
        let cache = paths.base_codex_home.join("models_cache.json");
        fs::write(
            &cache,
            r#"{"models":[{"slug":"gpt-test","display_name":"GPT Test"}]}"#,
        )
        .unwrap();

        assert_eq!(
            model_candidates(&cache).unwrap(),
            vec![Candidate {
                value: "gpt-test".to_string(),
                description: "GPT Test".to_string(),
            }]
        );

        let _ = fs::remove_dir_all(&paths.base_codex_home);
    }
}
