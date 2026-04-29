use std::fs;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use serde_json::Value;

use crate::slot;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotAuth {
    pub access_token: Option<String>,
    pub account_id: Option<String>,
    pub fedramp: bool,
    pub api_key: Option<String>,
    pub provider: Option<String>,
}

pub fn read_slot_auth(slot_dir: &Path) -> Result<SlotAuth> {
    let provider = slot::read_override_string(slot_dir, "model_provider")?;
    let auth_path = slot_dir.join("home/auth.json");
    if !auth_path.exists() {
        return Ok(SlotAuth {
            provider,
            ..SlotAuth::default()
        });
    }

    let content =
        fs::read_to_string(&auth_path).with_context(|| format!("read {}", auth_path.display()))?;
    let auth: Value =
        serde_json::from_str(&content).with_context(|| format!("parse {}", auth_path.display()))?;
    let tokens = auth.get("tokens").and_then(Value::as_object);
    let id_token = tokens
        .and_then(|tokens| tokens.get("id_token"))
        .and_then(Value::as_object);

    let access_token = tokens
        .and_then(|tokens| tokens.get("access_token"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let account_id = tokens
        .and_then(|tokens| tokens.get("account_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            id_token
                .and_then(|id_token| id_token.get("chatgpt_account_id"))
                .and_then(Value::as_str)
        })
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let fedramp = id_token
        .and_then(|id_token| id_token.get("chatgpt_account_is_fedramp"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let api_key = auth
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    Ok(SlotAuth {
        access_token,
        account_id,
        fedramp,
        api_key,
        provider,
    })
}
