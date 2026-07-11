use std::fmt;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use base64::engine::general_purpose::URL_SAFE;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde_json::json;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::envfile;
use crate::keychain;
use crate::slot;

const REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";
const CHATGPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshSlotAuthError {
    Permanent(String),
    Transient(String),
}

impl fmt::Display for RefreshSlotAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Permanent(message) | Self::Transient(message) => formatter.write_str(message),
        }
    }
}

fn resolve_auth_path(slot_dir: &Path, base_codex_home: Option<&Path>) -> Result<PathBuf> {
    let slot_auth = slot_dir.join("home").join("auth.json");
    if slot_auth.exists() {
        return Ok(slot_auth);
    }
    if let Some(home) = base_codex_home {
        if slot_dir == home {
            return Ok(home.join("auth.json"));
        }
    }
    Ok(slot_auth)
}

pub fn read_slot_auth(slot_dir: &Path, base_codex_home: Option<&Path>) -> Result<SlotAuth> {
    let provider = slot::read_override_string(slot_dir, "model_provider")?;

    // Read OPENAI_API_KEY from env.conf (takes precedence) and auth.json (fallback).
    let envs = envfile::read_env_file(&slot_dir.join("env.conf")).unwrap_or_default();
    let env_api_key = envs.get("OPENAI_API_KEY").cloned();

    // keychain.conf is authoritative for PAT-backed slots. Do not fall back to
    // auth.json here, or usage selection can disagree with launch behavior.
    if let Some(conf) = keychain::read_keychain_conf(slot_dir)? {
        let Some(pat) = keychain::fetch_pat_from_keychain(&conf.service, &conf.account)? else {
            eprintln!(
                "cx: Keychain entry {}/{} not found for PAT slot",
                conf.service, conf.account
            );
            return Ok(SlotAuth {
                provider,
                ..SlotAuth::default()
            });
        };
        if !keychain::is_pat(&pat) {
            eprintln!(
                "cx: Keychain entry {}/{} does not contain a PAT",
                conf.service, conf.account
            );
            return Ok(SlotAuth {
                provider,
                ..SlotAuth::default()
            });
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(keychain::whoami_timeout())
            .build();
        let hydrate_result = match &client {
            Ok(client) => keychain::read_or_hydrate_metadata(slot_dir, &pat, &envs, client),
            Err(err) => Err(anyhow::anyhow!("build whoami client: {err}")),
        };
        match hydrate_result {
            Ok(Some(metadata)) => {
                return Ok(SlotAuth {
                    access_token: Some(pat),
                    refresh_token: None,
                    account_id: Some(metadata.account_id),
                    email: metadata.email,
                    fedramp: metadata.fedramp,
                    api_key: None,
                    provider,
                });
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("cx: slot PAT unavailable ({err:#})");
            }
        }
        return Ok(SlotAuth {
            provider,
            ..SlotAuth::default()
        });
    }

    // OAuth fallback: read auth.json (existing logic).
    let auth_path = resolve_auth_path(slot_dir, base_codex_home)?;
    if !auth_path.exists() {
        return Ok(SlotAuth {
            api_key: env_api_key,
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
    let json_api_key = auth
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
        api_key: env_api_key.or(json_api_key),
        provider,
    })
}

/// Read provider from overrides.conf only, without touching keychain or auth.json.
/// Used by the legacy cross-slot session adapter.
pub fn read_slot_provider(slot_dir: &Path) -> Result<Option<String>> {
    slot::read_override_string(slot_dir, "model_provider")
}

pub fn refresh_slot_auth(
    slot_dir: &Path,
    client: &Client,
    base_codex_home: Option<&Path>,
) -> std::result::Result<Option<SlotAuth>, RefreshSlotAuthError> {
    refresh_slot_auth_with_endpoint(slot_dir, client, &refresh_token_url(), base_codex_home)
}

fn refresh_slot_auth_with_endpoint(
    slot_dir: &Path,
    client: &Client,
    endpoint: &str,
    base_codex_home: Option<&Path>,
) -> std::result::Result<Option<SlotAuth>, RefreshSlotAuthError> {
    let auth_path = match resolve_auth_path(slot_dir, base_codex_home) {
        Ok(path) => path,
        Err(err) => {
            return Err(RefreshSlotAuthError::Transient(format!(
                "resolve auth path: {err}"
            )));
        }
    };
    if !auth_path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&auth_path).map_err(|err| {
        RefreshSlotAuthError::Transient(format!("read {}: {err}", auth_path.display()))
    })?;
    let mut document = serde_json::from_str::<Value>(&content).map_err(|err| {
        RefreshSlotAuthError::Transient(format!("parse {}: {err}", auth_path.display()))
    })?;
    let refresh_token = document
        .get("tokens")
        .and_then(Value::as_object)
        .and_then(|tokens| tokens.get("refresh_token"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let Some(refresh_token) = refresh_token else {
        return Ok(None);
    };

    let response = client
        .post(endpoint)
        .json(&json!({
            "client_id": CHATGPT_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .map_err(|err| RefreshSlotAuthError::Transient(format!("token refresh failed: {err}")))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED {
            return Err(RefreshSlotAuthError::Permanent(refresh_rejected_message(
                &body,
            )));
        }
        return Err(RefreshSlotAuthError::Transient(format!(
            "token refresh returned {status}"
        )));
    }

    let refreshed = response.json::<Value>().map_err(|err| {
        RefreshSlotAuthError::Transient(format!("parse token refresh response: {err}"))
    })?;
    update_auth_document(&mut document, &refreshed)?;
    atomic_write_private_json(&auth_path, &document).map_err(|err| {
        RefreshSlotAuthError::Transient(format!("write {}: {err:#}", auth_path.display()))
    })?;
    read_slot_auth(slot_dir, base_codex_home)
        .map(Some)
        .map_err(|err| RefreshSlotAuthError::Transient(format!("read refreshed auth: {err:#}")))
}

pub fn access_token_is_expired(access_token: &str) -> bool {
    let Some(exp) = decode_jwt_payload(access_token)
        .and_then(|payload| payload.get("exp").and_then(Value::as_i64))
    else {
        return false;
    };
    exp <= unix_seconds()
}

fn decode_jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn refresh_token_url() -> String {
    std::env::var(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR)
        .unwrap_or_else(|_| REFRESH_TOKEN_URL.to_string())
}

fn update_auth_document(
    document: &mut Value,
    refreshed: &Value,
) -> std::result::Result<(), RefreshSlotAuthError> {
    let tokens = document
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| RefreshSlotAuthError::Transient("auth tokens object is missing".into()))?;

    for key in ["id_token", "access_token", "refresh_token"] {
        if let Some(value) = refreshed
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            tokens.insert(key.to_string(), Value::String(value.to_string()));
        }
    }

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|err| RefreshSlotAuthError::Transient(format!("format refresh time: {err}")))?;
    let object = document
        .as_object_mut()
        .ok_or_else(|| RefreshSlotAuthError::Transient("auth document is not an object".into()))?;
    object.insert("last_refresh".to_string(), Value::String(now));
    Ok(())
}

fn refresh_rejected_message(body: &str) -> String {
    match refresh_error_code(body).as_deref() {
        Some("refresh_token_expired") => "refresh token expired".to_string(),
        Some("refresh_token_reused") => "refresh token already used".to_string(),
        Some("refresh_token_invalidated") => "refresh token revoked".to_string(),
        Some(code) => format!("refresh token rejected ({code})"),
        None => "refresh token rejected".to_string(),
    }
}

fn refresh_error_code(body: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(body).ok()?;
    value
        .get("error")
        .and_then(|error| {
            error
                .get("code")
                .or_else(|| error.get("error_code"))
                .or_else(|| error.get("type"))
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
        })
        .or_else(|| value.get("code").and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn atomic_write_private_json(path: &Path, document: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let parent = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("auth path must have a file name")?;
    let temp_path = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        unix_nanos()
    ));
    let content = serde_json::to_string_pretty(document).context("serialize auth document")?;
    write_private_file(&temp_path, format!("{content}\n").as_bytes())?;
    fs::rename(&temp_path, path)
        .with_context(|| format!("replace {} with {}", temp_path.display(), path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn write_private_file(path: &Path, content: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    file.write_all(content)
        .with_context(|| format!("write {}", path.display()))
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, content: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    file.write_all(content)
        .with_context(|| format!("write {}", path.display()))
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_else(|_| OffsetDateTime::now_utc().unix_timestamp())
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
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;
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

        let auth = read_slot_auth(&slot_dir, None).unwrap();

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

        let auth = read_slot_auth(&slot_dir, None).unwrap();

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

        let auth = read_slot_auth(&slot_dir, None).unwrap();

        assert_eq!(auth.access_token, Some("access".to_string()));
        assert_eq!(auth.refresh_token, Some("refresh".to_string()));
        assert_eq!(auth.account_id, Some("acc_refresh".to_string()));
        let _ = fs::remove_dir_all(slot_dir);
    }

    #[test]
    fn read_slot_auth_does_not_fallback_to_oauth_when_keychain_entry_is_missing() {
        let slot_dir = temp_slot_dir("missing-keychain-pat");
        fs::write(
            slot_dir.join("keychain.conf"),
            "service=cx-test-missing\naccount=missing@example.com\n",
        )
        .unwrap();
        fs::write(slot_dir.join("env.conf"), "OPENAI_API_KEY=sk-test\n").unwrap();
        fs::write(
            slot_dir.join("home/auth.json"),
            json!({
                "tokens": {
                    "access_token": "old-access",
                    "refresh_token": "old-refresh",
                    "account_id": "acc_old"
                }
            })
            .to_string(),
        )
        .unwrap();

        let auth = read_slot_auth(&slot_dir, None).unwrap();

        assert_eq!(auth.access_token, None);
        assert_eq!(auth.refresh_token, None);
        assert_eq!(auth.account_id, None);
        assert_eq!(auth.api_key, None);
        let _ = fs::remove_dir_all(slot_dir);
    }

    #[test]
    fn read_slot_auth_does_not_fallback_to_default_for_named_slot() {
        let root = temp_slot_dir("named-slot-no-default-fallback");
        let base_home = root.join("base");
        let slot_dir = root.join("slots/dia1");
        fs::create_dir_all(slot_dir.join("home")).unwrap();
        fs::create_dir_all(&base_home).unwrap();
        fs::write(
            base_home.join("auth.json"),
            json!({
                "tokens": {
                    "access_token": "default-access",
                    "refresh_token": "default-refresh"
                }
            })
            .to_string(),
        )
        .unwrap();

        let auth = read_slot_auth(&slot_dir, Some(&base_home)).unwrap();

        assert_eq!(auth.access_token, None);
        assert_eq!(auth.refresh_token, None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_slot_auth_uses_default_auth_for_default_slot_home() {
        let root = temp_slot_dir("default-slot-auth");
        let base_home = root.join("base");
        fs::create_dir_all(&base_home).unwrap();
        fs::write(
            base_home.join("auth.json"),
            json!({
                "tokens": {
                    "access_token": "default-access",
                    "refresh_token": "default-refresh"
                }
            })
            .to_string(),
        )
        .unwrap();

        let auth = read_slot_auth(&base_home, Some(&base_home)).unwrap();

        assert_eq!(auth.access_token, Some("default-access".to_string()));
        assert_eq!(auth.refresh_token, Some("default-refresh".to_string()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn refresh_slot_auth_updates_tokens_and_preserves_private_mode() {
        let slot_dir = temp_slot_dir("refresh-success");
        fs::write(
            slot_dir.join("home/auth.json"),
            json!({
                "tokens": {
                    "access_token": "old-access",
                    "refresh_token": "old-refresh",
                    "id_token": jwt(json!({
                        "email": "old@example.com",
                        "chatgpt_account_id": "acc_old"
                    }))
                }
            })
            .to_string(),
        )
        .unwrap();
        let server = refresh_server(
            "200 OK",
            &json!({
                "access_token": "new-access",
                "refresh_token": "new-refresh",
                "id_token": jwt(json!({
                    "email": "new@example.com",
                    "chatgpt_account_id": "acc_new"
                }))
            })
            .to_string(),
        );
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let auth = refresh_slot_auth_with_endpoint(&slot_dir, &client, &server.url, None)
            .unwrap()
            .unwrap();
        let request = server.request.join().unwrap();
        let request_body = request.split("\r\n\r\n").nth(1).unwrap_or("");
        let request_json = serde_json::from_str::<Value>(request_body).unwrap();

        assert_eq!(
            request_json.get("grant_type"),
            Some(&json!("refresh_token"))
        );
        assert_eq!(
            request_json.get("refresh_token"),
            Some(&json!("old-refresh"))
        );
        assert_eq!(
            request_json.get("client_id"),
            Some(&json!(CHATGPT_CLIENT_ID))
        );
        assert_eq!(auth.access_token, Some("new-access".to_string()));
        assert_eq!(auth.refresh_token, Some("new-refresh".to_string()));
        assert_eq!(auth.email, Some("new@example.com".to_string()));
        assert_eq!(auth.account_id, Some("acc_new".to_string()));

        let document = fs::read_to_string(slot_dir.join("home/auth.json")).unwrap();
        let document = serde_json::from_str::<Value>(&document).unwrap();
        assert!(document
            .get("last_refresh")
            .and_then(Value::as_str)
            .is_some());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(slot_dir.join("home/auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        let _ = fs::remove_dir_all(slot_dir);
    }

    #[test]
    fn refresh_slot_auth_classifies_expired_refresh_token_as_permanent() {
        let slot_dir = temp_slot_dir("refresh-permanent");
        fs::write(
            slot_dir.join("home/auth.json"),
            json!({
                "tokens": {
                    "access_token": "old-access",
                    "refresh_token": "old-refresh"
                }
            })
            .to_string(),
        )
        .unwrap();
        let server = refresh_server(
            "401 Unauthorized",
            r#"{"error":{"code":"refresh_token_expired"}}"#,
        );
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let err =
            refresh_slot_auth_with_endpoint(&slot_dir, &client, &server.url, None).unwrap_err();
        let _ = server.request.join().unwrap();

        assert_eq!(
            err,
            RefreshSlotAuthError::Permanent("refresh token expired".to_string())
        );
        let _ = fs::remove_dir_all(slot_dir);
    }

    #[test]
    fn access_token_expiry_uses_jwt_exp_when_available() {
        let now = unix_seconds();

        assert!(access_token_is_expired(&jwt(json!({ "exp": now - 1 }))));
        assert!(!access_token_is_expired(&jwt(json!({ "exp": now + 3600 }))));
        assert!(!access_token_is_expired(&jwt(json!({ "sub": "no-exp" }))));
        assert!(!access_token_is_expired("not-a-jwt"));
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

    fn jwt(payload: Value) -> String {
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(payload.to_string())
        )
    }

    struct RefreshServer {
        url: String,
        request: thread::JoinHandle<String>,
    }

    fn refresh_server(status: &str, body: &str) -> RefreshServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let status = status.to_string();
        let body = body.to_string();
        let request = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => request.extend_from_slice(&chunk[..n]),
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        break;
                    }
                    Err(err) => panic!("read refresh request: {err}"),
                }
                if request_is_complete(&request) {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        RefreshServer {
            url: format!("http://{addr}/oauth/token"),
            request,
        }
    }

    fn request_is_complete(request: &[u8]) -> bool {
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .or_else(|| {
                headers
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
            })
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        request.len() >= header_end + content_length
    }
}
