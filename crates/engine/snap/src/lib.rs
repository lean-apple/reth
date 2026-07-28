//! snap/2 (EIP-8189) state synchronization.
//!
//! This crate implements the client half of snap/2: it drives a pivot block forward as the
//! chain advances and streams the hashed state at that pivot from peers, verifying every
//! response against the pivot's state root before persisting it.
//!
//! The two halves are:
//!
//! * [`PivotTracker`] — tracks the block whose state is being downloaded, and advances it when the
//!   chain moves far enough ahead that serving peers can no longer answer for the old root.
//! * [`StateDownloader`] — streams accounts, storage and bytecodes at a given root, verifying range
//!   proofs and writing each batch to the database before requesting the next one.
//!
//! [`sync_state`] ties the two together: it downloads at the current pivot, and whenever a peer
//! reports the root as unavailable it advances the pivot and resumes from where it left off,
//! without discarding the state already written.
//!
//! [`catch_up_with_bals`] then carries that state from the pivot to the chain head by replaying
//! block access lists, which is what EIP-8189 uses in place of snap/1's trie healing.
//!
//! [`SnapStateWriter::finalize_sync`] closes the sync: it rebuilds the state trie over everything
//! that was assembled, checks its root against the block header, and persists the trie tables from
//! the same pass.
//!
//! [`AppliedChain`] covers reorgs during catch-up. Applying a BAL does not record what it
//! replaced, so an orphaned block cannot be rewound; instead the keys each applied block wrote are
//! remembered, and a reorg leaves behind exactly those the new chain does not rewrite.
//!
//! [`recover_from_reorg`] then re-reads those keys with single-key snap requests, so a reorg costs
//! a handful of lookups rather than a restart.
//!
//! What this crate does *not* do yet: the engine wiring that feeds [`SnapSyncEvent`]s in. Snap
//! sync stays opt-in; it is not the default sync path.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod download;
pub mod pivot;
pub mod reorg;
pub mod storage;

mod bal;
mod proof;

pub use download::{DownloadStateOutcome, StateDownloader};
pub use pivot::{PivotTracker, SnapSyncEvent};
pub use reorg::{AppliedChain, StaleKeys};
pub use storage::SnapStateWriter;

use crate::bal::{decode_block_access_list, BlockStateDiff};
use alloy_primitives::B256;
use reth_db_api::transaction::{DbTx, DbTxMut};
use reth_network_p2p::{headers::client::HeadersClient, snap::client::SnapClient};
use reth_provider::{DatabaseProviderFactory, HeaderProvider};
use reth_storage_api::{DBProvider, StateWriter};
use tracing::{debug, info};

/// How many blocks behind the chain head the pivot is placed.
///
/// Serving nodes reconstruct hashed state at `head - N` by reverse-applying changesets, so this
/// must be large enough that the pivot's state is always fully persisted rather than still held
/// in the engine's in-memory overlay.
pub const PIVOT_OFFSET: u64 = 16;

/// Soft response size limit requested for snap protocol messages (2 MiB).
///
/// Matches the cap servers apply, so asking for more only wastes a round trip.
pub const SNAP_RESPONSE_BYTES_LIMIT: u64 = 2 * 1024 * 1024;

/// Downloads the full state at the tracked pivot, advancing the pivot whenever peers can no
/// longer serve the root it currently points at.
///
/// Returns the block number and root the state was completed at. Progress already written to the
/// database is kept across pivot advances: only the accounts after the resume point are refetched.
pub async fn sync_state<C, F>(
    client: &C,
    factory: &F,
    tracker: &mut PivotTracker,
) -> Result<(u64, B256), SnapSyncError>
where
    C: SnapClient + HeadersClient + 'static,
    F: DatabaseProviderFactory,
    F::Provider: DBProvider + HeaderProvider<Header = C::Header>,
    F::ProviderRW: DBProvider + StateWriter,
    <F::Provider as DBProvider>::Tx: DbTx,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    let mut resume_from = B256::ZERO;

    loop {
        let root = tracker.pivot_root();
        match StateDownloader::new(client, factory, root).run(resume_from).await? {
            DownloadStateOutcome::Done => return Ok((tracker.pivot_block(), root)),
            DownloadStateOutcome::Stale { resume_from: next } => {
                resume_from = next;
                if !tracker.advance_pivot(client, factory).await? {
                    // The pivot is already at the newest block we know about, so there is no
                    // fresher root to retry with. Waiting for the next engine event is the
                    // caller's job; reporting the stale root lets it decide.
                    return Err(SnapSyncError::StaleRoot { root, resume_from })
                }
            }
        }
    }
}

