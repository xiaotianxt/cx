use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use clap::Arg;
use clap::Command;
use clap::ValueHint;

use crate::paths::ManagerPaths;
use crate::slot;
use crate::target;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Candidate {
    pub(super) value: String,
    pub(super) description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DynamicKind {
    Slots,
    Targets,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PathKind {
    Files,
    Directories,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ValueCompletion {
    Dynamic(DynamicKind),
    Path(PathKind),
}

pub(super) fn print_plain_values(candidates: Vec<Candidate>) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for candidate in candidates {
        writeln!(out, "{}", sanitize(&candidate.value)).context("write completion candidate")?;
    }
    Ok(())
}

pub(super) fn print_described_candidates(candidates: Vec<Candidate>) -> Result<()> {
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

pub(super) fn completion_for_arg(arg: &Arg) -> Option<ValueCompletion> {
    match arg.get_id().as_str() {
        "slot" | "slots" => Some(ValueCompletion::Dynamic(DynamicKind::Slots)),
        "target" => Some(ValueCompletion::Dynamic(DynamicKind::Targets)),
        _ => value_hint_completion(arg.get_value_hint()),
    }
}

pub(super) fn value_candidates(
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

pub(super) fn command_candidates(command: &Command) -> Vec<Candidate> {
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

pub(super) fn option_candidates(command: &Command) -> Vec<Candidate> {
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

pub(super) fn possible_value_candidates(arg: &Arg) -> Vec<Candidate> {
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

pub(super) fn filter_candidates(candidates: Vec<Candidate>, prefix: &str) -> Vec<Candidate> {
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

pub(super) fn slot_candidates(paths: &ManagerPaths) -> Result<Vec<Candidate>> {
    let rotation = slot::load_rotation(paths).unwrap_or_default();
    let rotation_set = rotation.iter().cloned().collect::<HashSet<_>>();
    let mut candidates = BTreeMap::<String, String>::new();

    for slot in rotation {
        if slot::validate_slot_name(&slot).is_err() {
            continue;
        }
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
            if slot::validate_slot_name(&slot).is_err() {
                continue;
            }
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

pub(super) fn target_candidates(paths: &ManagerPaths) -> Result<Vec<Candidate>> {
    Ok(target::list_targets(paths)?
        .into_iter()
        .map(|value| Candidate {
            value,
            description: "target config".to_string(),
        })
        .collect())
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

    #[test]
    fn slot_candidates_describe_rotation_and_extra_dirs() {
        let paths = temp_paths("slots");
        fs::create_dir_all(paths.slot_home("bus1")).unwrap();
        fs::create_dir_all(paths.slot_home("dia1")).unwrap();
        fs::create_dir_all(paths.slot_home("bad slot")).unwrap();
        fs::write(&paths.rotation_file, "bus1\nmissing\nbad slot\n").unwrap();

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
        assert!(!candidates
            .iter()
            .any(|candidate| candidate.value == "bad slot"));

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
