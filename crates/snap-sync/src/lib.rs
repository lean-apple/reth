//! snap/2 (EIP-8189) state synchronization.
//!
//! Snap sync is a bootstrap subsystem: it assembles a state generation from peers instead of
//! executing blocks to reach it. It consumes forkchoice information but is not part of Engine API
//! processing, so it takes the chain through [`CanonicalChainSource`] rather than reaching into
//! the engine tree.
//!
//! [`SnapSyncSession`] is the one serialized owner. A session:
//!
//! 1. picks a target behind the canonical head by following parent links, and resets to a clean
//!    state generation,
//! 2. streams accounts, storage and bytecodes at that target, verifying every response against its
//!    state root,
//! 3. applies the block access lists from the target up to the head, EIP-8189's replacement for
//!    snap/1 trie healing,
//! 4. rebuilds the state trie, checks its root against the header, and persists the trie tables.
//!
//! Everything under [`download`] and the proof verification behind it is response checking, and is
//! independent of that sequencing; [`store`] is the only place state is written.
//!
//! Snap sync is opt-in and is not reth's default sync path.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod chain;
pub mod download;
pub mod error;
pub mod heal;
pub mod session;
pub mod store;

mod metrics;
mod proof;

pub use chain::{BlockRef, CanonicalChainSource, ChainError};
pub use download::{DownloadStateOutcome, StateDownloader};
pub use error::SnapSyncError;
pub use session::{SnapSyncSession, StepOutcome, SyncState};
pub use store::SnapStateWriter;

/// How many blocks behind the canonical head a sync target is placed.
///
/// Serving nodes reconstruct hashed state at `head - N` by reverse-applying changesets, so this
/// must be large enough that the target's state is always fully persisted rather than still held
/// in the engine's in-memory overlay.
pub const PIVOT_OFFSET: u64 = 16;

/// How many peers a single request is tried against before the session gives up.
///
/// A peer that answers with something unusable is reported and the request reissued, so one bad
/// peer costs a round trip rather than the whole sync.
pub(crate) const MAX_REQUEST_ATTEMPTS: usize = 3;

/// Soft response size limit requested for snap protocol messages (2 MiB).
///
/// Matches the cap servers apply, so asking for more only wastes a round trip.
pub const SNAP_RESPONSE_BYTES_LIMIT: u64 = 2 * 1024 * 1024;
