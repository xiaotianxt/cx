use std::cmp::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use reqwest::blocking::Client;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use reqwest::header::AUTHORIZATION;
use serde::Serialize;
use serde_json::Value;

use crate::auth;
use crate::paths::ManagerPaths;
use crate::slot;

const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";

#[derive(Debug, Clone)]
pub struct UsageChecker {
    client: Client,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
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
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
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
    Error,
    BadJson,
}

impl UsageChecker {
    pub fn new(timeout_seconds: f32) -> Result<Self> {
        let timeout = Duration::from_secs_f32(timeout_seconds.max(0.1));
        let client = Client::builder().timeout(timeout).build()?;
        Ok(Self { client })
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
        let slot_home = slot_dir.join("home");
        if !slot_home.is_dir() {
            return Ok(SlotResult::new(
                slot,
                index,
                SlotStatus::Missing,
                -1.0,
                "missing slot home",
            ));
        }

        let auth = auth::read_slot_auth(&slot_dir)?;
        let account_label = auth.account_label();
        if auth.access_token.is_none() {
            if auth.api_key.is_some() {
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

        let base_url = read_slot_base_url(&slot_dir, &slot_home)?;
        let url = usage_url(&base_url);
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

        let response = match self.client.get(&url).headers(headers).send() {
            Ok(response) => response,
            Err(err) => {
                return Ok(
                    SlotResult::new(slot, index, SlotStatus::Error, -1.0, err.to_string())
                        .with_account_label(account_label),
                );
            }
        };
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
        Ok(result_from_payload(slot, index, &payload).with_account_label(account_label))
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
        }
    }

    pub fn with_account_label(mut self, account_label: Option<String>) -> Self {
        self.account_label = account_label;
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
            SlotStatus::HttpError | SlotStatus::Error | SlotStatus::BadJson
        )
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
            SlotStatus::Error => "error",
            SlotStatus::BadJson => "bad_json",
        }
    }
}

pub fn sort_by_score_desc(results: &mut [SlotResult]) {
    results.sort_by(compare_by_score_desc);
}

fn compare_by_score_desc(left: &SlotResult, right: &SlotResult) -> Ordering {
    right
        .score
        .partial_cmp(&left.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left.index.cmp(&right.index))
}

pub fn compare_for_selection(left: &SlotResult, right: &SlotResult) -> Ordering {
    compare_by_score_desc(left, right)
}

fn read_slot_base_url(slot_dir: &std::path::Path, slot_home: &std::path::Path) -> Result<String> {
    let raw = slot::read_override_string(slot_dir, "chatgpt_base_url")?
        .or(slot::read_config_string(
            &slot_home.join("config.toml"),
            "chatgpt_base_url",
        )?)
        .unwrap_or_else(|| DEFAULT_CHATGPT_BASE_URL.to_string());
    Ok(normalize_chatgpt_base_url(&raw))
}

fn normalize_chatgpt_base_url(raw_url: &str) -> String {
    let mut base_url = raw_url.trim().trim_end_matches('/').to_string();
    if (base_url.starts_with("https://chatgpt.com")
        || base_url.starts_with("https://chat.openai.com"))
        && !base_url.contains("/backend-api")
    {
        base_url.push_str("/backend-api");
    }
    base_url
}

fn usage_url(base_url: &str) -> String {
    if base_url.contains("/backend-api") {
        format!("{base_url}/wham/usage")
    } else {
        format!("{base_url}/api/codex/usage")
    }
}

