use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use serde_json::Map;
use serde_json::Value;

use crate::auth;
use crate::cli::AppSyncArgs;
use crate::paths::home_dir;
use crate::paths::ManagerPaths;
use crate::selector;
use crate::slot;
use crate::target;
use crate::usage;
use crate::usage::SlotStatus;

const MOUSEDO_CHATGPT_BASE_URL: &str = "https://chatgpt.com";
const MOUSEDO_SECRETS_PATH: &str = "Library/Application Support/Mousedo/AI Providers/secrets.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSyncResult {
    pub app: String,
    pub slot: String,
    pub account: String,
    pub path: PathBuf,
}

pub fn sync_app(paths: &ManagerPaths, args: AppSyncArgs) -> Result<AppSyncResult> {
    sync_mousedo(paths, &args)
}

fn sync_mousedo(paths: &ManagerPaths, args: &AppSyncArgs) -> Result<AppSyncResult> {
    let slot = select_oauth_slot(paths, args)?;
    let slot_auth = auth::read_slot_auth(&paths.slot_dir(&slot))?;
    require_oauth_fields(&slot_auth, &slot)?;

    let path = mousedo_secrets_path()?;
    sync_mousedo_json(&path, &slot_auth)?;

    Ok(AppSyncResult {
        account: slot_auth
            .account_label()
            .unwrap_or_else(|| "account".to_string()),
        app: "mousedo".to_string(),
        path,
        slot,
    })
}

fn select_oauth_slot(paths: &ManagerPaths, args: &AppSyncArgs) -> Result<String> {
    if let Some(slot) = args.slot.as_deref() {
        slot::validate_slot_name(slot)?;
        return Ok(slot.to_string());
    }

    let slots = if let Some(target_name) = args.target.as_deref() {
        target::load_target(paths, target_name)?.slots_or_rotation(paths)?
    } else {
        slot::load_rotation(paths)?
    };
    let options = selector::SlotQueryOptions::new(args.timeout, args.jobs, args.retries)
        .with_no_cache(args.no_cache);
    let mut progress = NoProgress;
    let results = selector::query_slots_with_progress(paths, &slots, options, &mut progress)?;

    let mut candidates = results
        .iter()
        .filter(|result| result.status == SlotStatus::Available)
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| usage::compare_for_selection(left, right));

    for candidate in candidates {
        let slot_auth = auth::read_slot_auth(&paths.slot_dir(&candidate.slot))?;
        if has_required_oauth_fields(&slot_auth) {
            return Ok(candidate.slot.clone());
        }
    }

    anyhow::bail!("no usable Codex OAuth slot found for app sync")
}

fn has_required_oauth_fields(slot_auth: &auth::SlotAuth) -> bool {
    slot_auth.access_token.is_some()
        && slot_auth.refresh_token.is_some()
        && slot_auth.account_id.is_some()
}

fn require_oauth_fields(slot_auth: &auth::SlotAuth, slot: &str) -> Result<()> {
    if slot_auth.access_token.is_none() {
        anyhow::bail!("slot `{slot}` has no ChatGPT OAuth access token");
    }
    if slot_auth.refresh_token.is_none() {
        anyhow::bail!("slot `{slot}` has no ChatGPT OAuth refresh token");
    }
    if slot_auth.account_id.is_none() {
        anyhow::bail!("slot `{slot}` has no ChatGPT account id");
    }
    Ok(())
}

fn sync_mousedo_json(path: &Path, slot_auth: &auth::SlotAuth) -> Result<()> {
    let mut document = read_json_object(path)?;
    set_dotted_json_value(
        &mut document,
        "secrets.openai.oauthAccessToken",
        Value::String(
            slot_auth
                .access_token
                .clone()
                .context("missing tokens.access_token")?,
        ),
    )?;
    set_dotted_json_value(
        &mut document,
        "secrets.openai.oauthRefreshToken",
        Value::String(
            slot_auth
                .refresh_token
                .clone()
                .context("missing tokens.refresh_token")?,
        ),
    )?;
    set_dotted_json_value(
        &mut document,
        "settings.openAI.authMethod",
        Value::String("oauth".to_string()),
    )?;
    set_dotted_json_value(
        &mut document,
        "settings.openAI.baseURL",
        Value::String(MOUSEDO_CHATGPT_BASE_URL.to_string()),
    )?;
    set_dotted_json_value(
        &mut document,
        "settings.openAI.isEnabled",
        Value::Bool(true),
    )?;
    set_dotted_json_value(
        &mut document,
        "settings.openAI.oauthAccountID",
        Value::String(
            slot_auth
                .account_id
                .clone()
                .context("missing tokens.account_id")?,
        ),
    )?;
    atomic_write_private_json(path, &document)
}

