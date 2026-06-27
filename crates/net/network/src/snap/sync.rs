//! Pure, reusable helpers for snap/2 sync.
//!
//! Orchestration — pivot selection, restart on reorg, progress checkpoints — is owned by the staged
//! sync pipeline, mirroring how geth drives its snap syncer from the downloader rather than from
//! the syncer itself. This module only holds the version-agnostic pieces the sync stage builds on:
//! a progress descriptor and BAL catch-up chunking.

use std::ops::RangeInclusive;

/// Coarse progress of snap/2 sync, surfaced for reporting (analogous to geth's snap sync progress).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapSyncPhase {
    /// Downloading account/storage/code ranges at the frozen pivot.
    DownloadingState,
    /// Applying verified BAL diffs from the pivot up to the head.
    CatchingUpBal,
    /// Checking the computed state root against the head header.
    VerifyingRoot,
    /// Sync finished.
    Done,
}

/// Splits the BAL catch-up span `pivot + 1 ..= head` into chunks of at most `chunk` blocks.
///
/// Catch-up is chunked so BAL fetch/apply never buffers the whole span in memory (EIP-8189).
/// Returns an empty vector when the pivot is already at or beyond head.
pub fn catch_up_spans(pivot: u64, head: u64, chunk: u64) -> Vec<RangeInclusive<u64>> {
    assert!(chunk > 0, "chunk size must be non-zero");
    let mut spans = Vec::new();
    let mut start = pivot.saturating_add(1);
    while start <= head {
        let end = start.saturating_add(chunk - 1).min(head);
        spans.push(start..=end);
        start = end + 1;
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catch_up_spans_are_chunked() {
        assert_eq!(catch_up_spans(100, 103, 2), vec![101..=102, 103..=103]);
        assert_eq!(catch_up_spans(100, 100, 4), Vec::<RangeInclusive<u64>>::new());
        assert_eq!(catch_up_spans(0, 5, 10), vec![1..=5]);
    }
}
