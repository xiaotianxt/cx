use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use crate::cache::entries;
use crate::cache::CacheStore;
use crate::paths::ManagerPaths;
use crate::usage::compare_for_selection;
use crate::usage::SlotResult;
use crate::usage::SlotStatus;
use crate::usage::UsageChecker;

pub const DEFAULT_SLOT_QUERY_JOBS: usize = 8;
pub const DEFAULT_SLOT_QUERY_RETRIES: usize = 1;

const USAGE_CACHE_SCHEMA_VERSION: u64 = 1;
const USAGE_CACHE_TTL_SECONDS: i64 = 30;
const USAGE_STALE_TTL_SECONDS: i64 = 10 * 60;
const RATE_STATE_SCHEMA_VERSION: u64 = 1;
const DEFAULT_START_INTERVAL_MS: u64 = 35;
const MIN_START_INTERVAL_MS: u64 = 15;
const MAX_START_INTERVAL_MS: u64 = 750;
const SUCCESS_RECOVERY_WINDOW: u64 = 4;
const SUCCESS_INTERVAL_STEP_MS: u64 = 20;
const DEFAULT_THROTTLE_COOLDOWN_SECONDS: i64 = 1;
const MAX_THROTTLE_COOLDOWN_SECONDS: i64 = 30;
const RATE_STATE_IDLE_RESET_SECONDS: i64 = 15 * 60;
const RETRY_BACKOFF_BASE_MS: u64 = 250;
const RETRY_BACKOFF_CAP_MS: u64 = 2_000;

pub trait SlotQueryProgress {
    fn started(&mut self, _total: usize) {}
    fn slot_checked(&mut self, _result: &SlotResult) {}
    fn retry_started(&mut self, _attempt: usize, _total_attempts: usize, _total: usize) {}
    fn finished(&mut self) {}
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlotQueryOptions {
    pub timeout: f32,
    pub jobs: usize,
    pub retries: usize,
}

impl SlotQueryOptions {
    pub fn new(timeout: f32, jobs: usize, retries: usize) -> Self {
        Self {
            timeout,
            jobs: jobs.max(1),
            retries,
        }
    }
}

pub fn query_slots_with_progress<P: SlotQueryProgress>(
    paths: &ManagerPaths,
    slots: &[String],
    options: SlotQueryOptions,
    progress: &mut P,
) -> Result<Vec<SlotResult>> {
    if slots.is_empty() {
        return Ok(Vec::new());
    }

    progress.started(slots.len());
    let checker = UsageChecker::new(options.timeout)?;
    let cache = UsageSlotCache::new(paths);
    let mut rate_state = UsageRateState::load(paths);
    let indexed_slots = slots
        .iter()
        .enumerate()
        .map(|(index, slot)| (index, slot.clone()))
        .collect::<Vec<_>>();
    let mut results = {
        let query_context = LiveQueryContext {
            paths,
            checker: &checker,
            cache: &cache,
            rate_state: &rate_state,
        };
        query_indexed_slots(&query_context, &indexed_slots, options.jobs, None, progress)
    };
    rate_state.record_results(&results);
    results.sort_by_key(|result| result.index);

    for attempt in 0..options.retries {
        let retry_slots = results
            .iter()
            .filter(|result| result.is_retryable_transient())
            .map(|result| (result.index, result.slot.clone()))
            .collect::<Vec<_>>();
        if retry_slots.is_empty() {
            break;
        }

        progress.retry_started(attempt + 1, options.retries, retry_slots.len());
        let retry_results = {
            let query_context = LiveQueryContext {
                paths,
                checker: &checker,
                cache: &cache,
                rate_state: &rate_state,
            };
            query_indexed_slots(
                &query_context,
                &retry_slots,
                options.jobs,
                Some(attempt),
                progress,
            )
        };
        rate_state.record_results(&retry_results);
        for result in retry_results {
            let index = result.index;
            if index < results.len() {
                results[index] = result;
            }
        }
    }

    let _ignored = rate_state.save(paths);
    results.sort_by_key(|result| result.index);
    progress.finished();
    Ok(results)
}

struct LiveQueryContext<'a> {
    paths: &'a ManagerPaths,
    checker: &'a UsageChecker,
    cache: &'a UsageSlotCache,
    rate_state: &'a UsageRateState,
}

