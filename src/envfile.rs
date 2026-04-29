use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;

pub fn read_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    let mut vars = BTreeMap::new();
    if !path.exists() {
        return Ok(vars);
    }

    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, raw_value)) = line.split_once('=') else {
            continue;
        };
        if is_valid_env_key(key.trim()) {
            vars.insert(key.trim().to_string(), parse_value(raw_value.trim()));
        }
    }
    Ok(vars)
}

pub fn write_env_file(path: &Path, envs: &[String]) -> Result<()> {
    let mut content = String::new();
    for entry in envs {
        let (key, value) = entry
            .split_once('=')
            .with_context(|| format!("--env requires KEY=value, got {entry}"))?;
        if !is_valid_env_key(key) {
            anyhow::bail!("invalid environment variable name: {key}");
        }
        content.push_str("export ");
        content.push_str(key);
        content.push('=');
        content.push_str(&quote_double(value));
        content.push('\n');
    }
    fs::write(path, content).with_context(|| format!("write {}", path.display()))
}

fn parse_value(value: &str) -> String {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        return unescape_double(&value[1..value.len() - 1]);
    }
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return value[1..value.len() - 1].replace("'\\''", "'");
    }
    value.to_string()
}

fn quote_double(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '`' => out.push_str("\\`"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn unescape_double(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some('_' | 'A'..='Z' | 'a'..='z'))
        && chars.all(|ch| matches!(ch, '_' | 'A'..='Z' | 'a'..='z' | '0'..='9'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_env_round_trips() {
        let value = "sk-with $ dollars \"quotes\" and \\ slash";
        assert_eq!(parse_value(&quote_double(value)), value);
    }
}
