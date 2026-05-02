use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;

use super::TokenTotals;

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

pub(super) fn parse_token_count_line(line: &str) -> Option<TokenTotals> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
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
