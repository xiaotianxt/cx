use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

use wait_timeout::ChildExt;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::envfile;

const KEYCHAIN_CONF_FILE: &str = "keychain.conf";
const KEYCHAIN_META_FILE: &str = "keychain-meta.json";
const DEFAULT_AUTHAPI_BASE_URL: &str = "https://auth.openai.com/api/accounts";
const AUTHAPI_BASE_URL_ENV_VAR: &str = "CODEX_AUTHAPI_BASE_URL";
const WHOAMI_PATH: &str = "/v1/user-auth-credential/whoami";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KeychainConf {
    pub service: String,
    pub account: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SlotAuthMetadata {
    pub email: Option<String>,
    pub account_id: String,
    pub fedramp: bool,
    pub authapi_base_url: String,
    pub pat_fingerprint: String,
    pub cached_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HydrateError {
    Revoked(String),
    Transient(String),
}

impl std::fmt::Display for HydrateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Revoked(message) | Self::Transient(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for HydrateError {}

pub fn read_keychain_conf(slot_dir: &Path) -> Result<Option<KeychainConf>> {
    let path = slot_dir.join(KEYCHAIN_CONF_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let vars = envfile::read_env_file(&path)?;
    let service = vars
        .get("service")
        .context("keychain.conf missing `service`")?
        .clone();
    let account = vars
        .get("account")
        .context("keychain.conf missing `account`")?
        .clone();
    Ok(Some(KeychainConf { service, account }))
}

pub fn fetch_pat_from_keychain(service: &str, account: &str) -> Result<Option<String>> {
    let mut child = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| anyhow::anyhow!("security find-generic-password: {err}"))?;
    match child.wait_timeout(whoami_timeout()) {
        Ok(Some(status)) if !status.success() => return Ok(None),
        Ok(None) => {
            let _ = child.kill();
            return Ok(None);
        }
        Err(err) => return Err(anyhow::anyhow!("security wait failed: {err}")),
        _ => {}
    }
    let mut buf = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut buf)?;
    }
    let pat = buf.trim_end_matches(['\n', '\r', ' ']).to_string();
    if pat.is_empty() {
        return Ok(None);
    }
    Ok(Some(pat))
}

pub fn is_pat(token: &str) -> bool {
    token.starts_with("at-")
}

pub fn inject_pat_env_from_keychain(
    slot_dir: &Path,
    slot: &str,
    envs: &mut BTreeMap<String, String>,
) -> Result<()> {
    let Some(conf) = read_keychain_conf(slot_dir)? else {
        return Ok(());
    };
    inject_pat_env_from_conf_with_parent_env(
        slot,
        envs,
        &conf,
        parent_auth_env_var_present,
        fetch_pat_from_keychain,
    )
}

fn inject_pat_env_from_conf_with_parent_env<F, G>(
    slot: &str,
    envs: &mut BTreeMap<String, String>,
    conf: &KeychainConf,
    parent_env_var_present: G,
    fetch: F,
) -> Result<()>
where
    F: FnOnce(&str, &str) -> Result<Option<String>>,
    G: Fn(&str) -> bool,
{
    if auth_env_conflicts(envs, &parent_env_var_present) {
        anyhow::bail!(
            "slot {}: CODEX_ACCESS_TOKEN, CODEX_API_KEY, or OPENAI_API_KEY already set in env.conf/target, or CODEX_API_KEY/OPENAI_API_KEY already set in parent environment; remove one auth source (keychain.conf is also present)",
            slot
        );
    }
    let pat = fetch(&conf.service, &conf.account)?.with_context(|| {
        format!(
            "Keychain entry {}/{} not found; run `keychain-secret set {} {}`",
            conf.service, conf.account, conf.service, conf.account
        )
    })?;
    if !is_pat(&pat) {
        anyhow::bail!(
            "Keychain entry {}/{} does not contain a PAT (expected `at-` prefix)",
            conf.service,
            conf.account
        );
    }
    envs.insert("CODEX_ACCESS_TOKEN".to_string(), pat);
    Ok(())
}

fn auth_env_conflicts<G>(envs: &BTreeMap<String, String>, parent_env_var_present: &G) -> bool
where
    G: Fn(&str) -> bool,
{
    ["CODEX_ACCESS_TOKEN", "CODEX_API_KEY", "OPENAI_API_KEY"]
        .into_iter()
        .any(|key| envs.contains_key(key))
        || ["CODEX_API_KEY", "OPENAI_API_KEY"]
            .into_iter()
            .any(parent_env_var_present)
}

fn parent_auth_env_var_present(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|value| !value.is_empty())
}

