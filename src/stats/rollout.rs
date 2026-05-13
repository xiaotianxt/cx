use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::TokenTotals;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct TokenUsageEvent {
    pub(super) timestamp_unix: i64,
    pub(super) totals: TokenTotals,
}

#[derive(Debug, Clone)]
pub(super) struct TokenUsageScan {
    pub(super) events: Vec<TokenUsageEvent>,
    pub(super) final_totals: Option<TokenTotals>,
}

pub(super) fn read_final_token_usage(path: &Path) -> Result<Option<TokenTotals>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut latest = None;
    for line in reader.lines() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        if !line.contains("\"token_count\"") {
            continue;
        }
        if let Some(usage) = parse_token_count_line(&line) {
            latest = Some(usage);
        }
    }
    Ok(latest)
}

pub(super) fn read_token_usage_events(path: &Path) -> Result<Vec<TokenUsageEvent>> {
    Ok(read_token_usage_scan_from(path, 0, None)?.events)
}

pub(super) fn read_token_usage_scan_from(
    path: &Path,
    offset: u64,
    previous_totals: Option<TokenTotals>,
) -> Result<TokenUsageScan> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))
            .with_context(|| format!("seek {}", path.display()))?;
    }
    let reader = BufReader::new(file);
    let mut previous = previous_totals;
    let mut events = Vec::new();

    for line in reader.lines() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        if !line.contains("\"token_count\"") {
            continue;
        }
        let Some(sample) = parse_token_count_sample(&line) else {
            continue;
        };
        let totals = previous.as_ref().map_or_else(
            || sample.totals.clone(),
            |last| sample.totals.delta_from(last),
        );
        previous = Some(sample.totals);
        if totals.total_tokens > 0 {
            events.push(TokenUsageEvent {
                timestamp_unix: sample.timestamp_unix,
                totals,
            });
        }
    }

    Ok(TokenUsageScan {
        events,
        final_totals: previous,
    })
}

pub(super) fn parse_token_count_line(line: &str) -> Option<TokenTotals> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    parse_token_totals(&value)
}

fn parse_token_count_sample(line: &str) -> Option<TokenUsageEvent> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let timestamp_unix = parse_utc_timestamp(value.get("timestamp")?.as_str()?)?;
    let totals = parse_token_totals(&value)?;

    Some(TokenUsageEvent {
        timestamp_unix,
        totals,
    })
}

fn parse_token_totals(value: &serde_json::Value) -> Option<TokenTotals> {
    let usage = value
        .get("payload")?
        .get("info")?
        .get("total_token_usage")?;
    let input_tokens = usage.get("input_tokens")?.as_u64()?;
    let cached_input_tokens = usage
        .get("cached_input_tokens")
        .or_else(|| usage.get("cache_read_input_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
        .min(input_tokens);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let reasoning_output_tokens = usage
        .get("reasoning_output_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(input_tokens + output_tokens);

    Some(TokenTotals {
        samples: 1,
        total_tokens,
        uncached_input_tokens: input_tokens - cached_input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_output_tokens,
    })
}

fn parse_utc_timestamp(value: &str) -> Option<i64> {
    OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .map(|timestamp| timestamp.unix_timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utc_timestamp() {
        assert_eq!(parse_utc_timestamp("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_utc_timestamp("2026-05-04T12:34:56.789Z"),
            Some(1_777_898_096)
        );
        assert_eq!(
            parse_utc_timestamp("2026-05-04T08:34:56.789-04:00"),
            Some(1_777_898_096)
        );
        assert_eq!(parse_utc_timestamp("2026-02-31T00:00:00Z"), None);
    }

    #[test]
    fn token_usage_events_skip_duplicate_totals() {
        let first = r#"{"timestamp":"1970-01-01T00:00:10.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"total_tokens":110}}}}"#;
        let duplicate = r#"{"timestamp":"1970-01-01T00:00:11.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"total_tokens":110}}}}"#;
        let next = r#"{"timestamp":"1970-01-01T00:00:12.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":150,"cached_input_tokens":120,"output_tokens":20,"total_tokens":170}}}}"#;

        let samples = [
            parse_token_count_sample(first).expect("first sample"),
            parse_token_count_sample(duplicate).expect("duplicate sample"),
            parse_token_count_sample(next).expect("next sample"),
        ];
        let first_delta = samples[0].totals.clone();
        let duplicate_delta = samples[1].totals.delta_from(&samples[0].totals);
        let next_delta = samples[2].totals.delta_from(&samples[1].totals);

        assert_eq!(first_delta.total_tokens, 110);
        assert_eq!(duplicate_delta.total_tokens, 0);
        assert_eq!(next_delta.total_tokens, 60);
        assert_eq!(next_delta.cached_input_tokens, 40);
    }

    #[test]
    fn token_usage_scan_from_offset_uses_previous_totals() {
        let first = r#"{"timestamp":"1970-01-01T00:00:10.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"total_tokens":110}}}}"#;
        let second = r#"{"timestamp":"1970-01-01T00:00:11.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":150,"cached_input_tokens":120,"output_tokens":20,"total_tokens":170}}}}"#;
        let path = std::env::temp_dir().join(format!(
            "cx-rollout-scan-{}-{}.jsonl",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let prefix = format!("{first}\n");
        std::fs::write(&path, format!("{prefix}{second}\n")).expect("write rollout");

        let previous = parse_token_count_line(first).expect("first totals");
        let scan = read_token_usage_scan_from(&path, prefix.len() as u64, Some(previous))
            .expect("scan appended rollout");

        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].totals.total_tokens, 60);
        assert_eq!(scan.events[0].totals.cached_input_tokens, 40);
        assert_eq!(scan.final_totals.unwrap().total_tokens, 170);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn token_usage_parses_cache_read_alias_and_reasoning() {
        let line = r#"{"timestamp":"1970-01-01T00:00:10.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cache_read_input_tokens":25,"output_tokens":10,"reasoning_output_tokens":4,"total_tokens":110}}}}"#;

        let usage = parse_token_count_line(line).expect("token_count line");

        assert_eq!(usage.uncached_input_tokens, 75);
        assert_eq!(usage.cached_input_tokens, 25);
        assert_eq!(usage.output_tokens, 10);
        assert_eq!(usage.reasoning_output_tokens, 4);
        assert_eq!(usage.total_tokens, 110);
    }
}
