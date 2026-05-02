use std::fs;

use anyhow::Context;
use anyhow::Result;

use crate::paths::ManagerPaths;

pub fn load_rotation(paths: &ManagerPaths) -> Result<Vec<String>> {
    if !paths.rotation_file.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&paths.rotation_file)
        .with_context(|| format!("read {}", paths.rotation_file.display()))?;
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect())
}

pub(super) fn append_rotation(paths: &ManagerPaths, slot: &str) -> Result<()> {
    fs::create_dir_all(&paths.manager_dir)
        .with_context(|| format!("create {}", paths.manager_dir.display()))?;
    let mut slots = load_rotation(paths)?;
    if !slots.iter().any(|existing| existing == slot) {
        slots.push(slot.to_string());
        fs::write(&paths.rotation_file, slots.join("\n") + "\n")
            .with_context(|| format!("write {}", paths.rotation_file.display()))?;
    }
    Ok(())
}

pub(super) fn remove_from_rotation(paths: &ManagerPaths, slot: &str) -> Result<bool> {
    if !paths.rotation_file.exists() {
        return Ok(false);
    }

    let slots = load_rotation(paths)?;
    let filtered = slots
        .iter()
        .filter(|existing| existing.as_str() != slot)
        .cloned()
        .collect::<Vec<_>>();
    if filtered.len() == slots.len() {
        return Ok(false);
    }

    let content = if filtered.is_empty() {
        String::new()
    } else {
        filtered.join("\n") + "\n"
    };
    fs::write(&paths.rotation_file, content)
        .with_context(|| format!("write {}", paths.rotation_file.display()))?;
    Ok(true)
}
