use std::fs;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use toml::Value;

pub fn read_override_lines(slot_dir: &Path) -> Result<Vec<String>> {
    let path = slot_dir.join("overrides.conf");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

pub fn read_override_string(slot_dir: &Path, key: &str) -> Result<Option<String>> {
    for line in read_override_lines(slot_dir)? {
        let Ok(value) = toml::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(value) = value.get(key).and_then(Value::as_str) {
            return Ok(Some(value.to_string()));
        }
    }
    Ok(None)
}

pub fn read_config_string(path: &Path, key: &str) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value =
        toml::from_str::<Value>(&content).with_context(|| format!("parse {}", path.display()))?;
    Ok(value.get(key).and_then(Value::as_str).map(str::to_string))
}
