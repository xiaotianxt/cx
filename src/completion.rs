use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use serde_json::Value;

use crate::cli::CompleteKind;
use crate::cli::CompletionShell;
use crate::paths::ManagerPaths;
use crate::slot;
use crate::target;

const FISH_COMPLETIONS: &str = r#"# cx fish completions
function __cx_complete_slots
    cx __complete slots 2>/dev/null
end

function __cx_complete_models
    cx __complete models 2>/dev/null
end

function __cx_complete_targets
    cx __complete targets 2>/dev/null
end

function __cx_no_subcommand
    not __fish_seen_subcommand_from status stats select add remove login target doctor install completions help
end

complete -c cx -f
complete -c cx -n "__cx_no_subcommand" -a status -d "Query slot availability"
complete -c cx -n "__cx_no_subcommand" -a stats -d "Show local token usage"
complete -c cx -n "__cx_no_subcommand" -a select -d "Print the best slot"
complete -c cx -n "__cx_no_subcommand" -a add -d "Create or update a slot"
complete -c cx -n "__cx_no_subcommand" -a remove -d "Remove a slot from rotation"
complete -c cx -n "__cx_no_subcommand" -a login -d "Log into a slot"
complete -c cx -n "__cx_no_subcommand" -a target -d "Manage target configs"
complete -c cx -n "__cx_no_subcommand" -a doctor -d "Validate local layout"
complete -c cx -n "__cx_no_subcommand" -a install -d "Install cx locally"
complete -c cx -n "__cx_no_subcommand" -a completions -d "Generate shell completions"

complete -c cx -s s -l slot -r -a "(__cx_complete_slots)" -d "Use a specific slot"
complete -c cx -l target -r -a "(__cx_complete_targets)" -d "Use a target config"
complete -c cx -l manager-dir -r -d "Profile-manager directory"
complete -c cx -l codex-bin -r -d "Path to the real Codex binary"
complete -c cx -l cx-quiet -d "Suppress cx slot banner"
complete -c cx -l cx-debug -d "Print slot selection details"
complete -c cx -s m -r -a "(__cx_complete_models)" -d "Codex model"

complete -c cx -n "__fish_seen_subcommand_from status" -l sort -r -a "score rotation" -d "Sort status output"
complete -c cx -n "__fish_seen_subcommand_from status select stats" -l target -r -a "(__cx_complete_targets)" -d "Use a target config"
complete -c cx -n "__fish_seen_subcommand_from status select stats login remove" -a "(__cx_complete_slots)"
complete -c cx -n "__fish_seen_subcommand_from stats" -l by-slot -d "Break down usage by slot"
complete -c cx -n "__fish_seen_subcommand_from stats" -l price -d "Include price estimates"
complete -c cx -n "__fish_seen_subcommand_from stats" -l no-price -d "Skip price estimates"
complete -c cx -n "__fish_seen_subcommand_from stats" -l refresh-prices -d "Refresh cached pricing"
complete -c cx -n "__fish_seen_subcommand_from stats" -l price-url -r -d "Pricing page URL"
complete -c cx -n "__fish_seen_subcommand_from stats" -l json -d "Print JSON"
complete -c cx -n "__fish_seen_subcommand_from stats" -l calibrate -d "Calibrate token mix"
complete -c cx -n "__fish_seen_subcommand_from target" -a "list show add remove"
complete -c cx -n "__fish_seen_subcommand_from target" -l json -d "Print JSON"
complete -c cx -n "__fish_seen_subcommand_from target" -l set -r -d "Target Codex config override"
complete -c cx -n "__fish_seen_subcommand_from target" -l env -r -d "Target environment variable"
complete -c cx -n "__fish_seen_subcommand_from completions" -a "fish zsh bash"
"#;

const ZSH_COMPLETIONS: &str = r#"#compdef cx

_cx_slots() {
  local -a candidates
  local value description
  while IFS=$'\t' read -r value description; do
    [[ -n "$value" ]] && candidates+=("${value}:${description}")
  done < <(cx __complete slots 2>/dev/null)
  _describe 'slots' candidates
}

_cx_models() {
  local -a candidates
  local value description
  while IFS=$'\t' read -r value description; do
    [[ -n "$value" ]] && candidates+=("${value}:${description}")
  done < <(cx __complete models 2>/dev/null)
  _describe 'models' candidates
}

_cx_targets() {
  local -a candidates
  local value description
  while IFS=$'\t' read -r value description; do
    [[ -n "$value" ]] && candidates+=("${value}:${description}")
  done < <(cx __complete targets 2>/dev/null)
  _describe 'targets' candidates
}

