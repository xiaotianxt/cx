use std::collections::BTreeSet;

use anyhow::Result;

use crate::paths::ManagerPaths;
use crate::usage::compare_for_selection;
use crate::usage::SlotResult;
use crate::usage::UsageChecker;

pub fn query_slots(
    paths: &ManagerPaths,
    slots: &[String],
    timeout: f32,
) -> Result<Vec<SlotResult>> {
    if slots.is_empty() {
        return Ok(Vec::new());
    }

    let checker = UsageChecker::new(timeout)?;
    let mut results = std::thread::scope(|scope| {
        let handles = slots
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                let checker = checker.clone();
                scope.spawn(move || checker.query_slot(paths, slot, index))
            })
            .collect::<Vec<_>>();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("slot query thread panicked"))
            .collect::<Vec<_>>()
    });
    results.sort_by_key(|result| result.index);
    Ok(results)
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
}
