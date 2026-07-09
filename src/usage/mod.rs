use std::time::Duration;
use std::time::SystemTime;

use anyhow::Result;
use reqwest::blocking::Client;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use reqwest::header::AUTHORIZATION;
use reqwest::header::RETRY_AFTER;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use time::format_description::well_known::Rfc2822;
use time::OffsetDateTime;

use crate::auth;
use crate::paths::ManagerPaths;
use crate::slot;

mod ollama;
mod payload;
mod score;

pub(crate) use payload::format_refresh_in;
pub use score::compare_for_selection;
pub use score::sort_by_score_desc;

const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";

#[derive(Debug, Clone)]
pub struct UsageChecker {
    client: Client,
    timeout: Duration,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SlotResult {
    pub slot: String,
    #[serde(rename = "account")]
    pub account_label: Option<String>,
    pub index: usize,
    pub status: SlotStatus,
    pub score: f64,
    pub summary: String,
    #[serde(rename = "fiveHourUsedPercent")]
    pub five_hour_used_percent: Option<f64>,
    #[serde(rename = "weeklyUsedPercent")]
    pub weekly_used_percent: Option<f64>,
    #[serde(rename = "resetAt")]
    pub reset_at: Option<i64>,
    #[serde(rename = "fiveHourRefreshAt")]
    pub five_hour_refresh_at: Option<i64>,
    #[serde(rename = "weeklyRefreshAt")]
    pub weekly_refresh_at: Option<i64>,
    #[serde(rename = "planType")]
    pub plan_type: Option<String>,
    #[serde(rename = "rateLimitReachedType")]
    pub rate_limit_reached_type: Option<String>,
    #[serde(rename = "cacheAgeSeconds", skip_serializing_if = "Option::is_none")]
    pub cache_age_seconds: Option<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub stale: bool,
    #[serde(rename = "refreshStatus", skip_serializing_if = "Option::is_none")]
    pub refresh_status: Option<String>,
    #[serde(rename = "retryAfterSeconds", skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<i64>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SlotStatus {
    Available,
    ApiKey,
    ExternalProvider,
    Exhausted,
    NeedsLogin,
    NoAuth,
    Missing,
    HttpError,
    RateLimited,
    Error,
    BadJson,
}

impl UsageChecker {
    pub fn new(timeout_seconds: f32) -> Result<Self> {
        let timeout = Duration::from_secs_f32(timeout_seconds.max(0.1));
        let client = Client::builder().timeout(timeout).build()?;
        Ok(Self { client, timeout })
    }

    pub fn query_slot(&self, paths: &ManagerPaths, slot: &str, index: usize) -> SlotResult {
        match self.query_slot_inner(paths, slot, index) {
            Ok(result) => result,
            Err(err) => SlotResult::new(slot, index, SlotStatus::Error, -1.0, format!("{err:#}")),
        }
    }

