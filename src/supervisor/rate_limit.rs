use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputLimitKind {
    Usage,
    Provider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputRateLimitSignal {
    pub(crate) kind: OutputLimitKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RolloutTurnState {
    user_message_seen: bool,
    assistant_message_seen: bool,
    side_effect_seen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RolloutRateLimitSignal {
    pub(crate) safe_to_continue: bool,
}

pub(crate) fn classify_output(text: &str) -> Option<OutputRateLimitSignal> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("too many requests")
        || lower.contains("quota exceeded")
        || lower.contains("server overloaded")
        || lower.contains("http 429")
        || lower.contains(" 429")
    {
        return Some(OutputRateLimitSignal {
            kind: OutputLimitKind::Provider,
        });
    }

    if lower.contains("usage limit")
        || lower.contains("rate limit")
        || lower.contains("limit reached")
        || lower.contains("you've hit your")
        || lower.contains("you have hit your")
    {
        return Some(OutputRateLimitSignal {
            kind: OutputLimitKind::Usage,
        });
    }

    None
}

pub(crate) fn inspect_rollout_fragment(
    fragment: &str,
    state: &mut RolloutTurnState,
) -> Option<RolloutRateLimitSignal> {
    let mut signal = None;
    for line in fragment.lines() {
        inspect_rollout_line(line, state, &mut signal);
    }
    signal
}

fn inspect_rollout_line(
    line: &str,
    state: &mut RolloutTurnState,
    signal: &mut Option<RolloutRateLimitSignal>,
) {
    let parsed = serde_json::from_str::<Value>(line).ok();
    let payload_type = parsed
        .as_ref()
        .and_then(|value| value.pointer("/payload/type"))
        .and_then(Value::as_str);
    let line_lower = line.to_ascii_lowercase();

    if matches!(payload_type, Some("user_message")) || line_lower.contains("\"user_message\"") {
        *state = RolloutTurnState {
            user_message_seen: true,
            ..RolloutTurnState::default()
        };
    }

    if matches!(payload_type, Some("turn_started")) || line_lower.contains("\"turn_started\"") {
        state.assistant_message_seen = false;
        state.side_effect_seen = false;
    }

    if matches!(
        payload_type,
        Some("agent_message" | "assistant_message" | "message")
    ) || line_lower.contains("\"agent_message\"")
        || line_lower.contains("\"assistant_message\"")
    {
        state.assistant_message_seen = true;
    }

    if has_side_effect_marker(payload_type, &line_lower) {
        state.side_effect_seen = true;
    }

    let reached = parsed
        .as_ref()
        .is_some_and(json_contains_rate_limit_reached)
        || fallback_line_mentions_rate_limit(&line_lower);
    if reached {
        *signal = Some(RolloutRateLimitSignal {
            safe_to_continue: state.user_message_seen
                && !state.assistant_message_seen
                && !state.side_effect_seen,
        });
    }
}

fn has_side_effect_marker(payload_type: Option<&str>, line_lower: &str) -> bool {
    matches!(
        payload_type,
        Some(
            "exec_command_begin"
                | "exec_command"
                | "patch_apply_begin"
                | "apply_patch"
                | "mcp_tool_call_begin"
                | "mcp_tool_call"
                | "web_search_begin"
                | "dynamic_tool_call"
                | "image_generation_begin"
        )
    ) || line_lower.contains("exec_command")
        || line_lower.contains("patch_apply")
        || line_lower.contains("apply_patch")
        || line_lower.contains("mcp_tool_call")
        || line_lower.contains("web_search")
        || line_lower.contains("dynamic_tool_call")
        || line_lower.contains("image_generation")
}

fn fallback_line_mentions_rate_limit(line_lower: &str) -> bool {
    line_lower.contains("\"rate_limits\"")
        && (line_lower.contains("\"rate_limit_reached_type\"")
            || line_lower.contains("\"limit_reached\":true")
            || line_lower.contains("\"allowed\":false")
            || line_lower.contains("\"used_percent\":100")
            || line_lower.contains("\"used_percent\":100.0"))
}

fn json_contains_rate_limit_reached(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            if map
                .get("rate_limit_reached_type")
                .is_some_and(non_empty_json_value)
            {
                return true;
            }
            if map.get("limit_reached").and_then(Value::as_bool) == Some(true) {
                return true;
            }
            if map.get("allowed").and_then(Value::as_bool) == Some(false) {
                return true;
            }
            if map
                .get("used_percent")
                .and_then(Value::as_f64)
                .is_some_and(|percent| percent >= 100.0)
            {
                return true;
            }
            map.values().any(json_contains_rate_limit_reached)
        }
        Value::Array(values) => values.iter().any(json_contains_rate_limit_reached),
        _ => false,
    }
}

fn non_empty_json_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_classifier_distinguishes_usage_and_provider_limits() {
        assert_eq!(
            classify_output("You've hit your usage limit").map(|signal| signal.kind),
            Some(OutputLimitKind::Usage)
        );
        assert_eq!(
            classify_output("HTTP 429 too many requests").map(|signal| signal.kind),
            Some(OutputLimitKind::Provider)
        );
    }

    #[test]
    fn rollout_rate_limit_without_side_effect_is_safe_to_continue() {
        let mut state = RolloutTurnState::default();
        let fragment = r#"{"payload":{"type":"user_message","message":"finish it"}}"#.to_string()
            + "\n"
            + r#"{"payload":{"type":"turn_started"}}"#
            + "\n"
            + r#"{"payload":{"type":"token_count","info":{"rate_limits":{"primary":{"used_percent":100.0}}}}}"#;

        let signal = inspect_rollout_fragment(&fragment, &mut state).unwrap();

        assert!(signal.safe_to_continue);
    }

    #[test]
    fn rollout_rate_limit_after_tool_side_effect_is_not_safe_to_continue() {
        let mut state = RolloutTurnState::default();
        let fragment = r#"{"payload":{"type":"user_message","message":"edit files"}}"#.to_string()
            + "\n"
            + r#"{"payload":{"type":"exec_command_begin"}}"#
            + "\n"
            + r#"{"payload":{"type":"token_count","info":{"rate_limits":{"primary":{"rate_limit_reached_type":"primary"}}}}}"#;

        let signal = inspect_rollout_fragment(&fragment, &mut state).unwrap();

        assert!(!signal.safe_to_continue);
    }
}
