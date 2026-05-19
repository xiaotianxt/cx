use std::fs;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use base64::engine::general_purpose::URL_SAFE;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;

use crate::slot;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotAuth {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub fedramp: bool,
    pub api_key: Option<String>,
    pub provider: Option<String>,
}

impl SlotAuth {
    pub fn account_label(&self) -> Option<String> {
        let suffix = self.account_id.as_deref().and_then(short_account_id);
        if let Some(email) = self.email.as_deref().and_then(mask_email) {
            return Some(match suffix {
                Some(suffix) => format!("{email} #{suffix}"),
                None => email,
            });
        }

        if let Some(suffix) = suffix {
            return Some(format!("id:{suffix}"));
        }
        if self.api_key.is_some() {
            return Some("api_key".to_string());
        }
        self.provider
            .as_deref()
            .filter(|provider| *provider != "openai")
            .map(|provider| format!("provider:{provider}"))
    }
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
    let raw_id_token = tokens
        .and_then(|tokens| tokens.get("id_token"))
        .filter(|value| !value.is_null());
    let decoded_id_token = raw_id_token
        .and_then(Value::as_str)
        .and_then(decode_jwt_payload);
    let id_token = raw_id_token
        .and_then(Value::as_object)
        .or_else(|| decoded_id_token.as_ref().and_then(Value::as_object));

    let access_token = tokens
        .and_then(|tokens| tokens.get("access_token"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let refresh_token = tokens
        .and_then(|tokens| tokens.get("refresh_token"))
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
    let email = email_from_object(id_token)
        .or_else(|| email_from_object(tokens))
        .or_else(|| {
            auth.as_object()
                .and_then(|auth| email_from_object(Some(auth)))
        })
        .or_else(|| find_email(&auth));
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
        refresh_token,
        account_id,
        email,
        fedramp,
        api_key,
        provider,
    })
}

fn decode_jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn email_from_object(object: Option<&serde_json::Map<String, Value>>) -> Option<String> {
    let object = object?;
    [
        "email",
        "preferred_username",
        "login_hint",
        "chatgpt_account_email",
        "account_email",
        "user_email",
    ]
    .into_iter()
    .filter_map(|key| object.get(key).and_then(Value::as_str))
    .find_map(clean_email)
}

fn find_email(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            email_from_object(Some(object)).or_else(|| object.values().find_map(find_email))
        }
        Value::Array(values) => values.iter().find_map(find_email),
        _ => None,
    }
}

fn clean_email(value: &str) -> Option<String> {
    let value = value.trim();
    if value.contains('@') && !value.chars().any(char::is_whitespace) {
        Some(value.to_string())
    } else {
        None
    }
}

fn mask_email(value: &str) -> Option<String> {
    let (local, domain) = value.split_once('@')?;
    if local.is_empty() || domain.is_empty() {
        return None;
    }

    Some(format!("{}@{domain}", mask_local_part(local)))
}

fn mask_local_part(local: &str) -> String {
    let mut chars = local.chars();
    let Some(first) = chars.next() else {
        return "***".to_string();
    };
    let last = chars.last();
    match last {
        Some(last) if local.chars().count() > 2 => format!("{first}***{last}"),
        _ => format!("{first}***"),
    }
}

fn short_account_id(account_id: &str) -> Option<String> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return None;
    }

    let mut suffix = account_id.chars().rev().take(6).collect::<Vec<_>>();
    suffix.reverse();
    Some(suffix.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use serde_json::json;

    use super::*;

    #[test]
    fn account_label_masks_email_and_shortens_account_id() {
        let auth = SlotAuth {
            email: Some("user.one@example.com".to_string()),
            account_id: Some("account_1234567890".to_string()),
            ..SlotAuth::default()
        };

        assert_eq!(
            auth.account_label(),
            Some("u***e@example.com #567890".to_string())
        );
    }

    #[test]
    fn read_slot_auth_extracts_email_from_id_token_object() {
        let slot_dir = temp_slot_dir("id-token-object");
        fs::write(
            slot_dir.join("home/auth.json"),
            json!({
                "tokens": {
                    "access_token": "token",
                    "id_token": {
                        "email": "person@example.com",
                        "chatgpt_account_id": "acc_abcdef"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let auth = read_slot_auth(&slot_dir).unwrap();

        assert_eq!(auth.email, Some("person@example.com".to_string()));
        assert_eq!(auth.account_id, Some("acc_abcdef".to_string()));
        let _ = fs::remove_dir_all(slot_dir);
    }

    #[test]
    fn read_slot_auth_extracts_email_from_jwt_id_token() {
        let slot_dir = temp_slot_dir("jwt-id-token");
        let jwt = format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(
                json!({
                    "email": "jwt@example.com",
                    "chatgpt_account_id": "acc_jwt123"
                })
                .to_string()
            )
        );
        fs::write(
            slot_dir.join("home/auth.json"),
            json!({
                "tokens": {
                    "access_token": "token",
                    "id_token": jwt
                }
            })
            .to_string(),
        )
        .unwrap();

        let auth = read_slot_auth(&slot_dir).unwrap();

        assert_eq!(auth.email, Some("jwt@example.com".to_string()));
        assert_eq!(auth.account_id, Some("acc_jwt123".to_string()));
        let _ = fs::remove_dir_all(slot_dir);
    }

    #[test]
    fn read_slot_auth_extracts_refresh_token() {
        let slot_dir = temp_slot_dir("refresh-token");
        fs::write(
            slot_dir.join("home/auth.json"),
            json!({
                "tokens": {
                    "access_token": "access",
                    "refresh_token": "refresh",
                    "account_id": "acc_refresh"
                }
            })
            .to_string(),
        )
        .unwrap();

        let auth = read_slot_auth(&slot_dir).unwrap();

        assert_eq!(auth.access_token, Some("access".to_string()));
        assert_eq!(auth.refresh_token, Some("refresh".to_string()));
        assert_eq!(auth.account_id, Some("acc_refresh".to_string()));
        let _ = fs::remove_dir_all(slot_dir);
    }

    fn temp_slot_dir(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let slot_dir = std::env::temp_dir().join(format!("cx-auth-test-{name}-{unique}"));
        fs::create_dir_all(slot_dir.join("home")).unwrap();
        slot_dir
    }
}
