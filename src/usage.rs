use std::cmp::Ordering;
use std::time::Duration;

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
    pub index: usize,
    pub status: SlotStatus,
    pub score: f64,
    pub summary: String,
    #[serde(rename = "primaryUsedPercent")]
    pub primary_used_percent: Option<f64>,
    #[serde(rename = "secondaryUsedPercent")]
    pub secondary_used_percent: Option<f64>,
    #[serde(rename = "resetAt")]
    pub reset_at: Option<i64>,
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
        if auth.access_token.is_none() {
            if auth.api_key.is_some() {
                return Ok(SlotResult::new(
                    slot,
                    index,
                    SlotStatus::ApiKey,
                    100.0,
                    "OpenAI API key slot",
                ));
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
                ));
            }
            return Ok(SlotResult::new(
                slot,
                index,
                SlotStatus::NoAuth,
                -1.0,
                "no ChatGPT access token",
            ));
        }

        let base_url = read_slot_base_url(&slot_dir, &slot_home)?;
        let url = usage_url(&base_url);
        let mut headers = HeaderMap::new();
        headers.insert("User-Agent", HeaderValue::from_static("codex-cli"));
        if let Some(token) = auth.access_token {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))?;
            headers.insert(AUTHORIZATION, value);
        }
        if let Some(account_id) = auth.account_id {
            headers.insert("ChatGPT-Account-ID", HeaderValue::from_str(&account_id)?);
        }
        if auth.fedramp {
            headers.insert("X-OpenAI-Fedramp", HeaderValue::from_static("true"));
        }

        let response = match self.client.get(&url).headers(headers).send() {
            Ok(response) => response,
            Err(err) => {
                return Ok(SlotResult::new(
                    slot,
                    index,
                    SlotStatus::Error,
                    -1.0,
                    err.to_string(),
                ));
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
            ));
        }
        if !status.is_success() {
            return Ok(SlotResult::new(
                slot,
                index,
                SlotStatus::HttpError,
                -1.0,
                format!("usage check returned {status}"),
            ));
        }

        let body = response.text().unwrap_or_default();
        let Ok(payload) = serde_json::from_str::<Value>(&body) else {
            return Ok(SlotResult::new(
                slot,
                index,
                SlotStatus::BadJson,
                -1.0,
                "usage response is not valid JSON",
            ));
        };
        Ok(result_from_payload(slot, index, &payload))
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
            index,
            status,
            score,
            summary: summary.into(),
            primary_used_percent: None,
            secondary_used_percent: None,
            reset_at: None,
            plan_type: None,
            rate_limit_reached_type: None,
        }
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

pub fn compare_for_selection(left: &SlotResult, right: &SlotResult) -> Ordering {
    right
        .score
        .partial_cmp(&left.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left.index.cmp(&right.index))
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
    let primary = rate_limit.get("primary_window").unwrap_or(&Value::Null);
    let secondary = rate_limit.get("secondary_window").unwrap_or(&Value::Null);
    let primary_used = used_percent(primary);
    let secondary_used = used_percent(secondary);
    let score = [primary_used, secondary_used]
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
    let reset_at = reset_at(primary).or_else(|| reset_at(secondary));
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
            summarize_window(primary_used, secondary_used, score),
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
                summarize_window(primary_used, secondary_used, score)
            ),
        )
    };
    result.primary_used_percent = primary_used;
    result.secondary_used_percent = secondary_used;
    result.reset_at = reset_at;
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

fn summarize_window(primary: Option<f64>, secondary: Option<f64>, score: f64) -> String {
    let mut parts = vec![format!("remaining {score:.1}%")];
    if let Some(primary) = primary {
        parts.push(format!("primary used {primary:.1}%"));
    }
    if let Some(secondary) = secondary {
        parts.push(format!("secondary used {secondary:.1}%"));
    }
    parts.join(", ")
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
                "primary_window": { "used_percent": 2.0 },
                "secondary_window": { "used_percent": 21.0 }
            }
        });

        let result = result_from_payload("primary", 0, &payload);

        assert_eq!(result.status, SlotStatus::Available);
        assert_eq!(result.score, 79.0);
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
    fn chatgpt_url_uses_backend_usage_endpoint() {
        let base_url = normalize_chatgpt_base_url("https://chatgpt.com");

        assert_eq!(
            usage_url(&base_url),
            "https://chatgpt.com/backend-api/wham/usage"
        );
    }
}
