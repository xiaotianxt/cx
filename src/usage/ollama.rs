use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use aes::Aes128;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::BlockDecryptMut;
use cbc::cipher::KeyIvInit;
use pbkdf2::pbkdf2_hmac;
use reqwest::blocking::Client;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use sha1::Sha1;
use sha2::Digest;
use sha2::Sha256;
use wait_timeout::ChildExt;

use super::payload;
use super::SlotResult;
use super::SlotStatus;

const SETTINGS_URL: &str = "https://ollama.com/settings";
const HELIUM_COOKIE_DB: &str = "Library/Application Support/net.imput.helium/Default/Cookies";
const HELIUM_KEYCHAIN_SERVICE: &str = "Helium Storage Key";
const HELIUM_KEYCHAIN_ACCOUNT: &str = "Helium";
const CHROME_ROOT: &str = "Library/Application Support/Google/Chrome";
const CHROME_DEFAULT_PROFILE: &str = "Default";
const CHROME_KEYCHAIN_SERVICE: &str = "Chrome Safe Storage";
const CHROME_KEYCHAIN_ACCOUNT: &str = "Chrome";
const OLLAMA_HOST: &str = "ollama.com";
const SESSION_COOKIE: &str = "__Secure-session";
const AID_COOKIE: &str = "aid";
const CHROME_COOKIE_PREFIX: &[u8] = b"v10";
const CHROME_COOKIE_SALT: &[u8] = b"saltysalt";
const CHROME_COOKIE_IV: [u8; 16] = [b' '; 16];
const CHROME_COOKIE_ITERATIONS: u32 = 1003;
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(5);

type Aes128CbcDec = cbc::Decryptor<Aes128>;

#[derive(Debug, Clone)]
struct BrowserCookieSource {
    name: String,
    cookie_db: PathBuf,
    keychain_service: String,
    keychain_account: String,
}

#[derive(Debug, Clone)]
struct CookieRow {
    host: String,
    name: String,
    encrypted_value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
struct OllamaUsage {
    session_used_percent: f64,
    weekly_used_percent: f64,
    session_reset_at: Option<i64>,
    weekly_reset_at: Option<i64>,
}

pub(super) fn query(
    slot: &str,
    index: usize,
    account_label: Option<String>,
    client: &Client,
    envs: &BTreeMap<String, String>,
) -> Result<SlotResult> {
    let sources = BrowserCookieSource::configured_sources(envs)?;
    let mut failures = Vec::new();
    for source in sources {
        match query_from_source(&source, client) {
            Ok(usage) => return Ok(result_from_usage(slot, index, account_label, usage)),
            Err(err) => failures.push(format!("{}: {err:#}", source.name)),
        }
    }
    bail!("all Ollama cookie sources failed: {}", failures.join("; "))
}

fn query_from_source(source: &BrowserCookieSource, client: &Client) -> Result<OllamaUsage> {
    let cookies = read_ollama_cookies(source)?;
    let html = fetch_settings(client, &cookies)?;
    parse_settings_usage(&html).context("parse Ollama settings usage")
}

impl BrowserCookieSource {
    fn configured_sources(envs: &BTreeMap<String, String>) -> Result<Vec<Self>> {
        if let Some(source) = Self::from_env(envs)? {
            return Ok(vec![source]);
        }
        let requested = env_value(envs, "CX_OLLAMA_COOKIE_SOURCE")
            .unwrap_or_else(|_| "auto".to_string())
            .to_ascii_lowercase();
        match requested.as_str() {
            "auto" => Ok(vec![Self::helium()?, Self::chrome_with_env(envs)?]),
            "helium" => Ok(vec![Self::helium()?]),
            "chrome" => Ok(vec![Self::chrome_with_env(envs)?]),
            other => bail!(
                "unsupported CX_OLLAMA_COOKIE_SOURCE={other}; expected auto, helium, or chrome"
            ),
        }
    }

    fn from_env(envs: &BTreeMap<String, String>) -> Result<Option<Self>> {
        let Some(cookie_db) = env_os_value(envs, "CX_OLLAMA_COOKIE_DB") else {
            return Ok(None);
        };
        let keychain_service = env_value(envs, "CX_OLLAMA_KEYCHAIN_SERVICE")
            .unwrap_or_else(|_| CHROME_KEYCHAIN_SERVICE.to_string());
        let keychain_account = env_value(envs, "CX_OLLAMA_KEYCHAIN_ACCOUNT")
            .unwrap_or_else(|_| CHROME_KEYCHAIN_ACCOUNT.to_string());
        Ok(Some(Self {
            name: "custom".to_string(),
            cookie_db: PathBuf::from(cookie_db),
            keychain_service,
            keychain_account,
        }))
    }