fn query_indexed_slots(
    context: &LiveQueryContext<'_>,
    indexed_slots: &[(usize, String)],
    jobs: usize,
    retry_attempt: Option<usize>,
    progress: &mut impl SlotQueryProgress,
) -> Vec<SlotResult> {
    if indexed_slots.is_empty() {
        return Vec::new();
    }

    let jobs = jobs.max(1).min(indexed_slots.len());
    let next_index = Arc::new(AtomicUsize::new(0));
    let throttled = Arc::new(AtomicBool::new(false));
    let pacer = Arc::new(RequestPacer::new(context.rate_state.start_interval()));
    let (tx, rx) = mpsc::channel::<SlotResult>();
    let mut results = Vec::with_capacity(indexed_slots.len());

    std::thread::scope(|scope| {
        for _ in 0..jobs {
            let next_index = Arc::clone(&next_index);
            let throttled = Arc::clone(&throttled);
            let pacer = Arc::clone(&pacer);
            let tx = tx.clone();
            scope.spawn(move || loop {
                let item_index = next_index.fetch_add(1, Ordering::Relaxed);
                let Some((index, slot)) = indexed_slots.get(item_index) else {
                    break;
                };
                let result =
                    query_indexed_slot(context, &pacer, &throttled, *index, slot, retry_attempt);
                if result_observed_throttle(&result) {
                    throttled.store(true, Ordering::Relaxed);
                }
                if tx.send(result).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        for result in rx {
            progress.slot_checked(&result);
            if result_observed_throttle(&result) {
                throttled.store(true, Ordering::Relaxed);
            }
            results.push(result);
        }
    });
    results
}

fn query_indexed_slot(
    context: &LiveQueryContext<'_>,
    pacer: &RequestPacer,
    throttled: &AtomicBool,
    index: usize,
    slot: &str,
    retry_attempt: Option<usize>,
) -> SlotResult {
    if retry_attempt.is_none() {
        if let Some(result) = context.cache.fresh(slot, index) {
            return result;
        }
    }

    if throttled.load(Ordering::Relaxed) {
        return context.cache.stale_or_rate_limited(slot, index, None);
    }
    if let Some(retry_after_seconds) = context.rate_state.cooldown_remaining() {
        return context
            .cache
            .stale_or_cooldown(slot, index, retry_after_seconds);
    }

    if let Some(attempt) = retry_attempt {
        std::thread::sleep(retry_delay(slot, attempt));
    }
    pacer.wait();
    let result = context.checker.query_slot(context.paths, slot, index);
    if result.status == SlotStatus::RateLimited {
        return context
            .cache
            .stale_or_rate_limited(slot, index, result.retry_after_seconds);
    }
    if result.is_retryable_transient() {
        return context.cache.stale_or_refresh_error(slot, index, result);
    }
    if result.is_cacheable_usage() {
        let _ignored = context.cache.write(slot, &result);
    }
    result
}

fn retry_delay(slot: &str, attempt: usize) -> Duration {
    let shift = attempt.min(8) as u32;
    let cap = RETRY_BACKOFF_BASE_MS
        .saturating_mul(2_u64.saturating_pow(shift))
        .min(RETRY_BACKOFF_CAP_MS);
    Duration::from_millis(jitter_millis(slot, attempt, cap))
}

#[derive(Debug, Clone)]
struct UsageSlotCache {
    store: CacheStore,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CachedUsageSlot {
    #[serde(rename = "schemaVersion")]
    schema_version: u64,
    #[serde(rename = "fetchedAt")]
    fetched_at: i64,
    result: SlotResult,
}

impl UsageSlotCache {
    fn new(paths: &ManagerPaths) -> Self {
        Self {
            store: CacheStore::new(paths),
        }
    }

    fn fresh(&self, slot: &str, index: usize) -> Option<SlotResult> {
        self.cached_result(slot, index, USAGE_CACHE_TTL_SECONDS, false, None, None)
    }

    fn stale_or_rate_limited(
        &self,
        slot: &str,
        index: usize,
        retry_after_seconds: Option<i64>,
    ) -> SlotResult {
        self.cached_result(
            slot,
            index,
            USAGE_STALE_TTL_SECONDS,
            true,
            Some("rate_limited".to_string()),
            retry_after_seconds,
        )
        .unwrap_or_else(|| {
            let summary = match retry_after_seconds {
                Some(seconds) => format!("usage refresh rate limited; retry after {seconds}s"),
                None => "usage refresh rate limited".to_string(),
            };
            SlotResult::new(slot, index, SlotStatus::RateLimited, -1.0, summary)
                .with_retry_after_seconds(retry_after_seconds)
        })
    }

    fn stale_or_cooldown(&self, slot: &str, index: usize, retry_after_seconds: i64) -> SlotResult {
        self.cached_result(
            slot,
            index,
            USAGE_STALE_TTL_SECONDS,
            true,
            Some("cooldown".to_string()),
            Some(retry_after_seconds),
        )
        .unwrap_or_else(|| {
            SlotResult::new(
                slot,
                index,
                SlotStatus::RateLimited,
                -1.0,
                format!("usage refresh paused; retry after {retry_after_seconds}s"),
            )
            .with_retry_after_seconds(Some(retry_after_seconds))
            .with_refresh_status("cooldown")
        })
    }

    fn stale_or_refresh_error(
        &self,
        slot: &str,
        index: usize,
        refresh_error: SlotResult,
    ) -> SlotResult {
        self.cached_result(
            slot,
            index,
            USAGE_STALE_TTL_SECONDS,
            true,
            Some(refresh_error.status.as_str().to_string()),
            None,
        )
        .unwrap_or(refresh_error)
    }

    fn write(&self, slot: &str, result: &SlotResult) -> Result<()> {
        let cache = CachedUsageSlot {
            schema_version: USAGE_CACHE_SCHEMA_VERSION,
            fetched_at: unix_now(),
            result: result.clone().for_cache(),
        };
        self.store.write_json(slot_cache_relative(slot), &cache)?;
        Ok(())
    }

    fn cached_result(
        &self,
        slot: &str,
        index: usize,
        max_age_seconds: i64,
        stale: bool,
        refresh_status: Option<String>,
        retry_after_seconds: Option<i64>,
    ) -> Option<SlotResult> {
        let now = unix_now();
        let cache = self
            .store
            .read_json(slot_cache_relative(slot), |cache: &CachedUsageSlot| {
                cache.schema_version == USAGE_CACHE_SCHEMA_VERSION
            })?;
        let age_seconds = now.saturating_sub(cache.fetched_at);
        if age_seconds > max_age_seconds {
            return None;
        }
        Some(cache.result.mark_cached(
            index,
            age_seconds,
            stale,
            refresh_status,
            retry_after_seconds,
        ))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct UsageRateState {
    #[serde(rename = "schemaVersion")]
    schema_version: u64,
    #[serde(rename = "updatedAt")]
    updated_at: i64,
    #[serde(rename = "startIntervalMs")]
    start_interval_ms: u64,
    #[serde(rename = "throttledUntil")]
    throttled_until: Option<i64>,
    #[serde(rename = "throttleCount")]
    throttle_count: u64,
    #[serde(rename = "successfulRefreshes")]
    successful_refreshes: u64,
}

impl Default for UsageRateState {
    fn default() -> Self {
        Self {
            schema_version: RATE_STATE_SCHEMA_VERSION,
            updated_at: unix_now(),
            start_interval_ms: DEFAULT_START_INTERVAL_MS,
            throttled_until: None,
            throttle_count: 0,
            successful_refreshes: 0,
        }
    }
}

impl UsageRateState {
    fn load(paths: &ManagerPaths) -> Self {
        let mut state = CacheStore::new(paths)
            .read_json(entries::USAGE_RATE_STATE, |state: &UsageRateState| {
                state.schema_version == RATE_STATE_SCHEMA_VERSION
            })
            .unwrap_or_default();
        state.normalize(unix_now());
        state
    }

    fn save(&self, paths: &ManagerPaths) -> Result<()> {
        CacheStore::new(paths)
            .write_json(entries::USAGE_RATE_STATE, self)
            .map(|_| ())
    }

    fn start_interval(&self) -> Duration {
        Duration::from_millis(
            self.start_interval_ms
                .clamp(MIN_START_INTERVAL_MS, MAX_START_INTERVAL_MS),
        )
    }

    fn cooldown_remaining(&self) -> Option<i64> {
        let remaining = self.throttled_until?.saturating_sub(unix_now());
        (remaining > 0).then_some(remaining)
    }

    fn record_results(&mut self, results: &[SlotResult]) {
        let throttled_retry_after = results
            .iter()
            .filter(|result| result_observed_throttle(result))
            .filter_map(|result| result.retry_after_seconds)
            .max();
        let saw_throttle = results.iter().any(result_observed_throttle);
        let now = unix_now();
        if saw_throttle {
            self.throttle_count = self.throttle_count.saturating_add(1);
            self.successful_refreshes = 0;
            self.start_interval_ms = self
                .start_interval_ms
                .max(DEFAULT_START_INTERVAL_MS)
                .saturating_mul(2)
                .min(MAX_START_INTERVAL_MS);
            let throttle_exponent = (self.throttle_count.min(5) - 1) as u32;
            let fallback = DEFAULT_THROTTLE_COOLDOWN_SECONDS
                .saturating_mul(2_i64.saturating_pow(throttle_exponent))
                .min(MAX_THROTTLE_COOLDOWN_SECONDS);
            let cooldown_seconds = throttled_retry_after.unwrap_or(fallback).max(1);
            self.throttled_until = Some(now.saturating_add(cooldown_seconds));
            self.updated_at = now;
            return;
        }

        let live_results = results
            .iter()
            .filter(|result| result.cache_age_seconds.is_none())
            .collect::<Vec<_>>();
        if live_results.is_empty() {
            return;
        }

        self.throttled_until = None;
        self.throttle_count = 0;
        if live_results.iter().all(|result| !result.is_transient()) {
            self.successful_refreshes = self
                .successful_refreshes
                .saturating_add(live_results.len() as u64);
            while self.successful_refreshes >= SUCCESS_RECOVERY_WINDOW
                && self.start_interval_ms > MIN_START_INTERVAL_MS
            {
                self.start_interval_ms = self
                    .start_interval_ms
                    .saturating_sub(SUCCESS_INTERVAL_STEP_MS)
                    .max(MIN_START_INTERVAL_MS);
                self.successful_refreshes -= SUCCESS_RECOVERY_WINDOW;
            }
        }
        self.updated_at = now;
    }

    fn normalize(&mut self, now: i64) {
        self.start_interval_ms = self
            .start_interval_ms
            .clamp(MIN_START_INTERVAL_MS, MAX_START_INTERVAL_MS);
        if self.throttled_until.is_some_and(|until| until <= now) {
            self.throttled_until = None;
        }
        if self.throttled_until.is_none() && self.throttle_count == 0 {
            self.start_interval_ms = self.start_interval_ms.min(DEFAULT_START_INTERVAL_MS);
        }
        if self.throttled_until.is_none()
            && now.saturating_sub(self.updated_at) > RATE_STATE_IDLE_RESET_SECONDS
        {
            self.start_interval_ms = DEFAULT_START_INTERVAL_MS;
            self.throttle_count = 0;
            self.successful_refreshes = 0;
        }
        self.schema_version = RATE_STATE_SCHEMA_VERSION;
    }
}

#[derive(Debug)]
struct RequestPacer {
    next_start: Mutex<Instant>,
    interval: Duration,
}

impl RequestPacer {
    fn new(interval: Duration) -> Self {
        Self {
            next_start: Mutex::new(Instant::now()),
            interval,
        }
    }

    fn wait(&self) {
        let sleep_for = {
            let mut next_start = self.next_start.lock().expect("request pacer lock poisoned");
            let now = Instant::now();
            let start_at = (*next_start).max(now);
            *next_start = start_at + self.interval;
            start_at.saturating_duration_since(now)
        };
        if !sleep_for.is_zero() {
            std::thread::sleep(sleep_for);
        }
    }
}

fn slot_cache_relative(slot: &str) -> PathBuf {
    PathBuf::from(entries::USAGE_SLOT_CACHE_DIR).join(format!("{slot}.json"))
}

fn result_observed_throttle(result: &SlotResult) -> bool {
    result.refresh_status.as_deref() == Some("rate_limited")
        || (result.status == SlotStatus::RateLimited
            && result.refresh_status.as_deref() != Some("cooldown"))
}

fn jitter_millis(slot: &str, attempt: usize, cap_millis: u64) -> u64 {
    if cap_millis == 0 {
        return 0;
    }
    let mut hasher = DefaultHasher::new();
    slot.hash(&mut hasher);
    attempt.hash(&mut hasher);
    unix_now().hash(&mut hasher);
    (hasher.finish() % (cap_millis + 1)).min(cap_millis)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn choose_result(results: &[SlotResult]) -> Option<&SlotResult> {
    choose_result_excluding(results, &BTreeSet::new())
}

pub fn choose_result_excluding<'a>(
    results: &'a [SlotResult],
    excluded_slots: &BTreeSet<String>,
) -> Option<&'a SlotResult> {
    let mut available = results
        .iter()
        .filter(|result| !excluded_slots.contains(&result.slot))
        .filter(|result| result.is_available())
        .collect::<Vec<_>>();
    if !available.is_empty() {
        available.sort_by(|left, right| compare_for_selection(left, right));
        return available.first().copied();
    }

    let mut transient = results
        .iter()
        .filter(|result| !excluded_slots.contains(&result.slot))
        .filter(|result| result.is_transient())
        .collect::<Vec<_>>();
    transient.sort_by_key(|result| result.index);
    transient.first().copied()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::usage::SlotStatus;

    use super::*;

    fn temp_paths(name: &str) -> ManagerPaths {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "cx-selector-test-{name}-{}-{unique}",
            std::process::id()
        ));
        ManagerPaths::from_roots(root.join("codex"), root.join("profile-manager"))
    }

    #[test]
    fn chooses_highest_remaining_slot() {
        let results = vec![
            SlotResult::new("busy", 0, SlotStatus::Available, 20.0, "busy"),
            SlotResult::new("fresh", 1, SlotStatus::Available, 90.0, "fresh"),
            SlotResult::new("done", 2, SlotStatus::Exhausted, 100.0, "done"),
        ];

        assert_eq!(
            choose_result(&results).map(|result| result.slot.as_str()),
            Some("fresh")
        );
    }

    #[test]
    fn falls_back_to_transient_when_every_live_slot_failed_to_check() {
        let results = vec![
            SlotResult::new("bad-auth", 0, SlotStatus::NeedsLogin, -1.0, "bad"),
            SlotResult::new("network", 1, SlotStatus::Error, -1.0, "offline"),
        ];

        assert_eq!(
            choose_result(&results).map(|result| result.slot.as_str()),
            Some("network")
        );
    }

    #[test]
    fn excludes_current_and_cooldown_slots() {
        let results = vec![
            SlotResult::new("current", 0, SlotStatus::Available, 100.0, "current"),
            SlotResult::new("cooldown", 1, SlotStatus::Available, 90.0, "cooldown"),
            SlotResult::new("next", 2, SlotStatus::Available, 80.0, "next"),
        ];
        let excluded = BTreeSet::from([String::from("current"), String::from("cooldown")]);

        assert_eq!(
            choose_result_excluding(&results, &excluded).map(|result| result.slot.as_str()),
            Some("next")
        );
    }

    #[test]
    fn excludes_transient_slots_too() {
        let results = vec![
            SlotResult::new("current", 0, SlotStatus::Error, -1.0, "offline"),
            SlotResult::new("network", 1, SlotStatus::Error, -1.0, "offline"),
        ];
        let excluded = BTreeSet::from([String::from("current")]);

        assert_eq!(
            choose_result_excluding(&results, &excluded).map(|result| result.slot.as_str()),
            Some("network")
        );
    }

    #[test]
    fn query_options_clamp_jobs_to_one() {
        assert_eq!(SlotQueryOptions::new(2.0, 0, 1).jobs, 1);
    }

    #[test]
    fn retry_delay_uses_full_jitter_with_attempt_cap() {
        assert!(retry_delay("bus1", 0) <= Duration::from_millis(RETRY_BACKOFF_BASE_MS));
        assert!(retry_delay("bus1", 8) <= Duration::from_millis(RETRY_BACKOFF_CAP_MS));
    }

    #[test]
    fn usage_cache_is_per_slot_and_marks_fresh_results() {
        let paths = temp_paths("usage-cache-fresh");
        let cache = UsageSlotCache::new(&paths);
        let result = SlotResult::new("bus1", 7, SlotStatus::Available, 75.0, "fresh");

        cache.write("bus1", &result).unwrap();

        let cached = cache.fresh("bus1", 3).expect("fresh slot cache");
        assert_eq!(cached.slot, "bus1");
        assert_eq!(cached.index, 3);
        assert_eq!(cached.status, SlotStatus::Available);
        assert_eq!(cached.cache_age_seconds, Some(0));
        assert!(!cached.stale);
        assert!(cache.fresh("bus2", 4).is_none());

        let _ = fs::remove_dir_all(paths.base_codex_home);
    }

    #[test]
    fn usage_cache_can_fallback_to_stale_on_rate_limit() {
        let paths = temp_paths("usage-cache-stale");
        let cache = UsageSlotCache::new(&paths);
        let result = SlotResult::new("bus1", 0, SlotStatus::Available, 75.0, "cached");
        cache.write("bus1", &result).unwrap();

        let stale = cache.stale_or_rate_limited("bus1", 2, Some(4));

        assert_eq!(stale.status, SlotStatus::Available);
        assert_eq!(stale.index, 2);
        assert!(stale.stale);
        assert_eq!(stale.refresh_status.as_deref(), Some("rate_limited"));
        assert_eq!(stale.retry_after_seconds, Some(4));

        let _ = fs::remove_dir_all(paths.base_codex_home);
    }

    #[test]
    fn usage_cache_can_fallback_to_stale_on_transient_error() {
        let paths = temp_paths("usage-cache-transient");
        let cache = UsageSlotCache::new(&paths);
        let result = SlotResult::new("bus1", 0, SlotStatus::Available, 75.0, "cached");
        cache.write("bus1", &result).unwrap();

        let error = SlotResult::new("bus1", 2, SlotStatus::Error, -1.0, "offline");
        let stale = cache.stale_or_refresh_error("bus1", 2, error);

        assert_eq!(stale.status, SlotStatus::Available);
        assert!(stale.stale);
        assert_eq!(stale.refresh_status.as_deref(), Some("error"));

        let _ = fs::remove_dir_all(paths.base_codex_home);
    }

    #[test]
    fn rate_state_uses_multiplicative_decrease_and_additive_recovery() {
        let mut state = UsageRateState::default();
        let throttled = SlotResult::new("bus1", 0, SlotStatus::RateLimited, -1.0, "limited")
            .with_retry_after_seconds(Some(3));

        state.record_results(&[throttled]);

        assert_eq!(state.start_interval_ms, DEFAULT_START_INTERVAL_MS * 2);
        assert!(matches!(state.cooldown_remaining(), Some(1..=3)));

        state.throttled_until = None;
        let successes = (0..SUCCESS_RECOVERY_WINDOW)
            .map(|index| {
                SlotResult::new(
                    &format!("bus{index}"),
                    index as usize,
                    SlotStatus::Available,
                    90.0,
                    "ok",
                )
            })
            .collect::<Vec<_>>();
        state.record_results(&successes);

        assert_eq!(
            state.start_interval_ms,
            DEFAULT_START_INTERVAL_MS * 2 - SUCCESS_INTERVAL_STEP_MS
        );
        assert_eq!(state.throttle_count, 0);
    }

    #[test]
    fn rate_state_idle_reset_prevents_permanent_slowdown() {
        let mut state = UsageRateState {
            start_interval_ms: MAX_START_INTERVAL_MS,
            updated_at: unix_now() - RATE_STATE_IDLE_RESET_SECONDS - 1,
            throttle_count: 4,
            successful_refreshes: 3,
            ..UsageRateState::default()
        };

        state.normalize(unix_now());

        assert_eq!(state.start_interval_ms, DEFAULT_START_INTERVAL_MS);
        assert_eq!(state.throttle_count, 0);
        assert_eq!(state.successful_refreshes, 0);
    }

    #[test]
    fn rate_state_without_throttle_debt_uses_aggressive_default() {
        let mut state = UsageRateState {
            start_interval_ms: DEFAULT_START_INTERVAL_MS * 3,
            updated_at: unix_now(),
            throttle_count: 0,
            throttled_until: None,
            ..UsageRateState::default()
        };

        state.normalize(unix_now());

        assert_eq!(state.start_interval_ms, DEFAULT_START_INTERVAL_MS);
    }

    #[test]
    fn rate_state_preserves_recent_throttle_slowdown() {
        let mut state = UsageRateState {
            start_interval_ms: DEFAULT_START_INTERVAL_MS * 3,
            updated_at: unix_now(),
            throttle_count: 1,
            throttled_until: None,
            ..UsageRateState::default()
        };

        state.normalize(unix_now());

        assert_eq!(state.start_interval_ms, DEFAULT_START_INTERVAL_MS * 3);
    }
}
