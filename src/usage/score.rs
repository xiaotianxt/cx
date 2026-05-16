use std::cmp::Ordering;

use super::SlotResult;

const EXPECTED_SESSION_FLOOR_PERCENT: f64 = 20.0;
const PERCENT_EPSILON: f64 = 0.000_001;

pub fn sort_by_score_desc(results: &mut [SlotResult]) {
    results.sort_by(compare_by_score_desc);
}

fn compare_by_score_desc(left: &SlotResult, right: &SlotResult) -> Ordering {
    right
        .score
        .partial_cmp(&left.score)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left.index.cmp(&right.index))
}

pub fn compare_for_selection(left: &SlotResult, right: &SlotResult) -> Ordering {
    let left_rank = SelectionRank::from_result(left);
    let right_rank = SelectionRank::from_result(right);

    match (left_rank, right_rank) {
        (Some(left_rank), Some(right_rank)) => left_rank
            .compare(right_rank)
            .then_with(|| left.index.cmp(&right.index)),
        _ => compare_by_score_desc(left, right),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct SelectionRank {
    bottleneck_remaining: f64,
    bottleneck_refresh_at: Option<i64>,
    has_expected_session_capacity: bool,
}

impl SelectionRank {
    fn from_result(result: &SlotResult) -> Option<Self> {
        let five_hour =
            QuotaWindow::from_usage(result.five_hour_used_percent, result.five_hour_refresh_at);
        let weekly = QuotaWindow::from_usage(result.weekly_used_percent, result.weekly_refresh_at);
        let windows = [five_hour, weekly];

        let bottleneck_remaining = windows
            .into_iter()
            .flatten()
            .map(|window| window.remaining_percent)
            .min_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal))?;

        let bottleneck_refresh_at = [five_hour, weekly]
            .into_iter()
            .flatten()
            .filter(|window| {
                (window.remaining_percent - bottleneck_remaining).abs() <= PERCENT_EPSILON
            })
            .filter_map(|window| window.refresh_at)
            .min();

        Some(Self {
            bottleneck_remaining,
            bottleneck_refresh_at,
            has_expected_session_capacity: bottleneck_remaining >= EXPECTED_SESSION_FLOOR_PERCENT,
        })
    }

    fn compare(self, other: Self) -> Ordering {
        match (
            self.has_expected_session_capacity,
            other.has_expected_session_capacity,
        ) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (true, true) => {
                compare_refresh_at(self.bottleneck_refresh_at, other.bottleneck_refresh_at)
                    .then_with(|| {
                        compare_remaining_desc(
                            self.bottleneck_remaining,
                            other.bottleneck_remaining,
                        )
                    })
            }
            (false, false) => {
                compare_remaining_desc(self.bottleneck_remaining, other.bottleneck_remaining)
                    .then_with(|| {
                        compare_refresh_at(self.bottleneck_refresh_at, other.bottleneck_refresh_at)
                    })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct QuotaWindow {
    remaining_percent: f64,
    refresh_at: Option<i64>,
}

impl QuotaWindow {
    fn from_usage(used_percent: Option<f64>, refresh_at: Option<i64>) -> Option<Self> {
        let used_percent = used_percent?;
        used_percent.is_finite().then(|| Self {
            remaining_percent: 100.0 - used_percent.clamp(0.0, 100.0),
            refresh_at,
        })
    }
}

fn compare_remaining_desc(left: f64, right: f64) -> Ordering {
    right.partial_cmp(&left).unwrap_or(Ordering::Equal)
}

fn compare_refresh_at(left: Option<i64>, right: Option<i64>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::SlotStatus;

    fn usage_slot(
        name: &str,
        index: usize,
        five_hour_remaining: f64,
        five_hour_refresh_at: i64,
        weekly_remaining: f64,
        weekly_refresh_at: i64,
    ) -> SlotResult {
        let score = five_hour_remaining.min(weekly_remaining);
        let mut result = SlotResult::new(name, index, SlotStatus::Available, score, "usage");
        result.five_hour_used_percent = Some(100.0 - five_hour_remaining);
        result.five_hour_refresh_at = Some(five_hour_refresh_at);
        result.weekly_used_percent = Some(100.0 - weekly_remaining);
        result.weekly_refresh_at = Some(weekly_refresh_at);
        result
    }

    #[test]
    fn selection_prefers_safe_slot_with_earlier_bottleneck_refresh() {
        let mut results = [
            usage_slot("reserve", 0, 95.0, 300, 80.0, 20_000),
            usage_slot("recycling", 1, 25.0, 100, 90.0, 30_000),
        ];

        results.sort_by(compare_for_selection);

        let slots = results
            .iter()
            .map(|result| result.slot.as_str())
            .collect::<Vec<_>>();
        assert_eq!(slots, ["recycling", "reserve"]);
    }

    #[test]
    fn selection_keeps_thin_slots_behind_safe_slots() {
        let mut results = [
            usage_slot("thin", 0, 8.0, 100, 90.0, 30_000),
            usage_slot("safe", 1, 20.0, 10_000, 90.0, 30_000),
        ];

        results.sort_by(compare_for_selection);

        let slots = results
            .iter()
            .map(|result| result.slot.as_str())
            .collect::<Vec<_>>();
        assert_eq!(slots, ["safe", "thin"]);
    }

    #[test]
    fn selection_uses_remaining_capacity_when_every_slot_is_thin() {
        let mut results = [
            usage_slot("soon-empty", 0, 4.0, 100, 90.0, 30_000),
            usage_slot("less-thin", 1, 12.0, 10_000, 90.0, 30_000),
        ];

        results.sort_by(compare_for_selection);

        let slots = results
            .iter()
            .map(|result| result.slot.as_str())
            .collect::<Vec<_>>();
        assert_eq!(slots, ["less-thin", "soon-empty"]);
    }
}