    fn helium() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is not set"))?;
        Ok(Self {
            name: "helium".to_string(),
            cookie_db: home.join(HELIUM_COOKIE_DB),
            keychain_service: HELIUM_KEYCHAIN_SERVICE.to_string(),
            keychain_account: HELIUM_KEYCHAIN_ACCOUNT.to_string(),
        })
    }

    fn chrome_with_env(envs: &BTreeMap<String, String>) -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is not set"))?;
        let profile = env_value(envs, "CX_OLLAMA_CHROME_PROFILE")
            .unwrap_or_else(|_| CHROME_DEFAULT_PROFILE.to_string());
        Ok(Self {
            name: format!("chrome/{profile}"),
            cookie_db: home.join(CHROME_ROOT).join(profile).join("Cookies"),
            keychain_service: CHROME_KEYCHAIN_SERVICE.to_string(),
            keychain_account: CHROME_KEYCHAIN_ACCOUNT.to_string(),
        })
    }

    #[cfg(test)]
    fn chrome() -> Result<Self> {
        Self::chrome_with_env(&BTreeMap::new())
    }
}

fn env_value(
    envs: &BTreeMap<String, String>,
    key: &str,
) -> std::result::Result<String, std::env::VarError> {
    envs.get(key)
        .cloned()
        .ok_or(std::env::VarError::NotPresent)
        .or_else(|_| std::env::var(key))
}

fn env_os_value(envs: &BTreeMap<String, String>, key: &str) -> Option<OsString> {
    envs.get(key)
        .map(OsString::from)
        .or_else(|| std::env::var_os(key))
}

fn read_ollama_cookies(source: &BrowserCookieSource) -> Result<Vec<(String, String)>> {
    let rows = read_cookie_rows(&source.cookie_db)?;
    if rows.is_empty() {
        bail!("no ollama.com cookies in {}", source.cookie_db.display());
    }
    let password = read_keychain_password(&source.keychain_service, &source.keychain_account)?;
    let mut cookies = Vec::new();
    for row in rows {
        let value = decrypt_chromium_cookie(&row.host, &row.encrypted_value, password.as_bytes())
            .with_context(|| format!("decrypt {} cookie", row.name))?;
        cookies.push((row.name, value));
    }
    Ok(cookies)
}