fn result_from_payload(slot: &str, index: usize, payload: &Value) -> SlotResult {
    let rate_limit = payload.get("rate_limit").unwrap_or(&Value::Null);
    let five_hour_window = rate_limit.get("primary_window").unwrap_or(&Value::Null);
    let weekly_window = rate_limit.get("secondary_window").unwrap_or(&Value::Null);
    let five_hour_used = used_percent(five_hour_window);
    let weekly_used = used_percent(weekly_window);
    let five_hour_reset_at = reset_at(five_hour_window);
    let weekly_reset_at = reset_at(weekly_window);
    let score = [five_hour_used, weekly_used]
        .into_iter()
        .flatten()
        .map(|used| 100.0 - used)
        .fold(100.0, f64::min);
    let allowed = rate_limit
        .get("allowed")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let limit_reached = rate_limit
        .get("limit_reached")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reset_at = five_hour_reset_at.or(weekly_reset_at);
    let plan_type = payload
        .get("plan_type")
        .and_then(Value::as_str)
        .map(str::to_string);
    let reached_type = rate_limit_reached_type(payload);

    let mut result = if allowed && !limit_reached {
        SlotResult::new(
            slot,
            index,
            SlotStatus::Available,
            score,
            summarize_window(
                five_hour_used,
                weekly_used,
                five_hour_reset_at,
                weekly_reset_at,
                score,
            ),
        )
    } else {
        let reason = reached_type
            .as_deref()
            .unwrap_or("limit reached")
            .to_string();
        SlotResult::new(
            slot,
            index,
            SlotStatus::Exhausted,
            score,
            format!(
                "{reason}; {}",
                summarize_window(
                    five_hour_used,
                    weekly_used,
                    five_hour_reset_at,
                    weekly_reset_at,
                    score
                )
            ),
        )
    };
    result.five_hour_used_percent = five_hour_used;
    result.weekly_used_percent = weekly_used;
    result.reset_at = reset_at;
    result.five_hour_refresh_at = five_hour_reset_at;
    result.weekly_refresh_at = weekly_reset_at;
    result.plan_type = plan_type;
    result.rate_limit_reached_type = reached_type;
    result
}

fn used_percent(window: &Value) -> Option<f64> {
    let value = window.get("used_percent")?.as_f64()?;
    value.is_finite().then(|| value.clamp(0.0, 100.0))
}

fn reset_at(window: &Value) -> Option<i64> {
    window.get("reset_at")?.as_i64()
}

fn rate_limit_reached_type(payload: &Value) -> Option<String> {
    let value = payload.get("rate_limit_reached_type")?;
    value
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn summarize_window(
    five_hour: Option<f64>,
    weekly: Option<f64>,
    five_hour_reset_at: Option<i64>,
    weekly_reset_at: Option<i64>,
    score: f64,
) -> String {
    let mut parts = vec![format!("remaining {score:.1}%")];
    if let Some(five_hour) = five_hour {
        parts.push(window_summary("5h", five_hour, five_hour_reset_at));
    }
    if let Some(weekly) = weekly {
        parts.push(window_summary("weekly", weekly, weekly_reset_at));
    }
    parts.join(", ")
}

fn window_summary(label: &str, used_percent: f64, refresh_at: Option<i64>) -> String {
    let mut summary = format!("{label} used {used_percent:.1}%");
    if let Some(refresh_at) = refresh_at.and_then(format_refresh_in) {
        summary.push_str(&format!(" (refresh {refresh_at})"));
    }
    summary
}

pub(crate) fn format_refresh_in(refresh_at: i64) -> Option<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let seconds = refresh_at.saturating_sub(now);
    if seconds <= 0 {
        return Some("now".to_string());
    }

    let minutes = (seconds + 59) / 60;
    if minutes < 60 {
        return Some(format!("in {minutes}m"));
    }

    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;
    if hours < 24 {
        if remaining_minutes == 0 {
            Some(format!("in {hours}h"))
        } else {
            Some(format!("in {hours}h {remaining_minutes}m"))
        }
    } else {
        let days = hours / 24;
        let remaining_hours = hours % 24;
        if remaining_hours == 0 {
            Some(format!("in {days}d"))
        } else {
            Some(format!("in {days}d {remaining_hours}h"))
        }
    }
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

        let result = result_from_payload("primary", 0, &payload);

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

        let result = result_from_payload("bus3", 3, &payload);

        assert_eq!(result.status, SlotStatus::Exhausted);
        assert_eq!(result.score, 0.0);
        assert_eq!(
            result.rate_limit_reached_type,
            Some("workspace_member_credits_depleted".to_string())
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
        let base_url = normalize_chatgpt_base_url("https://chatgpt.com");

        assert_eq!(
            usage_url(&base_url),
            "https://chatgpt.com/backend-api/wham/usage"
        );
    }
}
