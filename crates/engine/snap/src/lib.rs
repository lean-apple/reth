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
//! * [`download_state`] — streams accounts, storage and bytecodes at a given root, verifying range
//!   proofs and writing each batch to the database before requesting the next one.
//!
//! [`sync_state`] ties the two together: it downloads at the current pivot, and whenever a peer
//! reports the root as unavailable it advances the pivot and resumes from where it left off,
//! without discarding the state already written.
//!
//! What this crate does *not* do yet: applying the block access lists collected between the final
//! pivot and the chain head (EIP-8189's replacement for snap/1 healing), the final state-root
//! check after that catch-up, and reorg recovery. Those build on top of [`sync_state`].

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod download;
pub mod pivot;

mod proof;
mod storage;

pub use download::{download_state, DownloadStateOutcome};
pub use pivot::{PivotTracker, SnapSyncEvent};

use alloy_primitives::B256;
use reth_db_api::transaction::{DbTx, DbTxMut};
use reth_network_p2p::{headers::client::HeadersClient, snap::client::SnapClient};
use reth_provider::{DatabaseProviderFactory, HeaderProvider};
use reth_storage_api::{DBProvider, StateWriter};

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
    F: DatabaseProviderFactory + Clone + Send + Sync + 'static,
    F::Provider: DBProvider + HeaderProvider<Header = C::Header>,
    F::ProviderRW: DBProvider + StateWriter,
    <F::Provider as DBProvider>::Tx: DbTx,
    <F::ProviderRW as DBProvider>::Tx: DbTxMut,
{
    let mut resume_from = B256::ZERO;

    loop {
        let root = tracker.pivot_root();
        match download_state(client, factory, root, resume_from).await? {
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
    /// A header required to resolve a pivot or a BAL commitment could not be found.
    #[error("header not found for block {0}")]
    MissingHeader(u64),
    /// No peer had the block access list for a block that requires one.
    #[error("block access list not available for block {0}")]
    MissingBal(u64),
}