fn read_cookie_rows(path: &Path) -> Result<Vec<CookieRow>> {
    if !path.exists() {
        bail!("cookie DB not found at {}", path.display());
    }
    // Helium holds a SQLite lock while running. Copy the DB (and WAL) to a
    // temp file so we can open it read-only without contending.
    let tmp = std::env::temp_dir().join(format!(
        "cx-ollama-cookies-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&tmp).context("create temp dir for cookie copy")?;
    let db_copy = tmp.join("Cookies");
    fs::copy(path, &db_copy).with_context(|| format!("copy {} to temp", path.display()))?;
    // Best-effort WAL/SHM copy — may not exist.
    for sidecar in ["Cookies-wal", "Cookies-shm"] {
        let src = path.with_file_name(sidecar);
        if src.exists() {
            let _ = fs::copy(&src, db_copy.with_file_name(sidecar));
        }
    }

    let conn = Connection::open_with_flags(&db_copy, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open copied cookie DB from {}", path.display()))?;
    let mut stmt = conn.prepare(
        "SELECT host_key, name, encrypted_value
         FROM cookies
         WHERE host_key = ?1 AND name IN (?2, ?3)
         ORDER BY name",
    )?;
    let rows = stmt
        .query_map(params![OLLAMA_HOST, SESSION_COOKIE, AID_COOKIE], |row| {
            Ok(CookieRow {
                host: row.get(0)?,
                name: row.get(1)?,
                encrypted_value: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let _ = fs::remove_dir_all(&tmp);
    Ok(rows)
}

fn read_keychain_password(service: &str, account: &str) -> Result<String> {
    let mut child = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn security for {service}/{account}"))?;
    match child.wait_timeout(KEYCHAIN_TIMEOUT) {
        Ok(Some(status)) => {
            if !status.success() {
                bail!("keychain item {service}/{account} was not found");
            }
        }
        Ok(None) => {
            let _ = child.kill();
            bail!("keychain read for {service}/{account} timed out");
        }
        Err(err) => {
            bail!("keychain read for {service}/{account} failed: {err}");
        }
    }
    let output = child.wait_with_output()?;
    let password = String::from_utf8(output.stdout).context("keychain password is not UTF-8")?;
    let password = password.trim_end();
    if password.is_empty() {
        bail!("keychain item {service}/{account} is empty");
    }
    Ok(password.to_string())
}

fn decrypt_chromium_cookie(host: &str, encrypted: &[u8], password: &[u8]) -> Result<String> {
    let ciphertext = encrypted
        .strip_prefix(CHROME_COOKIE_PREFIX)
        .ok_or_else(|| anyhow!("unsupported Chromium cookie prefix"))?;
    let mut key = [0u8; 16];
    pbkdf2_hmac::<Sha1>(
        password,
        CHROME_COOKIE_SALT,
        CHROME_COOKIE_ITERATIONS,
        &mut key,
    );
    let mut buffer = ciphertext.to_vec();
    let decrypted = Aes128CbcDec::new(&key.into(), &CHROME_COOKIE_IV.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map_err(|_| anyhow!("AES-CBC cookie decrypt failed"))?;
    let host_hash = Sha256::digest(host.as_bytes());
    let value = decrypted
        .strip_prefix(host_hash.as_slice())
        .unwrap_or(decrypted);
    String::from_utf8(value.to_vec()).context("cookie value is not UTF-8")
}

fn fetch_settings(client: &Client, cookies: &[(String, String)]) -> Result<String> {
    let cookie_header = cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    let response = client
        .get(SETTINGS_URL)
        .header(reqwest::header::USER_AGENT, "Mozilla/5.0")
        .header(reqwest::header::COOKIE, cookie_header)
        .send()
        .context("fetch Ollama settings")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Ollama settings returned {status}");
    }
    response.text().context("read Ollama settings response")
}

fn result_from_usage(
    slot: &str,
    index: usize,
    account_label: Option<String>,
    usage: OllamaUsage,
) -> SlotResult {
    let score = [
        Some(usage.session_used_percent),
        Some(usage.weekly_used_percent),
    ]
    .into_iter()
    .flatten()
    .map(|used| 100.0 - used)
    .fold(100.0, f64::min);
    let mut result = SlotResult::new(
        slot,
        index,
        SlotStatus::Available,
        score,
        payload::summarize_window(
            Some(usage.session_used_percent),
            Some(usage.weekly_used_percent),
            usage.session_reset_at,
            usage.weekly_reset_at,
            score,
        ),
    );
    result.account_label = account_label;
    result.five_hour_used_percent = Some(usage.session_used_percent);
    result.weekly_used_percent = Some(usage.weekly_used_percent);
    result.reset_at = usage.session_reset_at.or(usage.weekly_reset_at);
    result.five_hour_refresh_at = usage.session_reset_at;
    result.weekly_refresh_at = usage.weekly_reset_at;
    result.plan_type = Some("ollama".to_string());
    result
}

fn parse_settings_usage(html: &str) -> Option<OllamaUsage> {
    let text = html_to_text(html);
    Some(OllamaUsage {
        session_used_percent: parse_used_percent(&text, "Session usage")?,
        weekly_used_percent: parse_used_percent(&text, "Weekly usage")?,
        session_reset_at: parse_reset_at(&text, "Session usage"),
        weekly_reset_at: parse_reset_at(&text, "Weekly usage"),
    })
}

fn parse_used_percent(text: &str, label: &str) -> Option<f64> {
    let after = text.split_once(label)?.1;
    let before_used = after.split_once("% used")?.0;
    parse_last_number(before_used)
}

fn parse_reset_at(text: &str, label: &str) -> Option<i64> {
    let after = text.split_once(label)?.1;
    let after_reset = after.split_once("Resets in ")?.1;
    let phrase = after_reset.split('.').next()?.trim();
    let seconds = parse_duration_seconds(phrase)?;
    Some(unix_now().saturating_add(seconds))
}

fn parse_duration_seconds(phrase: &str) -> Option<i64> {
    let mut seconds = 0i64;
    let mut words = phrase.split_whitespace();
    while let Some(raw_number) = words.next() {
        let number = raw_number.parse::<i64>().ok()?;
        let unit = words.next()?.trim_end_matches(',');
        seconds = seconds.saturating_add(match unit {
            "hour" | "hours" => number.saturating_mul(60 * 60),
            "minute" | "minutes" => number.saturating_mul(60),
            "day" | "days" => number.saturating_mul(24 * 60 * 60),
            _ => return None,
        });
    }
    (seconds > 0).then_some(seconds)
}

fn parse_last_number(text: &str) -> Option<f64> {
    text.split(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<f64>().ok())
        .next_back()
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 100.0))
}

fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => {
                in_tag = true;
                text.push('\n');
            }
            '>' => {
                in_tag = false;
                text.push('\n');
            }
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&#x27;", "'")
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn settings_usage_parser_reads_percentages_and_resets() {
        let html = r#"
            <html><title>Usage · Settings</title>
            <body>
            <h2>Session usage</h2>
            <p>59.6% used</p>
            <p>Resets in 2 hours.</p>
            <h2>Weekly usage</h2>
            <p>47.3% used</p>
            <p>Resets in 7 hours.</p>
            </body></html>
        "#;

        let usage = parse_settings_usage(html).expect("usage parses");

        assert_eq!(usage.session_used_percent, 59.6);
        assert_eq!(usage.weekly_used_percent, 47.3);
        assert!(usage.session_reset_at.is_some());
        assert!(usage.weekly_reset_at.is_some());
    }

    #[test]
    fn duration_parser_accepts_hours_and_minutes() {
        assert_eq!(parse_duration_seconds("2 hours 5 minutes"), Some(7_500));
        assert_eq!(parse_duration_seconds("7 hours"), Some(25_200));
    }

    #[test]
    fn chrome_source_uses_requested_profile() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CX_OLLAMA_COOKIE_DB");
        std::env::remove_var("CX_OLLAMA_COOKIE_SOURCE");
        std::env::set_var("CX_OLLAMA_CHROME_PROFILE", "Profile 5");

        let source = BrowserCookieSource::chrome().expect("chrome source");

        assert_eq!(source.name, "chrome/Profile 5");
        assert!(source
            .cookie_db
            .ends_with("Google/Chrome/Profile 5/Cookies"));

        std::env::remove_var("CX_OLLAMA_CHROME_PROFILE");
    }

    #[test]
    fn explicit_cookie_db_overrides_source_selection() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("CX_OLLAMA_COOKIE_DB", "/tmp/ollama-cookies");
        std::env::set_var("CX_OLLAMA_KEYCHAIN_SERVICE", "Custom Storage");
        std::env::set_var("CX_OLLAMA_KEYCHAIN_ACCOUNT", "Custom Account");
        std::env::set_var("CX_OLLAMA_COOKIE_SOURCE", "helium");

        let sources = BrowserCookieSource::configured_sources(&BTreeMap::new()).expect("sources");

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "custom");
        assert_eq!(sources[0].cookie_db, PathBuf::from("/tmp/ollama-cookies"));
        assert_eq!(sources[0].keychain_service, "Custom Storage");
        assert_eq!(sources[0].keychain_account, "Custom Account");

        std::env::remove_var("CX_OLLAMA_COOKIE_DB");
        std::env::remove_var("CX_OLLAMA_KEYCHAIN_SERVICE");
        std::env::remove_var("CX_OLLAMA_KEYCHAIN_ACCOUNT");
        std::env::remove_var("CX_OLLAMA_COOKIE_SOURCE");
    }

    #[test]
    fn slot_env_selects_chrome_profile_for_configured_sources() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CX_OLLAMA_COOKIE_DB");
        std::env::remove_var("CX_OLLAMA_COOKIE_SOURCE");
        std::env::remove_var("CX_OLLAMA_CHROME_PROFILE");
        let envs = BTreeMap::from([
            ("CX_OLLAMA_COOKIE_SOURCE".to_string(), "chrome".to_string()),
            (
                "CX_OLLAMA_CHROME_PROFILE".to_string(),
                "Profile 5".to_string(),
            ),
        ]);

        let sources = BrowserCookieSource::configured_sources(&envs).expect("sources");

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "chrome/Profile 5");
        assert!(sources[0]
            .cookie_db
            .ends_with("Google/Chrome/Profile 5/Cookies"));
    }
}