    fn query_slot_inner(
        &self,
        paths: &ManagerPaths,
        slot: &str,
        index: usize,
    ) -> Result<SlotResult> {
        let slot_dir = paths.slot_dir(slot);
        let slot_home = paths.slot_home(slot);
        if !slot_home.is_dir() {
            return Ok(SlotResult::new(
                slot,
                index,
                SlotStatus::Missing,
                -1.0,
                "missing slot home",
            ));
        }

        let mut auth = auth::read_slot_auth(&slot_dir, Some(&paths.base_codex_home))?;
        let mut account_label = auth.account_label();
        if auth.access_token.is_none() {
            if auth.api_key.is_some() {
                // Ollama Cloud slots: scrape ollama.com/settings via Helium
                // browser cookies for real usage data. Falls back to a plain
                // 100-score slot if cookies are unavailable.
                if auth.provider.as_deref() == Some("ollama") {
                    match ollama::query(slot, index, account_label.clone(), &self.client) {
                        Ok(result) => return Ok(result),
                        Err(err) => {
                            return Ok(SlotResult::new(
                                slot,
                                index,
                                SlotStatus::ExternalProvider,
                                100.0,
                                format!("ollama usage unavailable: {err:#}"),
                            )
                            .with_account_label(account_label));
                        }
                    }
                }
                return Ok(SlotResult::new(
                    slot,
                    index,
                    SlotStatus::ApiKey,
                    100.0,
                    "OpenAI API key slot",
                )
                .with_account_label(account_label));
            }
            if let Some(provider) = auth
                .provider
                .as_deref()
                .filter(|provider| *provider != "openai")
            {
                return Ok(SlotResult::new(
                    slot,
                    index,
                    SlotStatus::ExternalProvider,
                    100.0,
                    format!("external provider slot ({provider})"),
                )
                .with_account_label(account_label));
            }
            return Ok(SlotResult::new(
                slot,
                index,
                SlotStatus::NoAuth,
                -1.0,
                "no ChatGPT access token",
            )
            .with_account_label(account_label));
        }

        if auth.refresh_token.is_some()
            && auth
                .access_token
                .as_deref()
                .is_some_and(auth::access_token_is_expired)
        {
            match auth::refresh_slot_auth(&slot_dir, &self.client, Some(&paths.base_codex_home)) {
                Ok(Some(refreshed_auth)) => {
                    auth = refreshed_auth;
                    account_label = auth.account_label();
                }
                Ok(None) => {
                    return Ok(SlotResult::new(
                        slot,
                        index,
                        SlotStatus::NeedsLogin,
                        -1.0,
                        "ChatGPT access token expired and no refresh token is available",
                    )
                    .with_account_label(account_label));
                }
                Err(auth::RefreshSlotAuthError::Permanent(message)) => {
                    return Ok(SlotResult::new(
                        slot,
                        index,
                        SlotStatus::NeedsLogin,
                        -1.0,
                        format!(
                            "ChatGPT access token expired; token refresh failed permanently: {message}"
                        ),
                    )
                    .with_account_label(account_label));
                }
                Err(auth::RefreshSlotAuthError::Transient(message)) => {
                    return Ok(SlotResult::new(
                        slot,
                        index,
                        SlotStatus::Error,
                        -1.0,
                        format!("ChatGPT access token expired; token refresh failed: {message}"),
                    )
                    .with_account_label(account_label));
                }
            }
        }

        let base_url = read_slot_base_url(&slot_dir, &slot_home)?;
        let url = payload::usage_url(&base_url);

        if let Some(daemon_result) =
            try_daemon_rate_limits(slot, index, &slot_home, &account_label, self.timeout)
        {
            return Ok(daemon_result);
        }

        let mut response = match self.send_usage_request(&url, &auth) {
            Ok(response) => response,
            Err(err) => {
                return Ok(
                    SlotResult::new(slot, index, SlotStatus::Error, -1.0, err.to_string())
                        .with_account_label(account_label),
                );
            }
        };
        if response.status() == 401 && auth.refresh_token.is_some() {
            match auth::refresh_slot_auth(&slot_dir, &self.client, Some(&paths.base_codex_home)) {
                Ok(Some(refreshed_auth)) => {
                    auth = refreshed_auth;
                    account_label = auth.account_label();
                    response = match self.send_usage_request(&url, &auth) {
                        Ok(response) => response,
                        Err(err) => {
                            return Ok(SlotResult::new(
                                slot,
                                index,
                                SlotStatus::Error,
                                -1.0,
                                err.to_string(),
                            )
                            .with_account_label(account_label));
                        }
                    };
                }
                Ok(None) => {
                    return Ok(SlotResult::new(
                        slot,
                        index,
                        SlotStatus::NeedsLogin,
                        -1.0,
                        "usage check returned 401 and no refresh token is available",
                    )
                    .with_account_label(account_label));
                }
                Err(auth::RefreshSlotAuthError::Permanent(message)) => {
                    return Ok(SlotResult::new(
                        slot,
                        index,
                        SlotStatus::NeedsLogin,
                        -1.0,
                        format!(
                            "usage check returned 401; token refresh failed permanently: {message}"
                        ),
                    )
                    .with_account_label(account_label));
                }
                Err(auth::RefreshSlotAuthError::Transient(message)) => {
                    return Ok(SlotResult::new(
                        slot,
                        index,
                        SlotStatus::Error,
                        -1.0,
                        format!("usage check returned 401; token refresh failed: {message}"),
                    )
                    .with_account_label(account_label));
                }
            }
        }
        let status = response.status();
        if status == 401 || status == 403 {
            return Ok(SlotResult::new(
                slot,
                index,
                SlotStatus::NeedsLogin,
                -1.0,
                format!("usage check returned {status}"),
            )
            .with_account_label(account_label));
        }
        if status == 429 {
            let retry_after_seconds = response.headers().get(RETRY_AFTER).and_then(retry_after);
            let summary = match retry_after_seconds {
                Some(seconds) => format!("usage check rate limited; retry after {seconds}s"),
                None => "usage check rate limited".to_string(),
            };
            return Ok(
                SlotResult::new(slot, index, SlotStatus::RateLimited, -1.0, summary)
                    .with_account_label(account_label)
                    .with_retry_after_seconds(retry_after_seconds),
            );
        }
        if !status.is_success() {
            return Ok(SlotResult::new(
                slot,
                index,
                SlotStatus::HttpError,
                -1.0,
                format!("usage check returned {status}"),
            )
            .with_account_label(account_label));
        }

        let body = response.text().unwrap_or_default();
        let Ok(payload) = serde_json::from_str::<Value>(&body) else {
            return Ok(SlotResult::new(
                slot,
                index,
                SlotStatus::BadJson,
                -1.0,
                "usage response is not valid JSON",
            )
            .with_account_label(account_label));
        };
        Ok(payload::result_from_payload(slot, index, &payload).with_account_label(account_label))
    }

