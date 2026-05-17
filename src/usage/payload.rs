use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;

use super::SlotResult;
use super::SlotStatus;

pub(super) fn result_from_payload(slot: &str, index: usize, payload: &Value) -> SlotResult {
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

    let minutes = seconds / 60;
    if minutes == 0 {
        return Some("in <1m".to_string());
    }
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

pub(super) fn normalize_chatgpt_base_url(raw_url: &str) -> String {
    let mut base_url = raw_url.trim().trim_end_matches('/').to_string();
    if (base_url.starts_with("https://chatgpt.com")
        || base_url.starts_with("https://chat.openai.com"))
        && !base_url.contains("/backend-api")
    {
        base_url.push_str("/backend-api");
    }
    base_url
}

pub(super) fn usage_url(base_url: &str) -> String {
    if base_url.contains("/backend-api") {
        format!("{base_url}/wham/usage")
    } else {
        format!("{base_url}/api/codex/usage")
    }
}
