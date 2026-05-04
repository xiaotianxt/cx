use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;

use super::TokenTotals;

#[derive(Debug, Clone)]
pub(super) struct TokenUsageEvent {
    pub(super) timestamp_unix: i64,
    pub(super) totals: TokenTotals,
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
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut previous = None;
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

    Ok(events)
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
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
        .min(input_tokens);
    let output_tokens = usage
        .get("output_tokens")
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
    })
}

fn parse_utc_timestamp(value: &str) -> Option<i64> {
    if value.len() < 20
        || value.as_bytes().get(4) != Some(&b'-')
        || value.as_bytes().get(7) != Some(&b'-')
        || value.as_bytes().get(10) != Some(&b'T')
        || value.as_bytes().get(13) != Some(&b':')
        || value.as_bytes().get(16) != Some(&b':')
    {
        return None;
    }
    let year = parse_digits(value, 0, 4)? as i64;
    let month = parse_digits(value, 5, 7)? as i64;
    let day = parse_digits(value, 8, 10)? as i64;
    let hour = parse_digits(value, 11, 13)? as i64;
    let minute = parse_digits(value, 14, 16)? as i64;
    let second = parse_digits(value, 17, 19)? as i64;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn parse_digits(value: &str, start: usize, end: usize) -> Option<u32> {
    value.get(start..end)?.parse().ok()
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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
}