    fn send_usage_request(
        &self,
        url: &str,
        auth: &auth::SlotAuth,
    ) -> Result<reqwest::blocking::Response> {
        let mut headers = HeaderMap::new();
        headers.insert("User-Agent", HeaderValue::from_static("codex-cli"));
        if let Some(token) = auth.access_token.as_deref() {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))?;
            headers.insert(AUTHORIZATION, value);
        }
        if let Some(account_id) = auth.account_id.as_deref() {
            headers.insert("ChatGPT-Account-ID", HeaderValue::from_str(account_id)?);
        }
        if auth.fedramp {
            headers.insert("X-OpenAI-Fedramp", HeaderValue::from_static("true"));
        }

        Ok(self.client.get(url).headers(headers).send()?)
    }
}

impl SlotResult {
    pub fn new(
        slot: &str,
        index: usize,
        status: SlotStatus,
        score: f64,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            slot: slot.to_string(),
            account_label: None,
            index,
            status,
            score,
            summary: summary.into(),
            five_hour_used_percent: None,
            weekly_used_percent: None,
            reset_at: None,
            five_hour_refresh_at: None,
            weekly_refresh_at: None,
            plan_type: None,
            rate_limit_reached_type: None,
            cache_age_seconds: None,
            stale: false,
            refresh_status: None,
            retry_after_seconds: None,
        }
    }

    pub fn with_account_label(mut self, account_label: Option<String>) -> Self {
        self.account_label = account_label;
        self
    }

    pub fn with_retry_after_seconds(mut self, retry_after_seconds: Option<i64>) -> Self {
        self.retry_after_seconds = retry_after_seconds;
        self
    }

    pub fn with_refresh_status(mut self, refresh_status: impl Into<String>) -> Self {
        self.refresh_status = Some(refresh_status.into());
        self
    }

    pub fn mark_cached(
        mut self,
        index: usize,
        age_seconds: i64,
        stale: bool,
        refresh_status: Option<String>,
        retry_after_seconds: Option<i64>,
    ) -> Self {
        let age_seconds = age_seconds.max(0);
        self.index = index;
        self.cache_age_seconds = Some(age_seconds);
        self.stale = stale;
        self.refresh_status = refresh_status;
        self.retry_after_seconds = retry_after_seconds;
        if stale {
            self.summary = format!("{}; stale cache {age_seconds}s old", self.summary);
        } else {
            self.summary = format!("{}; cached {age_seconds}s ago", self.summary);
        }
        self
    }

    pub fn for_cache(mut self) -> Self {
        self.index = 0;
        self.cache_age_seconds = None;
        self.stale = false;
        self.refresh_status = None;
        self.retry_after_seconds = None;
        self
    }

    pub fn is_available(&self) -> bool {
        matches!(
            self.status,
            SlotStatus::Available | SlotStatus::ApiKey | SlotStatus::ExternalProvider
        )
    }

    pub fn is_transient(&self) -> bool {
        matches!(
            self.status,
            SlotStatus::HttpError
                | SlotStatus::RateLimited
                | SlotStatus::Error
                | SlotStatus::BadJson
        )
    }

    pub fn is_retryable_transient(&self) -> bool {
        matches!(
            self.status,
            SlotStatus::HttpError | SlotStatus::Error | SlotStatus::BadJson
        )
    }

    pub fn is_cacheable_usage(&self) -> bool {
        matches!(self.status, SlotStatus::Available | SlotStatus::Exhausted)
            && self.cache_age_seconds.is_none()
            && self.refresh_status.is_none()
    }
}

