//! What snap sync needs to know about the canonical chain.
//!
//! Snap sync consumes forkchoice information but is not part of Engine API processing, so it takes
//! the chain through a narrow trait rather than reaching into the engine tree. An adapter on the
//! engine side decides what is canonical; this crate only asks.
//!
//! Blocks are identified by hash throughout. A height alone does not identify a block during a
//! reorg, and resolving a pivot, header or access list by number is exactly how a session ends up
//! mixing two chains together.

use alloy_primitives::{BlockNumber, B256};
use std::future::Future;

/// A block identified by hash, with the height and links a session needs to order and connect it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRef {
    /// Block hash. The identity of the block.
    pub hash: B256,
    /// Block number, for ordering and reporting only.
    pub number: BlockNumber,
    /// Hash of the parent, used to connect a segment without trusting heights.
    pub parent_hash: B256,
    /// State root committed to by this block's header.
    pub state_root: B256,
    /// EIP-7928 access list commitment from this block's header, when the fork is active.
    pub bal_hash: Option<B256>,
}

/// The canonical chain, as far as snap sync is concerned.
///
/// Canonicality comes from forkchoice alone. A payload that merely arrived is not canonical, and
/// the head may move to a lower height or to a different block at the same height, so
/// implementations must not assume the head only advances.
pub trait CanonicalChainSource: Send + Sync {
    /// Returns the current canonical head.
    fn head(&self) -> BlockRef;

    /// Returns a token that changes whenever forkchoice moves the canonical head.
    ///
    /// Rebuilding the state trie takes long enough that the head can move during it, so the token
    /// is read before the work and compared after: equal means no forkchoice update landed and a
    /// canonicality check taken beforehand still holds.
    fn canonical_token(&self) -> u64;

    /// Returns the block `depth` blocks below `from`, found by following parent links.
    ///
    /// This is how a pivot is chosen. Subtracting from a height would name a block on whichever
    /// chain happens to be canonical at lookup time, which is not necessarily this one.
    fn ancestor(
        &self,
        from: B256,
        depth: u64,
    ) -> impl Future<Output = Result<BlockRef, ChainError>> + Send;

    /// Returns the blocks from `ancestor` (exclusive) to `head` (inclusive), in ascending order.
    ///
    /// Walking by parent hash means the result is a single connected chain even if the head moved
    /// while the call was in flight. Returns an error when `ancestor` is not an ancestor of `head`.
    fn segment(
        &self,
        ancestor: B256,
        head: B256,
    ) -> impl Future<Output = Result<Vec<BlockRef>, ChainError>> + Send;
}

/// Why the canonical chain could not answer.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    /// The requested block is not known to the chain source.
    #[error("block {0} is not known")]
    UnknownBlock(B256),
    /// `ancestor` does not connect to `head` by parent links.
    #[error("block {ancestor} is not an ancestor of {head}")]
    NotAnAncestor {
        /// The block that was expected to be an ancestor.
        ancestor: B256,
        /// The head the segment was requested for.
        head: B256,
    },
}
