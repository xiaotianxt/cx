use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::Result;

use crate::paths::ManagerPaths;
use crate::usage::compare_for_selection;
use crate::usage::SlotResult;
use crate::usage::UsageChecker;

pub const DEFAULT_SLOT_QUERY_JOBS: usize = 4;
pub const DEFAULT_SLOT_QUERY_RETRIES: usize = 1;

pub trait SlotQueryProgress {
    fn started(&mut self, _total: usize) {}
    fn slot_checked(&mut self, _result: &SlotResult) {}
    fn retry_started(&mut self, _attempt: usize, _total_attempts: usize, _total: usize) {}
}

#[derive(Debug, Default)]
struct NoSlotQueryProgress;

impl SlotQueryProgress for NoSlotQueryProgress {}

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

pub fn query_slots(
    paths: &ManagerPaths,
    slots: &[String],
    options: SlotQueryOptions,
) -> Result<Vec<SlotResult>> {
    let mut progress = NoSlotQueryProgress;
    query_slots_with_progress(paths, slots, options, &mut progress)
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
    let indexed_slots = slots
        .iter()
        .enumerate()
        .map(|(index, slot)| (index, slot.clone()))
        .collect::<Vec<_>>();
    let mut results = query_indexed_slots(paths, &checker, &indexed_slots, options.jobs, progress);
    results.sort_by_key(|result| result.index);

    for attempt in 0..options.retries {
        let retry_slots = results
            .iter()
            .filter(|result| result.is_transient())
            .map(|result| (result.index, result.slot.clone()))
            .collect::<Vec<_>>();
        if retry_slots.is_empty() {
            break;
        }

        std::thread::sleep(retry_delay(attempt));
        progress.retry_started(attempt + 1, options.retries, retry_slots.len());
        let retry_results =
            query_indexed_slots(paths, &checker, &retry_slots, options.jobs, progress);
        for result in retry_results {
            let index = result.index;
            if index < results.len() {
                results[index] = result;
            }
        }
    }

    results.sort_by_key(|result| result.index);
    Ok(results)
}

fn query_indexed_slots(
    paths: &ManagerPaths,
    checker: &UsageChecker,
    indexed_slots: &[(usize, String)],
    jobs: usize,
    progress: &mut impl SlotQueryProgress,
) -> Vec<SlotResult> {
    let mut results = Vec::with_capacity(indexed_slots.len());
    for chunk in indexed_slots.chunks(jobs.max(1)) {
        let mut chunk_results = std::thread::scope(|scope| {
            let handles = chunk
                .iter()
                .map(|(index, slot)| {
                    let checker = checker.clone();
                    scope.spawn(move || checker.query_slot(paths, slot, *index))
                })
                .collect::<Vec<_>>();

            handles
                .into_iter()
                .map(|handle| handle.join().expect("slot query thread panicked"))
                .collect::<Vec<_>>()
        });
        for result in &chunk_results {
            progress.slot_checked(result);
        }
        results.append(&mut chunk_results);
    }
    results
}

fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(250 * (attempt as u64 + 1))
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
    use crate::usage::SlotStatus;

    use super::*;

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
    fn retry_delay_increases_by_attempt() {
        assert_eq!(retry_delay(0), Duration::from_millis(250));
        assert_eq!(retry_delay(1), Duration::from_millis(500));
    }
}