impl SlotStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SlotStatus::Available => "available",
            SlotStatus::ApiKey => "api_key",
            SlotStatus::ExternalProvider => "external_provider",
            SlotStatus::Exhausted => "exhausted",
            SlotStatus::NeedsLogin => "needs_login",
            SlotStatus::NoAuth => "no_auth",
            SlotStatus::Missing => "missing",
            SlotStatus::HttpError => "http_error",
            SlotStatus::RateLimited => "rate_limited",
            SlotStatus::Error => "error",
            SlotStatus::BadJson => "bad_json",
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn retry_after(value: &HeaderValue) -> Option<i64> {
    let raw = value.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(seconds) = raw.parse::<i64>() {
        return Some(seconds.max(0));
    }

    let retry_at = OffsetDateTime::parse(raw, &Rfc2822).ok()?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_else(|| OffsetDateTime::now_utc().unix_timestamp());
    Some(retry_at.unix_timestamp().saturating_sub(now).max(0))
}

fn read_slot_base_url(slot_dir: &std::path::Path, slot_home: &std::path::Path) -> Result<String> {
    let raw = slot::read_override_string(slot_dir, "chatgpt_base_url")?
        .or(slot::read_config_string(
            &slot_home.join("config.toml"),
            "chatgpt_base_url",
        )?)
        .unwrap_or_else(|| DEFAULT_CHATGPT_BASE_URL.to_string());
    Ok(payload::normalize_chatgpt_base_url(&raw))
}