pub fn pat_fingerprint(pat: &str) -> String {
    let hash = Sha256::digest(pat.as_bytes());
    let hex = hash.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!("sha256:{}", &hex[..16])
}

fn is_conf_older_than_cache(conf_path: &Path, cached_at: &str) -> bool {
    let Ok(cached_time) =
        time::OffsetDateTime::parse(cached_at, &time::format_description::well_known::Rfc3339)
    else {
        return false;
    };
    let Some(conf_mtime) = fs::metadata(conf_path).ok().and_then(|m| m.modified().ok()) else {
        return true; // No conf mtime = treat as fresh
    };
    let Ok(conf_time) = conf_mtime.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    let conf_offset = time::OffsetDateTime::from_unix_timestamp(conf_time.as_secs() as i64)
        .unwrap_or(cached_time);
    conf_offset <= cached_time
}

fn metadata_matches_token_and_endpoint(
    metadata: &SlotAuthMetadata,
    pat: &str,
    authapi_base_url: &str,
) -> bool {
    metadata.authapi_base_url == authapi_base_url
        && metadata.pat_fingerprint == pat_fingerprint(pat)
}

pub fn resolve_authapi_base_url(envs: &BTreeMap<String, String>) -> String {
    envs.get(AUTHAPI_BASE_URL_ENV_VAR)
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_AUTHAPI_BASE_URL.to_string())
}

pub fn hydrate_pat_metadata(
    pat: &str,
    authapi_base_url: &str,
    client: &reqwest::blocking::Client,
) -> std::result::Result<SlotAuthMetadata, HydrateError> {
    let endpoint = format!("{}{WHOAMI_PATH}", authapi_base_url.trim_end_matches('/'));
    let response = client
        .get(&endpoint)
        .bearer_auth(pat)
        .header("User-Agent", "cx-cli")
        .send()
        .map_err(|err| HydrateError::Transient(format!("whoami request failed: {err}")))?;
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(HydrateError::Revoked(format!(
            "personal access token rejected (HTTP {status})"
        )));
    }
    if !status.is_success() {
        return Err(HydrateError::Transient(format!(
            "whoami returned HTTP {status}"
        )));
    }
    let metadata: WhoamiResponse = response
        .json()
        .map_err(|err| HydrateError::Transient(format!("parse whoami response: {err}")))?;
    Ok(SlotAuthMetadata {
        email: metadata.email,
        account_id: metadata.chatgpt_account_id,
        fedramp: metadata.chatgpt_account_is_fedramp,
        authapi_base_url: authapi_base_url.to_string(),
        pat_fingerprint: pat_fingerprint(pat),
        cached_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
    })
}

#[derive(Debug, Deserialize)]
struct WhoamiResponse {
    email: Option<String>,
    chatgpt_account_id: String,
    chatgpt_account_is_fedramp: bool,
}

pub fn read_or_hydrate_metadata(
    slot_dir: &Path,
    pat: &str,
    envs: &BTreeMap<String, String>,
    client: &reqwest::blocking::Client,
) -> Result<Option<SlotAuthMetadata>> {
    let authapi_base_url = resolve_authapi_base_url(envs);
    let conf_path = slot_dir.join(KEYCHAIN_CONF_FILE);
    let meta_path = slot_dir.join(KEYCHAIN_META_FILE);

    if let Ok(content) = fs::read_to_string(&meta_path) {
        if let Ok(cached) = serde_json::from_str::<SlotAuthMetadata>(&content) {
            let cache_is_fresh =
                metadata_matches_token_and_endpoint(&cached, pat, &authapi_base_url)
                    && is_conf_older_than_cache(&conf_path, &cached.cached_at);
            if cache_is_fresh {
                return Ok(Some(cached));
            }
        }
    }

    match hydrate_pat_metadata(pat, &authapi_base_url, client) {
        Ok(metadata) => {
            write_meta_json(&meta_path, &metadata)?;
            Ok(Some(metadata))
        }
        Err(HydrateError::Transient(message)) => {
            if let Ok(content) = fs::read_to_string(&meta_path) {
                if let Ok(cached) = serde_json::from_str::<SlotAuthMetadata>(&content) {
                    if metadata_matches_token_and_endpoint(&cached, pat, &authapi_base_url) {
                        return Ok(Some(cached));
                    }
                }
            }
            Err(anyhow::anyhow!(message))
        }
        Err(HydrateError::Revoked(message)) => Err(anyhow::anyhow!(message)),
    }
}

