use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::rustc_data_structures::sync::run_stage;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RoundDelta {
    pub(super) input_index: usize,
    pub(super) indeterminate_count: usize,
}

pub(super) fn stage_deltas(counts: Vec<usize>) -> Vec<RoundDelta> {
    let len = counts.len();
    let deltas = run_stage(Arc::new(counts), len, |counts, index| RoundDelta {
        input_index: index,
        indeterminate_count: counts[index],
    });

    for (expected, delta) in deltas.iter().enumerate() {
        assert_eq!(
            delta.input_index, expected,
            "rs3_import_round_commit_order"
        );
    }

    deltas
}