fn read_json_object(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let document = serde_json::from_str::<Value>(&content)
        .with_context(|| format!("parse {}", path.display()))?;
    if !document.is_object() {
        anyhow::bail!("{} must contain a JSON object", path.display());
    }
    Ok(document)
}

fn set_dotted_json_value(document: &mut Value, dotted_path: &str, value: Value) -> Result<()> {
    let parts = dotted_path
        .split('.')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        anyhow::bail!("json value path cannot be empty");
    }

    let mut current = document;
    for index in 0..parts.len() - 1 {
        let part = parts[index];
        let object = current
            .as_object_mut()
            .context("json parent is not an object")?;
        let next = object
            .entry(part.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !next.is_object() {
            let conflict = parts[..=index].join(".");
            anyhow::bail!(
                "json path `{dotted_path}` conflicts at `{conflict}`: existing value is not an object"
            );
        }
        current = next;
    }

    let object = current
        .as_object_mut()
        .context("json parent is not an object")?;
    object.insert(parts[parts.len() - 1].to_string(), value);
    Ok(())
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
        .context("app sync path must have a file name")?;
    let temp_path = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        unix_nanos()
    ));
    let content = serde_json::to_string_pretty(document).context("serialize Mousedo secrets")?;
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

fn mousedo_secrets_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(MOUSEDO_SECRETS_PATH))
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

struct NoProgress;

impl selector::SlotQueryProgress for NoProgress {}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use serde_json::json;

    use super::*;

    #[test]
    fn json_patch_preserves_unknown_fields_and_creates_objects() {
        let mut document = json!({
            "secrets": {
                "existing": "kept"
            },
            "settings": {
                "openAI": {
                    "defaultModel": "gpt-5.5"
                }
            }
        });

        set_dotted_json_value(
            &mut document,
            "secrets.openai.oauthAccessToken",
            Value::String("access".to_string()),
        )
        .unwrap();
        set_dotted_json_value(
            &mut document,
            "settings.openAI.isEnabled",
            Value::Bool(true),
        )
        .unwrap();

        assert_eq!(document["secrets"]["existing"], "kept");
        assert_eq!(document["secrets"]["openai"]["oauthAccessToken"], "access");
        assert_eq!(document["settings"]["openAI"]["defaultModel"], "gpt-5.5");
        assert_eq!(document["settings"]["openAI"]["isEnabled"], true);
    }

    #[test]
    fn json_patch_rejects_non_object_parent() {
        let mut document = json!({
            "settings": {
                "openAI": false
            }
        });

        let err = set_dotted_json_value(
            &mut document,
            "settings.openAI.isEnabled",
            Value::Bool(true),
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("existing value is not an object"));
        assert_eq!(document["settings"]["openAI"], false);
    }

    #[test]
    fn atomic_json_write_sets_private_mode() {
        let dir = temp_dir("atomic-json");
        let path = dir.join("secrets.json");
        let document = json!({ "ok": true });

        atomic_write_private_json(&path, &document).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"ok\": true"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn sync_mousedo_json_patches_expected_fields() {
        let dir = temp_dir("sync-json");
        let path = dir.join("secrets.json");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &path,
            json!({
                "secrets": {
                    "existing": "kept"
                },
                "settings": {
                    "openAI": {
                        "defaultModel": "gpt-5.5"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let auth = auth::SlotAuth {
            access_token: Some("access".to_string()),
            refresh_token: Some("refresh".to_string()),
            account_id: Some("account".to_string()),
            ..auth::SlotAuth::default()
        };

        sync_mousedo_json(&path, &auth).unwrap();

        let document = serde_json::from_str::<Value>(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(document["secrets"]["existing"], "kept");
        assert_eq!(document["secrets"]["openai"]["oauthAccessToken"], "access");
        assert_eq!(
            document["secrets"]["openai"]["oauthRefreshToken"],
            "refresh"
        );
        assert_eq!(document["settings"]["openAI"]["authMethod"], "oauth");
        assert_eq!(
            document["settings"]["openAI"]["baseURL"],
            MOUSEDO_CHATGPT_BASE_URL
        );
        assert_eq!(document["settings"]["openAI"]["defaultModel"], "gpt-5.5");
        assert_eq!(document["settings"]["openAI"]["isEnabled"], true);
        assert_eq!(document["settings"]["openAI"]["oauthAccountID"], "account");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let _ = fs::remove_dir_all(dir);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "cx-appsync-test-{name}-{}-{unique}",
            std::process::id()
        ))
    }
}