fn write_meta_json(path: &Path, metadata: &SlotAuthMetadata) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let content = serde_json::to_string_pretty(metadata)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, format!("{content}\n")).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

pub fn write_keychain_conf(slot_dir: &Path, service: &str, account: &str) -> Result<()> {
    let path = slot_dir.join(KEYCHAIN_CONF_FILE);
    let content = format!("service={service}\naccount={account}\n");
    let tmp = path.with_extension("conf.tmp");
    fs::write(&tmp, &content).with_context(|| format!("write {}", tmp.display()))?;
    let path_display = path.display().to_string();
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path_display))?;
    Ok(())
}

pub fn remove_keychain_files(slot_dir: &Path) -> Result<()> {
    let conf = slot_dir.join(KEYCHAIN_CONF_FILE);
    let meta = slot_dir.join(KEYCHAIN_META_FILE);
    if conf.exists() {
        fs::remove_file(&conf)?;
    }
    if meta.exists() {
        fs::remove_file(&meta)?;
    }
    Ok(())
}

pub fn keychain_meta_path(slot_dir: &Path) -> PathBuf {
    slot_dir.join(KEYCHAIN_META_FILE)
}

pub fn whoami_timeout() -> Duration {
    Duration::from_secs(10)
}

pub fn pat_add(
    paths: &crate::paths::ManagerPaths,
    args: &crate::cli::PatAddArgs,
) -> anyhow::Result<()> {
    crate::slot::validate_slot_name(&args.slot)?;
    let slot_dir = paths.slot_dir(&args.slot);
    if args.slot != "default" && !slot_dir.is_dir() {
        anyhow::bail!("slot directory does not exist: {}", slot_dir.display());
    }
    if args.force {
        eprintln!("cx: --force is no longer needed; usable auth.json remains primary");
    }

    let pat = fetch_pat_from_keychain(&args.service, &args.account)?.with_context(|| {
        format!(
            "Keychain entry {} not found; run `keychain-secret set {} {}` first",
            args.service, args.service, args.account
        )
    })?;
    if !is_pat(&pat) {
        anyhow::bail!(
            "Keychain entry {}/{} does not contain a PAT (expected `at-` prefix)",
            args.service,
            args.account
        );
    }

    write_keychain_conf(&slot_dir, &args.service, &args.account)?;

    let envs = crate::envfile::read_env_file(&slot_dir.join("env.conf"))?;
    let client = reqwest::blocking::Client::builder()
        .timeout(whoami_timeout())
        .build()?;
    let metadata = read_or_hydrate_metadata(&slot_dir, &pat, &envs, &client)?
        .with_context(|| "failed to hydrate PAT metadata")?;

    println!(
        "pat bound: slot {} -> {}/{}",
        args.slot, args.service, args.account
    );
    println!("  priority: usable auth.json first, PAT fallback second");
    println!("  email: {}", metadata.email.as_deref().unwrap_or("(none)"));
    println!("  account_id: {}", metadata.account_id);
    println!("  fedramp: {}", metadata.fedramp);
    Ok(())
}

