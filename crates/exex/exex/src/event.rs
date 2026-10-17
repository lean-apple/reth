use alloy_eips::BlockNumHash;

/// Events emitted by an `ExEx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExExEvent {
    /// Highest block processed by the `ExEx`.
    ///
    /// The `ExEx` must guarantee that it will not require all earlier blocks in the future,
    /// meaning that Reth is allowed to prune them.
    ///
    /// On reorgs, it's possible for the height to go down.
    FinishedHeight(BlockNumHash),
    /// Lets the WAL be released through the finalized block while pruning still waits for
    /// [`ExExEvent::FinishedHeight`]. Released notifications are never replayed, so the `ExEx`
    /// must resume from a canonical head and rebuild unprocessed blocks from retained history.
    ReleaseFinalizedWal,
}
