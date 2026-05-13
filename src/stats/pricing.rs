use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde::Serialize;

use crate::cache::entries;
use crate::cache::CacheStore;
use crate::cli::StatsArgs;
use crate::paths::ManagerPaths;

use super::unix_now;
use super::TokenMix;
use super::TokenTotals;

pub(super) const DEFAULT_PRICE_URL: &str = "https://developers.openai.com/api/docs/pricing";
const PRICE_CACHE_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;
pub(super) const PRICE_CACHE_SCHEMA_VERSION: u64 = 2;

#[derive(Debug, Clone)]
pub(super) struct PriceBook {
    prices: BTreeMap<String, ModelPrice>,
    token_mix: TokenMix,
    pub(super) source: String,
    pub(super) note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StatsPricePolicy<'a> {
    Disabled,
    Enabled {
        price_url: &'a str,
        cache_policy: PriceCachePolicy,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PriceCachePolicy {
    UseCacheOrFallback,
    UseFreshCacheIfAvailable,
    Refresh,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PriceCache {
    #[serde(rename = "schemaVersion")]
    pub(super) schema_version: u64,
    #[serde(rename = "fetchedAt")]
    fetched_at: i64,
    #[serde(rename = "sourceUrl")]
    source_url: String,
    prices: BTreeMap<String, ModelPrice>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(super) struct ModelPrice {
    #[serde(rename = "inputPerMillion")]
    pub(super) input_per_million: f64,
    #[serde(rename = "cachedInputPerMillion")]
    pub(super) cached_input_per_million: Option<f64>,
    #[serde(rename = "outputPerMillion")]
    pub(super) output_per_million: f64,
}

impl<'a> StatsPricePolicy<'a> {
    pub(super) fn from_args(args: &'a StatsArgs) -> Self {
        if args.no_price {
            return Self::Disabled;
        }
        if args.refresh_prices {
            return Self::Enabled {
                price_url: args.price_url.as_deref().unwrap_or(DEFAULT_PRICE_URL),
                cache_policy: PriceCachePolicy::Refresh,
            };
        }
        if args.price || args.price_url.is_some() {
            return Self::Enabled {
                price_url: args.price_url.as_deref().unwrap_or(DEFAULT_PRICE_URL),
                cache_policy: PriceCachePolicy::UseFreshCacheIfAvailable,
            };
        }
        if !args.json {
            return Self::Enabled {
                price_url: DEFAULT_PRICE_URL,
                cache_policy: PriceCachePolicy::UseCacheOrFallback,
            };
        }
        Self::Disabled
    }
}

impl PriceCachePolicy {
    fn allows_cache_read(self) -> bool {
        matches!(
            self,
            Self::UseCacheOrFallback | Self::UseFreshCacheIfAvailable
        )
    }
}

impl PriceCache {
    fn new(source_url: String, prices: BTreeMap<String, ModelPrice>) -> Self {
        Self {
            schema_version: PRICE_CACHE_SCHEMA_VERSION,
            fetched_at: unix_now(),
            source_url,
            prices,
        }
    }
}

impl PriceBook {
    pub(super) fn estimate_cost(&self, provider: &str, model: &str, tokens: u64) -> Option<f64> {
        let price = self.model_price(provider, model)?;
        let cached_rate = price
            .cached_input_per_million
            .unwrap_or(price.input_per_million);
        let rate = (self.token_mix.uncached_input_share * price.input_per_million)
            + (self.token_mix.cached_input_share * cached_rate)
            + (self.token_mix.output_share * price.output_per_million);
        Some(tokens as f64 * rate / 1_000_000.0)
    }

    pub(super) fn estimate_token_totals_cost(
        &self,
        provider: &str,
        model: &str,
        totals: &TokenTotals,
    ) -> Option<f64> {
        let price = self.model_price(provider, model)?;
        let cached_rate = price
            .cached_input_per_million
            .unwrap_or(price.input_per_million);
        Some(
            ((totals.uncached_input_tokens as f64 * price.input_per_million)
                + (totals.cached_input_tokens as f64 * cached_rate)
                + (totals.output_tokens as f64 * price.output_per_million))
                / 1_000_000.0,
        )
    }

    fn model_price(&self, provider: &str, model: &str) -> Option<&ModelPrice> {
        if provider != "openai" {
            return None;
        }
        self.prices.get(&normalize_model_key(model))
    }
}

pub(super) fn load_price_book(
    paths: &ManagerPaths,
    price_url: &str,
    cache_policy: PriceCachePolicy,
    token_mix: TokenMix,
    token_mix_source: &str,
) -> PriceBook {
    if cache_policy.allows_cache_read() {
        if let Some(cache) = read_price_cache(paths) {
            if cache.source_url == price_url && !cache.prices.is_empty() {
                let fresh = unix_now().saturating_sub(cache.fetched_at) < PRICE_CACHE_TTL_SECONDS;
                if fresh || matches!(cache_policy, PriceCachePolicy::UseCacheOrFallback) {
                    return PriceBook {
                        prices: cache.prices,
                        token_mix,
                        source: format!("cache: {price_url}"),
                        note: price_estimate_note(token_mix, token_mix_source),
                    };
                }
            }
        }
        if matches!(cache_policy, PriceCachePolicy::UseCacheOrFallback) {
            return fallback_price_book(
                token_mix,
                token_mix_source,
                "no fresh price cache; use --refresh-prices to update",
            );
        }
    }

    match fetch_prices(price_url) {
        Ok(prices) if !prices.is_empty() => {
            let cache = PriceCache::new(price_url.to_string(), prices.clone());
            if let Err(_err) = write_price_cache(paths, &cache) {
                // Price estimates can use freshly fetched prices even when cache persistence fails.
            }
            PriceBook {
                prices,
                token_mix,
                source: price_url.to_string(),
                note: price_estimate_note(token_mix, token_mix_source),
            }
        }
        Ok(_) => fallback_price_book(
            token_mix,
            token_mix_source,
            "pricing page had no parseable model rows",
        ),
        Err(err) => fallback_price_book(token_mix, token_mix_source, &format!("{err:#}")),
    }
}

fn fetch_prices(price_url: &str) -> Result<BTreeMap<String, ModelPrice>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(3))
        .user_agent("cx")
        .build()?;
    let body = client
        .get(price_url)
        .send()
        .with_context(|| format!("fetch {price_url}"))?
        .error_for_status()
        .with_context(|| format!("fetch {price_url}"))?
        .text()
        .with_context(|| format!("read {price_url}"))?;
    Ok(parse_pricing_page(&body))
}

pub(super) fn read_price_cache(paths: &ManagerPaths) -> Option<PriceCache> {
    CacheStore::new(paths).read_json(entries::PRICE_CACHE, |cache: &PriceCache| {
        cache.schema_version == PRICE_CACHE_SCHEMA_VERSION
    })
}

fn write_price_cache(paths: &ManagerPaths, cache: &PriceCache) -> Result<()> {
    CacheStore::new(paths)
        .write_json(entries::PRICE_CACHE, cache)
        .map(|_| ())
}

#[cfg(test)]
pub(super) fn parse_price_cache(content: &str) -> Option<PriceCache> {
    let cache = serde_json::from_str::<PriceCache>(content).ok()?;
    (cache.schema_version == PRICE_CACHE_SCHEMA_VERSION).then_some(cache)
}

fn fallback_price_book(token_mix: TokenMix, token_mix_source: &str, reason: &str) -> PriceBook {
    PriceBook {
        prices: fallback_prices(),
        token_mix,
        source: "built-in fallback".to_string(),
        note: format!(
            "{}; pricing fetch fallback reason: {reason}",
            price_estimate_note(token_mix, token_mix_source)
        ),
    }
}

fn price_estimate_note(token_mix: TokenMix, token_mix_source: &str) -> String {
    format!(
        "estimate uses exact rollout token breakdown when available; fallback uses {:.2}% uncached input, {:.2}% cached input, {:.2}% output from {token_mix_source}; Codex state stores total tokens only",
        token_mix.uncached_input_share * 100.0,
        token_mix.cached_input_share * 100.0,
        token_mix.output_share * 100.0
    )
}

pub(super) fn parse_pricing_page(body: &str) -> BTreeMap<String, ModelPrice> {
    let decoded = decode_html(body);
    let mut prices = BTreeMap::new();
    let marker = "[[0,\"";
    let mut offset = 0;
    while let Some(relative) = decoded[offset..].find(marker) {
        let start = offset + relative + marker.len();
        let Some(end_relative) = decoded[start..].find('"') else {
            break;
        };
        let end = start + end_relative;
        let raw_model = &decoded[start..end];
        let after_end = (end + 240).min(decoded.len());
        if let Some(model) = normalize_price_model_name(raw_model) {
            if let Some(price) = parse_price_values(&decoded[end..after_end]) {
                prices.entry(model).or_insert(price);
            }
        }
        offset = end + 1;
    }
    prices
}

fn parse_price_values(text: &str) -> Option<ModelPrice> {
    let mut values = Vec::new();
    let mut offset = 0;
    while values.len() < 3 {
        let Some(relative) = text[offset..].find("[0,") else {
            break;
        };
        let start = offset + relative + 3;
        let Some(end_relative) = text[start..].find(']') else {
            break;
        };
        let end = start + end_relative;
        let raw = text[start..end].trim().trim_matches('"');
        values.push(raw.parse::<f64>().ok());
        offset = end + 1;
    }

    if values.len() < 3 {
        return None;
    }
    Some(ModelPrice {
        input_per_million: values[0]?,
        cached_input_per_million: values[1],
        output_per_million: values[2]?,
    })
}

fn normalize_price_model_name(raw: &str) -> Option<String> {
    let model = normalize_model_key(raw.split(" (").next().unwrap_or(raw).trim());
    if model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
        || model.starts_with("computer-use")
    {
        Some(model)
    } else {
        None
    }
}

fn normalize_model_key(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

fn decode_html(input: &str) -> String {
    input
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn fallback_prices() -> BTreeMap<String, ModelPrice> {
    [
        ("gpt-5.5", 5.0, Some(0.5), 30.0),
        ("gpt-5.4", 2.5, Some(0.25), 15.0),
        ("gpt-5.4-mini", 0.75, Some(0.075), 4.5),
        ("gpt-5.4-nano", 0.2, Some(0.02), 1.25),
        ("gpt-5.2", 1.75, Some(0.175), 14.0),
        ("gpt-5.1", 1.25, Some(0.125), 10.0),
        ("gpt-5", 1.25, Some(0.125), 10.0),
        ("gpt-5-mini", 0.25, Some(0.025), 2.0),
        ("gpt-5.3-codex", 1.75, Some(0.175), 14.0),
        ("gpt-5.2-codex", 3.5, Some(0.35), 28.0),
        ("gpt-5.1-codex-max", 2.5, Some(0.25), 20.0),
        ("gpt-5.1-codex", 2.5, Some(0.25), 20.0),
        ("gpt-5-codex", 2.5, Some(0.25), 20.0),
    ]
    .into_iter()
    .map(
        |(model, input_per_million, cached_input_per_million, output_per_million)| {
            (
                model.to_string(),
                ModelPrice {
                    input_per_million,
                    cached_input_per_million,
                    output_per_million,
                },
            )
        },
    )
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_price_book() -> PriceBook {
        PriceBook {
            prices: [(
                "gpt-test".to_string(),
                ModelPrice {
                    input_per_million: 5.0,
                    cached_input_per_million: Some(0.5),
                    output_per_million: 30.0,
                },
            )]
            .into_iter()
            .collect(),
            token_mix: TokenMix {
                uncached_input_share: 0.1,
                cached_input_share: 0.8,
                output_share: 0.1,
            },
            source: "test".to_string(),
            note: "test".to_string(),
        }
    }

    #[test]
    fn exact_cost_uses_token_categories() {
        let book = test_price_book();
        let totals = TokenTotals {
            samples: 1,
            total_tokens: 100,
            uncached_input_tokens: 10,
            cached_input_tokens: 80,
            output_tokens: 10,
            reasoning_output_tokens: 0,
        };

        let cost = book
            .estimate_token_totals_cost("openai", "gpt-test", &totals)
            .expect("priced model");

        assert!((cost - 0.00039).abs() < f64::EPSILON);
    }
}