pub fn pat_check(
    paths: &crate::paths::ManagerPaths,
    args: &crate::cli::PatCheckArgs,
) -> anyhow::Result<()> {
    crate::slot::validate_slot_name(&args.slot)?;
    let slot_dir = paths.slot_dir(&args.slot);
    let conf = read_keychain_conf(&slot_dir)?
        .with_context(|| format!("no keychain.conf for slot {}", args.slot))?;
    let pat = fetch_pat_from_keychain(&conf.service, &conf.account)?
        .with_context(|| format!("Keychain entry {}/{} not found", conf.service, conf.account))?;
    if !is_pat(&pat) {
        anyhow::bail!(
            "Keychain entry {}/{} does not contain a PAT (expected `at-` prefix)",
            conf.service,
            conf.account
        );
    }
    let envs = crate::envfile::read_env_file(&slot_dir.join("env.conf"))?;
    let client = reqwest::blocking::Client::builder()
        .timeout(whoami_timeout())
        .build()?;
    let metadata = read_or_hydrate_metadata(&slot_dir, &pat, &envs, &client)?
        .with_context(|| "PAT metadata hydration failed (token may be revoked)")?;
    println!("slot {} PAT: ok", args.slot);
    println!("  email: {}", metadata.email.as_deref().unwrap_or("(none)"));
    println!("  account_id: {}", metadata.account_id);
    println!("  fedramp: {}", metadata.fedramp);
    Ok(())
}

pub fn pat_remove(
    paths: &crate::paths::ManagerPaths,
    args: &crate::cli::PatRemoveArgs,
) -> anyhow::Result<()> {
    crate::slot::validate_slot_name(&args.slot)?;
    let slot_dir = paths.slot_dir(&args.slot);
    let conf = read_keychain_conf(&slot_dir)?;
    remove_keychain_files(&slot_dir)?;
    println!(
        "removed keychain.conf and keychain-meta.json from slot {}",
        args.slot
    );
    if let Some(conf) = conf {
        println!(
            "Keychain entry not removed; run `keychain-secret delete {} {}` to remove it from Keychain",
            conf.service, conf.account
        );
    }
    Ok(())
}

