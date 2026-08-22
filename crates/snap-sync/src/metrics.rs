//! Metrics for a snap sync session.

use reth_metrics::{metrics::Counter, Metrics};

/// Progress counters for one session.
#[derive(Metrics)]
#[metrics(scope = "snap_sync")]
pub(crate) struct SnapSyncMetrics {
    /// Block access lists applied while healing toward the head.
    pub(crate) access_lists_applied: Counter,
    /// Times no peer served the session's target root, forcing the target to move.
    pub(crate) targets_stale: Counter,
    /// Times a step stopped because no connected peer advertised `snap/2`.
    pub(crate) waits_for_peers: Counter,
}
