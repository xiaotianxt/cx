use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use crate::envfile;
use crate::paths::ManagerPaths;
use crate::slot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSpec {
    name: String,
    slots: Vec<String>,
    overrides: Vec<String>,
    env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetInput {
    pub name: String,
    pub slots: Vec<String>,
    pub overrides: Vec<String>,
    pub envs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TargetFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    slot: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    slots: Vec<String>,
    #[serde(default, rename = "set", skip_serializing_if = "Vec::is_empty")]
    sets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    overrides: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    env: Option<TargetEnv>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum TargetEnv {
    Map(BTreeMap<String, String>),
    Entries(Vec<String>),
}

impl TargetSpec {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn slots(&self) -> &[String] {
        &self.slots
    }

    pub fn overrides(&self) -> &[String] {
        &self.overrides
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    pub fn slots_or_rotation(&self, paths: &ManagerPaths) -> Result<Vec<String>> {
        if self.slots.is_empty() {
            slot::load_rotation(paths)
        } else {
            Ok(self.slots.clone())
        }
    }
}

impl TargetInput {
    fn into_file(self) -> Result<TargetFile> {
        validate_target_name(&self.name)?;
        let slots = normalize_slot_names(self.slots)?;
        let env = envfile::parse_env_entries(&self.envs)?;
        Ok(TargetFile {
            slot: None,
            slots,
            sets: self.overrides,
            overrides: Vec::new(),
            env: (!env.is_empty()).then_some(TargetEnv::Map(env)),
        })
    }
}

impl TargetFile {
    fn into_spec(self, name: String) -> Result<TargetSpec> {
        validate_target_name(&name)?;
        let mut slots = self.slots;
        if let Some(slot) = self.slot {
            slots.insert(0, slot);
        }
        let slots = normalize_slot_names(slots)?;

        let mut overrides = self.sets;
        overrides.extend(self.overrides);

        Ok(TargetSpec {
            name,
            slots,
            overrides: overrides
                .into_iter()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect(),
            env: self
                .env
                .map(TargetEnv::into_map)
                .transpose()?
                .unwrap_or_default(),
        })
    }
}

impl TargetEnv {
    fn into_map(self) -> Result<BTreeMap<String, String>> {
        match self {
            Self::Map(map) => {
                validate_env_map(&map)?;
                Ok(map)
            }
            Self::Entries(entries) => envfile::parse_env_entries(&entries),
        }
    }
}

pub fn load_target(paths: &ManagerPaths, name: &str) -> Result<TargetSpec> {
    validate_target_name(name)?;
    let path = paths.target_file(name);
    let content = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let file = toml::from_str::<TargetFile>(&content)
        .with_context(|| format!("parse target {}", path.display()))?;
    file.into_spec(name.to_string())
}

pub fn load_optional_target(
    paths: &ManagerPaths,
    name: Option<&str>,
) -> Result<Option<TargetSpec>> {
    name.map(|name| load_target(paths, name)).transpose()
}

pub fn list_targets(paths: &ManagerPaths) -> Result<Vec<String>> {
    if !paths.targets_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut targets = Vec::new();
    for entry in fs::read_dir(&paths.targets_dir)
        .with_context(|| format!("read {}", paths.targets_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("toml") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
            if validate_target_name(stem).is_ok() {
                targets.push(stem.to_string());
            }
        }
    }
    targets.sort();
    Ok(targets)
}

pub fn save_target(paths: &ManagerPaths, input: TargetInput) -> Result<()> {
    let name = input.name.clone();
    let file = input.into_file()?;
    fs::create_dir_all(&paths.targets_dir)
        .with_context(|| format!("create {}", paths.targets_dir.display()))?;
    let path = paths.target_file(&name);
    let content = toml::to_string_pretty(&file).context("serialize target config")?;
    fs::write(&path, content).with_context(|| format!("write {}", path.display()))
}

pub fn remove_target(paths: &ManagerPaths, name: &str) -> Result<bool> {
    validate_target_name(name)?;
    let path = paths.target_file(name);
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

fn validate_target_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        anyhow::bail!("invalid target name: {name}");
    }
    Ok(())
}

fn normalize_slot_names(slots: Vec<String>) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();
    for slot in slots {
        if slot.is_empty()
            || !slot
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        {
            anyhow::bail!("invalid slot name in target: {slot}");
        }
        if seen.insert(slot.clone()) {
            normalized.push(slot);
        }
    }
    Ok(normalized)
}

fn validate_env_map(map: &BTreeMap<String, String>) -> Result<()> {
    for key in map.keys() {
        if !envfile::is_valid_env_key(key) {
            anyhow::bail!("invalid environment variable name: {key}");
        }
    }
    Ok(())
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
            "cx-target-test-{name}-{}-{unique}",
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
    fn target_file_supports_slots_overrides_and_env_table() {
        let paths = temp_paths("parse");
        fs::create_dir_all(&paths.targets_dir).unwrap();
        fs::write(
            paths.target_file("work"),
            r#"
slots = ["bus1", "bus2"]
set = ['model="gpt-5.4"']

[env]
OPENAI_BASE_URL = "https://example.test"
"#,
        )
        .unwrap();

        let target = load_target(&paths, "work").unwrap();

        assert_eq!(target.slots(), ["bus1", "bus2"]);
        assert_eq!(target.overrides(), [r#"model="gpt-5.4""#]);
        assert_eq!(
            target.env().get("OPENAI_BASE_URL").map(String::as_str),
            Some("https://example.test")
        );

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn target_rejects_invalid_env_map_key() {
        let paths = temp_paths("bad-env");
        fs::create_dir_all(&paths.targets_dir).unwrap();
        fs::write(
            paths.target_file("work"),
            r#"
[env]
"BAD=KEY" = "value"
"#,
        )
        .unwrap();

        let err = load_target(&paths, "work").unwrap_err();

        assert!(format!("{err:#}").contains("invalid environment variable name"));

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn target_deduplicates_slots_preserving_first_occurrence() {
        let paths = temp_paths("dedupe");
        fs::create_dir_all(&paths.targets_dir).unwrap();
        fs::write(
            paths.target_file("work"),
            r#"
slot = "bus1"
slots = ["bus2", "bus1", "bus2"]
"#,
        )
        .unwrap();

        let target = load_target(&paths, "work").unwrap();

        assert_eq!(target.slots(), ["bus1", "bus2"]);

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn target_list_ignores_invalid_file_names() {
        let paths = temp_paths("list");
        fs::create_dir_all(&paths.targets_dir).unwrap();
        fs::write(paths.target_file("work"), "").unwrap();
        fs::write(paths.targets_dir.join("bad name.toml"), "").unwrap();

        let targets = list_targets(&paths).unwrap();

        assert_eq!(targets, ["work"]);

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn target_without_slots_falls_back_to_rotation() {
        let paths = temp_paths("rotation");
        fs::create_dir_all(&paths.targets_dir).unwrap();
        fs::write(
            paths.target_file("default-model"),
            r#"set = ['model="gpt-5.4"']"#,
        )
        .unwrap();
        fs::create_dir_all(&paths.manager_dir).unwrap();
        fs::write(&paths.rotation_file, "bus1\nbus2\n").unwrap();

        let target = load_target(&paths, "default-model").unwrap();

        assert_eq!(target.slots_or_rotation(&paths).unwrap(), ["bus1", "bus2"]);

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }

    #[test]
    fn save_target_round_trips() {
        let paths = temp_paths("save");
        save_target(
            &paths,
            TargetInput {
                name: "research".to_string(),
                slots: vec!["bus3".to_string()],
                overrides: vec![r#"model="gpt-5.5""#.to_string()],
                envs: vec!["FOO=bar".to_string()],
            },
        )
        .unwrap();

        let target = load_target(&paths, "research").unwrap();

        assert_eq!(target.slots(), ["bus3"]);
        assert_eq!(target.overrides(), [r#"model="gpt-5.5""#]);
        assert_eq!(target.env().get("FOO").map(String::as_str), Some("bar"));

        let _ = fs::remove_dir_all(&paths.manager_dir);
    }
}
