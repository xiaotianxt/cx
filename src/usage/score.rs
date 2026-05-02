use std::cmp::Ordering;

use super::SlotResult;

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
    compare_by_score_desc(left, right)
}
