use std::fs;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use toml::Value;

pub(crate) const CX_PROVIDER_ID: &str = "cx";

pub(crate) fn resolve(base_config: &Path, overrides: &[String]) -> Result<Vec<String>> {
    let mut effective = read_config(base_config)?;
    for (index, override_toml) in overrides.iter().enumerate() {
        let value = toml::from_str::<Value>(override_toml)
            .with_context(|| format!("parse Codex override #{}", index + 1))?;
        merge_value(&mut effective, value);
    }

    let source_provider = effective
        .get("model_provider")
        .and_then(Value::as_str)
        .unwrap_or("openai");
    let provider = if source_provider == "openai" {
        openai_alias(effective.get("openai_base_url").and_then(Value::as_str))
    } else {
        effective
            .get("model_providers")
            .and_then(|providers| providers.get(source_provider))
            .cloned()
            .with_context(|| {
                format!("model provider `{source_provider}` has no configuration to alias")
            })?
    };

    Ok(vec![
        format!("model_provider = \"{CX_PROVIDER_ID}\""),
        format!("model_providers.{CX_PROVIDER_ID} = {provider}"),
    ])
}

fn read_config(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Table(Default::default()));
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("read Codex config from {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("parse Codex config from {}", path.display()))
}

fn merge_value(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Table(target), Value::Table(source)) => {
            for (key, value) in source {
                if let Some(existing) = target.get_mut(&key) {
                    merge_value(existing, value);
                } else {
                    target.insert(key, value);
                }
            }
        }
        (target, source) => *target = source,
    }
}

fn openai_alias(base_url: Option<&str>) -> Value {
    let mut provider = toml::map::Map::new();
    provider.insert("name".to_string(), Value::String("OpenAI".to_string()));
    if let Some(base_url) = base_url.filter(|base_url| !base_url.is_empty()) {
        provider.insert("base_url".to_string(), Value::String(base_url.to_string()));
    }
    provider.insert(
        "wire_api".to_string(),
        Value::String("responses".to_string()),
    );
    provider.insert(
        "env_http_headers".to_string(),
        Value::Table(toml::map::Map::from_iter([
            (
                "OpenAI-Organization".to_string(),
                Value::String("OPENAI_ORGANIZATION".to_string()),
            ),
            (
                "OpenAI-Project".to_string(),
                Value::String("OPENAI_PROJECT".to_string()),
            ),
        ])),
    );
    provider.insert("requires_openai_auth".to_string(), Value::Boolean(true));
    provider.insert("supports_websockets".to_string(), Value::Boolean(true));
    Value::Table(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_openai_uses_first_party_auth_under_cx_alias() {
        let overrides = resolve(
            Path::new("/path/that/does/not/exist"),
            &["model = \"gpt-5.4\"".to_string()],
        )
        .unwrap();

        assert!(overrides[1].starts_with("model_providers.cx = {"));
        assert!(overrides[1].contains("name = \"OpenAI\""));
        assert!(overrides[1].contains("requires_openai_auth = true"));
        assert!(overrides[1].contains("supports_websockets = true"));
    }

    #[test]
    fn official_openai_alias_preserves_openai_base_url() {
        let overrides = resolve(
            Path::new("/path/that/does/not/exist"),
            &["openai_base_url = \"https://example.test/v1\"".to_string()],
        )
        .unwrap();

        assert!(overrides[1].contains("base_url = \"https://example.test/v1\""));
    }

    #[test]
    fn custom_provider_is_preserved_as_a_cli_compatible_inline_alias() {
        let overrides = resolve(
            Path::new("/path/that/does/not/exist"),
            &[
                "model_provider = \"pku\"".to_string(),
                "model_providers.pku = { name = \"PKU\", base_url = \"https://example.test/v1\", wire_api = \"responses\", env_key = \"PKU_API_KEY\", requires_openai_auth = false }".to_string(),
            ],
        )
        .unwrap();

        let alias = toml::from_str::<Value>(&overrides[1]).unwrap();
        let cx = alias
            .get("model_providers")
            .and_then(|providers| providers.get("cx"))
            .unwrap();
        assert_eq!(
            cx.get("base_url").and_then(Value::as_str),
            Some("https://example.test/v1")
        );
        assert_eq!(
            cx.get("env_key").and_then(Value::as_str),
            Some("PKU_API_KEY")
        );
        assert_eq!(
            cx.get("requires_openai_auth").and_then(Value::as_bool),
            Some(false)
        );
    }
}