fn try_daemon_rate_limits(
    slot: &str,
    index: usize,
    slot_home: &std::path::Path,
    account_label: &Option<String>,
    timeout: Duration,
) -> Option<SlotResult> {
    use serde_json::Value;
    let daemon = crate::daemon::try_query_rate_limits(slot_home, timeout)?;
    let rl = daemon.get("rateLimits")?;
    let primary = rl.get("primary")?;
    let secondary = rl.get("secondary")?;
    let credits = rl.get("credits");

    let five_hour_used = primary
        .get("usedPercent")
        .and_then(Value::as_i64)
        .map(|v| v as f64);
    let weekly_used = secondary
        .get("usedPercent")
        .and_then(Value::as_i64)
        .map(|v| v as f64);
    let five_hour_reset = primary.get("resetsAt").and_then(Value::as_i64);
    let weekly_reset = secondary.get("resetsAt").and_then(Value::as_i64);
    let has_credits = credits
        .and_then(|c| c.get("hasCredits"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let reached_type = rl.get("rateLimitReachedType");
    let allowed = has_credits && reached_type.is_none_or(|v| v.is_null());

    let remaining = 100.0 - weekly_used.or(five_hour_used).unwrap_or(0.0);
    let score = [five_hour_used, weekly_used]
        .into_iter()
        .flatten()
        .map(|used| 100.0 - used)
        .fold(100.0, f64::min);

    let reached_label = reached_type.and_then(Value::as_str).map(|s| s.to_string());

    let reset_at = five_hour_reset.or(weekly_reset);
    let exhausted_label = reached_label
        .clone()
        .unwrap_or_else(|| "limit reached".to_string());

    let mut result = if allowed {
        SlotResult::new(
            slot,
            index,
            SlotStatus::Available,
            score,
            payload::summarize_window(
                five_hour_used,
                weekly_used,
                five_hour_reset,
                weekly_reset,
                score,
            ),
        )
    } else {
        SlotResult::new(
            slot,
            index,
            SlotStatus::Exhausted,
            remaining,
            exhausted_label,
        )
    };
    result.five_hour_used_percent = five_hour_used;
    result.weekly_used_percent = weekly_used;
    result.reset_at = reset_at;
    result.five_hour_refresh_at = five_hour_reset;
    result.weekly_refresh_at = weekly_reset;
    result.rate_limit_reached_type = reached_label;
    Some(result.with_account_label(account_label.clone()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn credits_false_does_not_mark_slot_exhausted() {
        let payload = json!({
            "credits": { "has_credits": false },
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": { "used_percent": 2.0, "reset_at": 1 },
                "secondary_window": { "used_percent": 21.0, "reset_at": 2 }
            }
        });

        let result = payload::result_from_payload("primary", 0, &payload);

        assert_eq!(result.status, SlotStatus::Available);
        assert_eq!(result.score, 79.0);
        assert_eq!(
            result.summary,
            "remaining 79.0%, 5h used 2.0% (refresh now), weekly used 21.0% (refresh now)"
        );
        assert_eq!(result.reset_at, Some(1));
        assert_eq!(result.five_hour_refresh_at, Some(1));
        assert_eq!(result.weekly_refresh_at, Some(2));

        let json = serde_json::to_value(&result).expect("serialize result");
        assert_eq!(json.get("fiveHourUsedPercent"), Some(&json!(2.0)));
        assert_eq!(json.get("weeklyUsedPercent"), Some(&json!(21.0)));
        assert!(json.get("primaryUsedPercent").is_none());
        assert!(json.get("secondaryUsedPercent").is_none());
    }

    #[test]
    fn limit_reached_marks_slot_exhausted() {
        let payload = json!({
            "rate_limit_reached_type": { "type": "workspace_member_credits_depleted" },
            "rate_limit": {
                "allowed": false,
                "limit_reached": true,
                "primary_window": { "used_percent": 100.0 },
                "secondary_window": { "used_percent": 40.0 }
            }
        });

        let result = payload::result_from_payload("bus3", 3, &payload);

        assert_eq!(result.status, SlotStatus::Exhausted);
        assert_eq!(result.score, 0.0);
        assert_eq!(
            result.rate_limit_reached_type,
            Some("workspace_member_credits_depleted".to_string())
        );
    }

    #[test]
    fn retry_after_parses_delay_seconds() {
        let value = HeaderValue::from_static("12");

        assert_eq!(retry_after(&value), Some(12));
    }

    #[test]
    fn refresh_in_does_not_round_up_to_next_hour() {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        assert_eq!(
            payload::format_refresh_in(now + 5 * 60 * 60 - 1),
            Some("in 4h 59m".to_string())
        );
    }

    #[test]
    fn sorts_by_score_descending_then_rotation_order() {
        let mut results = vec![
            SlotResult::new("busy", 0, SlotStatus::Available, 20.0, "busy"),
            SlotResult::new("backup", 2, SlotStatus::Available, 90.0, "backup"),
            SlotResult::new("fresh", 1, SlotStatus::Available, 90.0, "fresh"),
            SlotResult::new("offline", 3, SlotStatus::Error, -1.0, "offline"),
        ];

        sort_by_score_desc(&mut results);

        let slots = results
            .iter()
            .map(|result| result.slot.as_str())
            .collect::<Vec<_>>();
        assert_eq!(slots, ["fresh", "backup", "busy", "offline"]);
    }

    #[test]
    fn chatgpt_url_uses_backend_usage_endpoint() {
        let base_url = payload::normalize_chatgpt_base_url("https://chatgpt.com");

        assert_eq!(
            payload::usage_url(&base_url),
            "https://chatgpt.com/backend-api/wham/usage"
        );
    }
}