_cx() {
  local curcontext="$curcontext" state
  typeset -A opt_args
  local -a commands
  commands=(
    'status:query slot availability'
    'stats:show local token usage'
    'select:print the best slot'
    'add:create or update a slot'
    'remove:remove a slot from rotation'
    'login:log into a slot'
    'target:manage target configs'
    'doctor:validate local layout'
    'install:install cx locally'
    'completions:generate shell completions'
  )

  _arguments -C \
    '(-h --help)'{-h,--help}'[Print help]' \
    '(-s --slot)'{-s,--slot}'[Use a specific slot]:slot:_cx_slots' \
    '--target[Use a target config]:target:_cx_targets' \
    '--manager-dir[Profile-manager directory]:directory:_files -/' \
    '--codex-bin[Path to the real Codex binary]:file:_files' \
    '--cx-quiet[Suppress cx slot banner]' \
    '--cx-debug[Print slot selection details]' \
    '-m[Codex model]:model:_cx_models' \
    '1:command:->command' \
    '*::arg:->arg'

  case "$state" in
    command)
      _describe -t commands 'cx command' commands
      ;;
    arg)
      case "${words[1]}" in
        status)
          if [[ "${words[CURRENT-1]}" == "--sort" ]]; then
            _values 'sort' score rotation
          elif [[ "${words[CURRENT-1]}" == "--target" ]]; then
            _cx_targets
          else
            _cx_slots
          fi
          ;;
        stats)
          if [[ "${words[CURRENT]}" == -* ]]; then
            _values 'stats options' \
              '--by-slot[Break down usage by slot]' \
              '--price[Include price estimates]' \
              '--no-price[Skip price estimates]' \
              '--refresh-prices[Refresh cached pricing]' \
              '--price-url[Pricing page URL]' \
              '--json[Print JSON]' \
              '--calibrate[Calibrate token mix]' \
              '--target[Use a target config]' \
              '--manager-dir[Profile-manager directory]'
          else
            _cx_slots
          fi
          ;;
        target)
          _values 'target command' list show add remove
          ;;
        select|login|remove)
          _cx_slots
          ;;
        completions)
          _values 'shell' fish zsh bash
          ;;
        *)
          _normal
          ;;
      esac
      ;;
  esac
}

_cx "$@"
"#;

const BASH_COMPLETIONS: &str = r#"# cx bash completions
__cx_complete_words() {
  local kind="$1"
  cx __complete "$kind" 2>/dev/null | while IFS=$'\t' read -r value _description; do
    [[ -n "$value" ]] && printf '%s\n' "$value"
  done
}

_cx() {
  local cur prev subcommand word
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"

  for word in "${COMP_WORDS[@]:1:$COMP_CWORD-1}"; do
    case "$word" in
      -*)
        ;;
      *)
        subcommand="$word"
        break
        ;;
    esac
  done

  case "$prev" in
    --sort)
      if [[ "$subcommand" == "status" ]]; then
        mapfile -t COMPREPLY < <(compgen -W "score rotation" -- "$cur")
        return 0
      fi
      ;;
    --slot|-s)
      mapfile -t COMPREPLY < <(compgen -W "$(__cx_complete_words slots)" -- "$cur")
      return 0
      ;;
    --target)
      mapfile -t COMPREPLY < <(compgen -W "$(__cx_complete_words targets)" -- "$cur")
      return 0
      ;;
    -m)
      mapfile -t COMPREPLY < <(compgen -W "$(__cx_complete_words models)" -- "$cur")
      return 0
      ;;
    completions)
      mapfile -t COMPREPLY < <(compgen -W "fish zsh bash" -- "$cur")
      return 0
      ;;
  esac

  if [[ -z "$subcommand" ]]; then
    if [[ "$cur" == -* ]]; then
      mapfile -t COMPREPLY < <(compgen -W "--slot -s --target --manager-dir --codex-bin --cx-quiet --cx-debug -m --help -h" -- "$cur")
    else
      mapfile -t COMPREPLY < <(compgen -W "status stats select add remove login target doctor install completions" -- "$cur")
    fi
    return 0
  fi

  case "$subcommand" in
    status)
      if [[ "$cur" == -* ]]; then
        mapfile -t COMPREPLY < <(compgen -W "--sort --target --manager-dir --timeout --json --help -h" -- "$cur")
      else
        mapfile -t COMPREPLY < <(compgen -W "$(__cx_complete_words slots)" -- "$cur")
      fi
      ;;
    stats)
      if [[ "$cur" == -* ]]; then
        mapfile -t COMPREPLY < <(compgen -W "--by-slot --price --no-price --refresh-prices --price-url --target --manager-dir --json --calibrate --help -h" -- "$cur")
      else
        mapfile -t COMPREPLY < <(compgen -W "$(__cx_complete_words slots)" -- "$cur")
      fi
      ;;
    target)
      mapfile -t COMPREPLY < <(compgen -W "list show add remove --json --manager-dir --set --env --help -h" -- "$cur")
      ;;
    select|login|remove)
      mapfile -t COMPREPLY < <(compgen -W "$(__cx_complete_words slots)" -- "$cur")
      ;;
    completions)
      mapfile -t COMPREPLY < <(compgen -W "fish zsh bash" -- "$cur")
      ;;
  esac
}

complete -F _cx cx
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    value: String,
    description: String,
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

pub fn print_candidates(kind: CompleteKind, manager_dir: Option<PathBuf>) -> Result<()> {
    let paths = ManagerPaths::new(manager_dir)?;
    let candidates = match kind {
        CompleteKind::Slots => slot_candidates(&paths)?,
        CompleteKind::Targets => target_candidates(&paths)?,
        CompleteKind::Models => model_candidates(&paths.base_codex_home.join("models_cache.json"))?,
    };

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
