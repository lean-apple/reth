//! Errors surfaced by a snap sync session.

use alloy_primitives::B256;

/// Errors that can occur during snap sync.
#[derive(Debug, thiserror::Error)]
pub enum SnapSyncError {
    /// A network request failed or a peer returned a malformed response.
    #[error("network request failed: {0}")]
    Network(String),
    /// A database operation failed.
    #[error("database error: {0}")]
    Database(String),
    /// RLP decoding of a peer response failed.
    #[error("RLP decode error: {0}")]
    RlpDecode(String),
    /// No peer could serve the pivot root and no fresher pivot is available.
    #[error("no peer serves state root {root}, download stalled at {resume_from}")]
    StaleRoot {
        /// The root that could not be served.
        root: B256,
        /// The account hash the download would resume from.
        resume_from: B256,
    },
    /// The BAL returned for a block does not match the header's commitment.
    #[error("block access list for block {block} does not match header commitment {expected}")]
    BalVerification {
        /// Block number.
        block: u64,
        /// Commitment from the block header.
        expected: B256,
    },
    /// The rebuilt state trie does not match the block's state root.
    #[error("state root mismatch at block {block}: expected {expected}, rebuilt {computed}")]
    StateRootMismatch {
        /// Block the state should correspond to.
        block: u64,
        /// State root from the block header.
        expected: B256,
        /// Root rebuilt from the downloaded state.
        computed: B256,
    },
    /// The block the session assembled state for is no longer canonical.
    #[error("block {0} left the canonical chain before the sync could be finalized")]
    Reorged(B256),
    /// A header required to resolve a pivot or a BAL commitment could not be found.
    #[error("header not found for block {0}")]
    MissingHeader(u64),
    /// No peer had the block access list for a block that requires one.
    #[error("block access list not available for block {0}")]
    MissingBal(u64),
    /// The database uses the legacy plain-state layout, which snap sync cannot populate.
    ///
    /// Snap responses are keyed by hashed address with no preimage, so only a layout that reads
    /// state from the hashed tables can be assembled from them.
    #[error("snap sync requires the v2 storage layout (hashed state as canonical state)")]
    UnsupportedStorageLayout,
    /// No connected peer advertises `snap/2`.
    ///
    /// The network layer fails snap requests immediately rather than queueing them when no
    /// capable peer is connected, so this says nothing about the session — only that it has to
    /// wait. Distinct from a peer that answers badly, which is worth retrying straight away.
    #[error("no connected peer advertises snap/2")]
    NoSnapPeers,
}
