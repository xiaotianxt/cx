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

mod payload;
mod score;

pub(crate) use payload::format_refresh_in;
pub use score::compare_for_selection;
pub use score::sort_by_score_desc;

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
        let url = payload::usage_url(&base_url);
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
        Ok(payload::result_from_payload(slot, index, &payload).with_account_label(account_label))
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

fn read_slot_base_url(slot_dir: &std::path::Path, slot_home: &std::path::Path) -> Result<String> {
    let raw = slot::read_override_string(slot_dir, "chatgpt_base_url")?
        .or(slot::read_config_string(
            &slot_home.join("config.toml"),
            "chatgpt_base_url",
        )?)
        .unwrap_or_else(|| DEFAULT_CHATGPT_BASE_URL.to_string());
    Ok(payload::normalize_chatgpt_base_url(&raw))
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