pub fn pat_refresh(
    paths: &crate::paths::ManagerPaths,
    args: &crate::cli::PatRefreshArgs,
) -> anyhow::Result<()> {
    crate::slot::validate_slot_name(&args.slot)?;
    let slot_dir = paths.slot_dir(&args.slot);
    let conf = read_keychain_conf(&slot_dir)?
        .with_context(|| format!("no keychain.conf for slot {}", args.slot))?;
    let pat = fetch_pat_from_keychain(&conf.service, &conf.account)?
        .with_context(|| format!("Keychain entry {}/{} not found", conf.service, conf.account))?;
    let envs = crate::envfile::read_env_file(&slot_dir.join("env.conf"))?;
    let client = reqwest::blocking::Client::builder()
        .timeout(whoami_timeout())
        .build()?;
    let authapi_base_url = resolve_authapi_base_url(&envs);
    let metadata = hydrate_pat_metadata(&pat, &authapi_base_url, &client)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    write_meta_json(&keychain_meta_path(&slot_dir), &metadata)?;
    println!("refreshed metadata for slot {}", args.slot);
    println!("  email: {}", metadata.email.as_deref().unwrap_or("(none)"));
    println!("  account_id: {}", metadata.account_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_pat_env_from_conf_inserts_codex_access_token() {
        let conf = KeychainConf {
            service: "codex-pat".to_string(),
            account: "test@example.com".to_string(),
        };
        let mut envs = BTreeMap::new();

        inject_pat_env_from_conf_with_parent_env(
            "dia4",
            &mut envs,
            &conf,
            |_| false,
            |service, account| {
                assert_eq!(service, "codex-pat");
                assert_eq!(account, "test@example.com");
                Ok(Some("at-test".to_string()))
            },
        )
        .unwrap();

        assert_eq!(envs.get("CODEX_ACCESS_TOKEN"), Some(&"at-test".to_string()));
    }

    #[test]
    fn inject_pat_env_from_conf_rejects_existing_auth_env() {
        let conf = KeychainConf {
            service: "codex-pat".to_string(),
            account: "test@example.com".to_string(),
        };
        let mut envs = BTreeMap::new();
        envs.insert("OPENAI_API_KEY".to_string(), "sk-test".to_string());

        let err = inject_pat_env_from_conf_with_parent_env(
            "dia4",
            &mut envs,
            &conf,
            |_| false,
            |_, _| panic!("conflicting env should be rejected before keychain fetch"),
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("OPENAI_API_KEY"));
        assert!(!envs.contains_key("CODEX_ACCESS_TOKEN"));
    }

    #[test]
    fn inject_pat_env_from_conf_rejects_existing_codex_access_token_env() {
        let conf = KeychainConf {
            service: "codex-pat".to_string(),
            account: "test@example.com".to_string(),
        };
        let mut envs = BTreeMap::new();
        envs.insert("CODEX_ACCESS_TOKEN".to_string(), "at-existing".to_string());

        let err = inject_pat_env_from_conf_with_parent_env(
            "dia4",
            &mut envs,
            &conf,
            |_| false,
            |_, _| panic!("conflicting env should be rejected before keychain fetch"),
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("CODEX_ACCESS_TOKEN"));
        assert_eq!(
            envs.get("CODEX_ACCESS_TOKEN"),
            Some(&"at-existing".to_string())
        );
    }

    #[test]
    fn inject_pat_env_from_conf_rejects_parent_auth_env() {
        let conf = KeychainConf {
            service: "codex-pat".to_string(),
            account: "test@example.com".to_string(),
        };
        let mut envs = BTreeMap::new();

        let err = inject_pat_env_from_conf_with_parent_env(
            "dia4",
            &mut envs,
            &conf,
            |key| key == "OPENAI_API_KEY",
            |_, _| panic!("conflicting parent env should be rejected before keychain fetch"),
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("parent environment"));
        assert!(!envs.contains_key("CODEX_ACCESS_TOKEN"));
    }

    #[test]
    fn inject_pat_env_from_conf_allows_parent_codex_access_token() {
        let conf = KeychainConf {
            service: "codex-pat".to_string(),
            account: "test@example.com".to_string(),
        };
        let mut envs = BTreeMap::new();

        inject_pat_env_from_conf_with_parent_env(
            "dia4",
            &mut envs,
            &conf,
            |key| key == "CODEX_ACCESS_TOKEN",
            |_, _| Ok(Some("at-slot".to_string())),
        )
        .unwrap();

        assert_eq!(envs.get("CODEX_ACCESS_TOKEN"), Some(&"at-slot".to_string()));
    }

    #[test]
    fn inject_pat_env_from_conf_rejects_non_pat_token() {
        let conf = KeychainConf {
            service: "codex-pat".to_string(),
            account: "test@example.com".to_string(),
        };
        let mut envs = BTreeMap::new();

        let err = inject_pat_env_from_conf_with_parent_env(
            "dia4",
            &mut envs,
            &conf,
            |_| false,
            |_, _| Ok(Some("sk-test".to_string())),
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("does not contain a PAT"));
        assert!(!envs.contains_key("CODEX_ACCESS_TOKEN"));
    }

    #[test]
    fn inject_pat_env_from_conf_errors_when_keychain_entry_is_missing() {
        let conf = KeychainConf {
            service: "codex-pat".to_string(),
            account: "test@example.com".to_string(),
        };
        let mut envs = BTreeMap::new();

        let err = inject_pat_env_from_conf_with_parent_env(
            "dia4",
            &mut envs,
            &conf,
            |_| false,
            |_, _| Ok(None),
        )
        .unwrap_err();

        let message = format!("{err:#}");
        assert!(message.contains("Keychain entry codex-pat/test@example.com not found"));
        assert!(message.contains("keychain-secret set codex-pat test@example.com"));
        assert!(!envs.contains_key("CODEX_ACCESS_TOKEN"));
    }

    #[test]
    fn inject_pat_env_from_conf_propagates_keychain_fetch_errors() {
        let conf = KeychainConf {
            service: "codex-pat".to_string(),
            account: "test@example.com".to_string(),
        };
        let mut envs = BTreeMap::new();

        let err = inject_pat_env_from_conf_with_parent_env(
            "dia4",
            &mut envs,
            &conf,
            |_| false,
            |_, _| Err(anyhow::anyhow!("security wait failed")),
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("security wait failed"));
        assert!(!envs.contains_key("CODEX_ACCESS_TOKEN"));
    }
}
