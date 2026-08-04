//! What snap sync needs to know about the canonical chain.
//!
//! Snap sync consumes forkchoice information but is not part of Engine API processing, so it takes
//! the chain through a narrow trait rather than reaching into the engine tree. An adapter on the
//! engine side decides what is canonical; this crate only asks.
//!
//! Blocks are identified by hash throughout. A height alone does not identify a block during a
//! reorg, and resolving a pivot, header or access list by number is exactly how a session ends up
//! mixing two chains together.

use alloy_consensus::BlockHeader as _;
use alloy_primitives::{BlockNumber, B256};
use parking_lot::RwLock;
use reth_provider::{DatabaseProviderFactory, HeaderProvider};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

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

impl<T> CanonicalChainSource for Arc<T>
where
    T: CanonicalChainSource + ?Sized,
{
    fn head(&self) -> BlockRef {
        (**self).head()
    }

    fn canonical_token(&self) -> u64 {
        (**self).canonical_token()
    }

    async fn ancestor(&self, from: B256, depth: u64) -> Result<BlockRef, ChainError> {
        (**self).ancestor(from, depth).await
    }

    async fn segment(&self, ancestor: B256, head: B256) -> Result<Vec<BlockRef>, ChainError> {
        (**self).segment(ancestor, head).await
    }
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
    /// Persisted headers could not be read.
    #[error("canonical header provider failed: {0}")]
    Provider(String),
}

/// Canonical chain source backed by Reth's persisted, validated headers.
#[derive(Debug)]
pub struct ProviderChain<F> {
    factory: F,
    head: RwLock<BlockRef>,
    token: AtomicU64,
}

impl<F> ProviderChain<F>
where
    F: DatabaseProviderFactory,
    F::Provider: HeaderProvider,
{
    /// Opens a chain source at a header already stored by the header stage.
    pub fn new(factory: F, head: B256) -> Result<Self, ChainError> {
        let head = Self::block_by_hash(&factory, head)?;
        Ok(Self { factory, head: RwLock::new(head), token: AtomicU64::new(0) })
    }

    /// Moves forkchoice to another persisted header.
    pub fn update_head(&self, hash: B256) -> Result<BlockRef, ChainError> {
        let current = self.head.read();
        if current.hash == hash {
            return Ok(*current)
        }
        drop(current);

        let head = Self::block_by_hash(&self.factory, hash)?;
        *self.head.write() = head;
        self.token.fetch_add(1, Ordering::Release);
        Ok(head)
    }

    fn block_by_hash(factory: &F, hash: B256) -> Result<BlockRef, ChainError> {
        let provider =
            factory.database_provider_ro().map_err(|err| ChainError::Provider(err.to_string()))?;
        let header = provider
            .sealed_header_by_hash(hash)
            .map_err(|err| ChainError::Provider(err.to_string()))?
            .ok_or(ChainError::UnknownBlock(hash))?;

        Ok(BlockRef {
            hash: header.hash(),
            number: header.number(),
            parent_hash: header.parent_hash(),
            state_root: header.state_root(),
            bal_hash: header.block_access_list_hash(),
        })
    }
}

impl<F> CanonicalChainSource for ProviderChain<F>
where
    F: DatabaseProviderFactory,
    F::Provider: HeaderProvider,
{
    fn head(&self) -> BlockRef {
        *self.head.read()
    }

    fn canonical_token(&self) -> u64 {
        self.token.load(Ordering::Acquire)
    }

    async fn ancestor(&self, from: B256, depth: u64) -> Result<BlockRef, ChainError> {
        let mut block = Self::block_by_hash(&self.factory, from)?;
        for _ in 0..depth {
            block = Self::block_by_hash(&self.factory, block.parent_hash)?;
        }
        Ok(block)
    }

    async fn segment(&self, ancestor: B256, head: B256) -> Result<Vec<BlockRef>, ChainError> {
        if ancestor == head {
            return Ok(Vec::new())
        }

        let anchor = Self::block_by_hash(&self.factory, ancestor)?;
        let mut block = Self::block_by_hash(&self.factory, head)?;
        if anchor.number >= block.number {
            return Err(ChainError::NotAnAncestor { ancestor, head })
        }

        let mut blocks = Vec::with_capacity((block.number - anchor.number) as usize);
        while block.number > anchor.number {
            blocks.push(block);
            block = Self::block_by_hash(&self.factory, block.parent_hash)?;
        }
        if block.hash != ancestor {
            return Err(ChainError::NotAnAncestor { ancestor, head })
        }

        blocks.reverse();
        Ok(blocks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use reth_db_api::{tables, transaction::DbTxMut};
    use reth_provider::{
        test_utils::create_test_provider_factory, StaticFileProviderFactory, StaticFileSegment,
        StaticFileWriter,
    };
    use reth_storage_api::DBProvider;

    /// A linked chain of `len` headers from genesis, each with a distinct state root.
    fn header_chain(len: u64) -> Vec<Header> {
        let mut headers = Vec::with_capacity(len as usize);
        let mut parent_hash = B256::ZERO;
        for number in 0..len {
            let header = Header {
                number,
                parent_hash,
                state_root: B256::with_last_byte(number as u8 + 1),
                ..Default::default()
            };
            parent_hash = header.hash_slow();
            headers.push(header);
        }
        headers
    }

    fn block_ref(header: &Header) -> BlockRef {
        BlockRef {
            hash: header.hash_slow(),
            number: header.number,
            parent_hash: header.parent_hash,
            state_root: header.state_root,
            bal_hash: header.block_access_list_hash,
        }
    }

    #[tokio::test]
    async fn provider_chain_walks_headers_persisted_by_reth() {
        let headers = header_chain(6);
        let factory = create_test_provider_factory();
        {
            let static_files = factory.static_file_provider();
            let mut writer = static_files.latest_writer(StaticFileSegment::Headers).unwrap();
            for header in &headers {
                writer.append_header(header, &header.hash_slow()).unwrap();
            }
        }
        let provider = factory.database_provider_rw().unwrap();
        for header in &headers {
            provider
                .tx_ref()
                .put::<tables::HeaderNumbers>(header.hash_slow(), header.number)
                .unwrap();
        }
        provider.commit().unwrap();
        let chain = Arc::new(ProviderChain::new(factory, headers[5].hash_slow()).unwrap());

        assert_eq!(
            chain.ancestor(headers[5].hash_slow(), 2).await.unwrap(),
            block_ref(&headers[3])
        );
        assert_eq!(
            chain.segment(headers[2].hash_slow(), headers[5].hash_slow()).await.unwrap(),
            headers[3..6].iter().map(block_ref).collect::<Vec<_>>()
        );

        chain.update_head(headers[4].hash_slow()).unwrap();
        assert_eq!(CanonicalChainSource::head(&chain), block_ref(&headers[4]));
    }
}