/// Replays block access lists from `from_block` up to the head the tracker currently knows about,
/// bringing the downloaded state forward without executing any transactions.
///
/// Returns the last block applied. The head moves while this runs, so the caller re-invokes with
/// the returned block plus one until it has caught up enough to hand over to the engine; each call
/// works against a head snapshot taken at entry so it always terminates.
pub async fn catch_up_with_bals<C, F>(
    client: &C,
    factory: &F,
    tracker: &mut PivotTracker,
    chain: &mut AppliedChain,
    from_block: u64,
) -> Result<CatchUpOutcome, SnapSyncError>
where
    C: SnapClient + HeadersClient + 'static,
    F: DatabaseProviderFactory,
    F::Provider: DBProvider + HeaderProvider<Header = C::Header>,
    F::ProviderRW: DBProvider + StateWriter,
    <F::Provider as DBProvider>::Tx: DbTx,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    tracker.drain_events();

    let writer = SnapStateWriter::new(factory);
    let target = tracker.known_head();
    let mut applied = from_block.saturating_sub(1);

    for block_number in from_block..=target {
        // A block whose parent is not what was applied below it means the chain moved while
        // catch-up was running. Applying it would stack new state on top of orphaned state.
        if let Some((hash, parent_hash)) = tracker.block_hashes(block_number) {
            if let Some(fork_block) = chain.divergence(block_number, parent_hash) {
                chain.orphan_from(fork_block + 1);

                debug!(target: "engine::snap", block_number, fork_block, "Reorg during catch-up");
                return Ok(CatchUpOutcome::Reorged { fork_block })
            }

            let diff = apply_bal(client, factory, tracker, writer, block_number).await?;
            chain.record(block_number, hash, &diff);
        } else {
            // Without engine-reported hashes there is nothing to compare, so the block is applied
            // but not recorded; a later reorg below it cannot be detected from this height.
            apply_bal(client, factory, tracker, writer, block_number).await?;
        }

        applied = block_number;
        debug!(target: "engine::snap", block_number, target, "Applied block access list");
    }

    Ok(CatchUpOutcome::Applied(applied))
}

/// Fetches, verifies and applies one block's access list, returning what it wrote.
async fn apply_bal<C, F>(
    client: &C,
    factory: &F,
    tracker: &PivotTracker,
    writer: SnapStateWriter<'_, F>,
    block_number: u64,
) -> Result<BlockStateDiff, SnapSyncError>
where
    C: SnapClient + HeadersClient + 'static,
    F: DatabaseProviderFactory,
    F::Provider: DBProvider + HeaderProvider<Header = C::Header>,
    F::ProviderRW: DBProvider + StateWriter,
    <F::Provider as DBProvider>::Tx: DbTx,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    let bal = tracker.verified_bal(client, factory, block_number).await?;
    let changes = decode_block_access_list(&bal, block_number)?;
    let diff = BlockStateDiff::from_changes(&changes);
    diff.apply(writer)?;
    Ok(diff)
}

/// Result of a [`catch_up_with_bals`] pass.
#[derive(Debug, PartialEq, Eq)]
pub enum CatchUpOutcome {
    /// Access lists were applied through this block.
    Applied(u64),
    /// The chain reorged mid-catch-up and nothing above `fork_block` was applied.
    ///
    /// Catch-up resumes from `fork_block + 1` along the new chain. Keys the new chain does not
    /// rewrite stay in [`AppliedChain::stale_keys`] and must be re-read from peers, because the
    /// values written for them came from a chain that no longer exists.
    Reorged {
        /// Last block whose applied state is still canonical.
        fork_block: u64,
    },
}

/// Re-reads the keys a reorg stranded, so the state matches the surviving chain again.
///
/// Returns `false` when no peer serves the pivot root any more, leaving the keys marked stale for
/// a retry once the pivot has advanced. On success nothing is left over and the state-root check
/// can proceed.
pub async fn recover_from_reorg<C, F>(
    client: &C,
    factory: &F,
    tracker: &PivotTracker,
    chain: &mut AppliedChain,
) -> Result<bool, SnapSyncError>
where
    C: SnapClient + 'static,
    F: DatabaseProviderFactory,
    F::ProviderRW: DBProvider + StateWriter,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    if chain.stale_keys().is_empty() {
        return Ok(true)
    }

    let mut downloader = StateDownloader::new(client, factory, tracker.pivot_root());
    if !downloader.refetch(chain.stale_keys()).await? {
        return Ok(false)
    }

    let recovered = chain.clear_stale();
    info!(target: "engine::snap", recovered, "Re-read state stranded by a reorg");

    Ok(true)
}

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
    /// A header required to resolve a pivot or a BAL commitment could not be found.
    #[error("header not found for block {0}")]
    MissingHeader(u64),
    /// No peer had the block access list for a block that requires one.
    #[error("block access list not available for block {0}")]
    MissingBal(u64),
}
