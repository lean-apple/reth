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
use alloy_primitives::{BlockNumber, Sealable as _, B256};
use reth_eth_wire_types::HeadersDirection;
use reth_network_p2p::headers::client::{HeadersClient, HeadersRequest};
use reth_provider::{DatabaseProviderFactory, HeaderProvider};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        RwLock,
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
    /// Headers could not be fetched from any peer.
    #[error("header download failed: {0}")]
    Download(String),
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
        if self.head.read().expect("head lock poisoned").hash == hash {
            return Ok(*self.head.read().expect("head lock poisoned"))
        }

        let head = Self::block_by_hash(&self.factory, hash)?;
        *self.head.write().expect("head lock poisoned") = head;
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
        *self.head.read().expect("head lock poisoned")
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

/// A [`CanonicalChainSource`] for a node that has no chain of its own yet.
///
/// A snap syncing node answers `SYNCING` to the consensus layer, so forkchoice reaches it as a
/// bare head hash. This resolves everything else — numbers, parent links, state roots, access
/// list commitments — from peers' headers, and verifies each header against the hash it was
/// requested by, so a peer cannot answer with a block other than the one asked for.
#[derive(Debug)]
pub struct HeaderChain<C> {
    /// Peer client every header comes from.
    client: C,
    /// The last head forkchoice reported, resolved to a full reference.
    head: RwLock<BlockRef>,
    /// Bumped whenever the head moves; see [`CanonicalChainSource::canonical_token`].
    token: AtomicU64,
}

/// Headers asked of a peer in one request.
///
/// Matches the limit serving implementations cap responses at, so asking for more would only
/// return short.
const MAX_HEADERS_PER_REQUEST: u64 = 192;

impl<C> HeaderChain<C>
where
    C: HeadersClient<Header: reth_primitives_traits::BlockHeader>,
{
    /// Creates a chain source anchored at an already-resolved head.
    pub fn new(client: C, head: BlockRef) -> Self {
        Self { client, head: RwLock::new(head), token: AtomicU64::new(0) }
    }

    /// Moves the head to `hash`, resolving its header from peers.
    ///
    /// The forkchoice head is trusted by hash only; everything else is fetched and checked.
    pub async fn update_head(&self, hash: B256) -> Result<BlockRef, ChainError> {
        if self.head.read().expect("head lock poisoned").hash == hash {
            return Ok(*self.head.read().expect("head lock poisoned"))
        }

        let head = self.block_by_hash(hash).await?;
        *self.head.write().expect("head lock poisoned") = head;
        self.token.fetch_add(1, Ordering::Release);
        Ok(head)
    }

    /// Fetches one block's reference, verified against the hash it was requested by.
    async fn block_by_hash(&self, hash: B256) -> Result<BlockRef, ChainError> {
        Ok(self.walk_falling(hash, 1).await?.pop().expect("walk returned one block"))
    }

    /// Fetches `count` blocks walking down parent links from `from` (inclusive).
    ///
    /// Returned descending by height. Every header is verified to hash to the block it stands
    /// for: the first to `from`, each next to its predecessor's parent hash, so an unrelated or
    /// reordered response never passes.
    async fn walk_falling(&self, from: B256, count: u64) -> Result<Vec<BlockRef>, ChainError> {
        let mut blocks: Vec<BlockRef> = Vec::with_capacity(count as usize);
        let mut attempts_left = crate::MAX_REQUEST_ATTEMPTS;

        while (blocks.len() as u64) < count {
            let cursor = blocks.last().map(|block| block.parent_hash).unwrap_or(from);
            let remaining = count - blocks.len() as u64;

            let response = self
                .client
                .get_headers(HeadersRequest {
                    start: cursor.into(),
                    limit: remaining.min(MAX_HEADERS_PER_REQUEST),
                    direction: HeadersDirection::Falling,
                })
                .await
                .map_err(|err| ChainError::Download(err.to_string()))?;

            let (peer, headers) = response.split();
            if headers.is_empty() {
                // The peer does not have the block; another might.
                attempts_left = attempts_left.saturating_sub(1);
                if attempts_left == 0 {
                    return Err(ChainError::UnknownBlock(cursor))
                }
                continue
            }

            let mut expected = cursor;
            let mut verified = Vec::with_capacity(headers.len());
            for header in headers {
                let hash = header.hash_slow();
                if hash != expected {
                    break
                }
                expected = header.parent_hash();
                verified.push(BlockRef {
                    hash,
                    number: header.number(),
                    parent_hash: header.parent_hash(),
                    state_root: header.state_root(),
                    bal_hash: header.block_access_list_hash(),
                });
            }

            if verified.is_empty() {
                // The peer answered with a block other than the one asked for.
                self.client.report_bad_message(peer);
                attempts_left = attempts_left.saturating_sub(1);
                if attempts_left == 0 {
                    return Err(ChainError::Download(format!(
                        "no peer served a verifiable header for {cursor}"
                    )))
                }
                continue
            }

            attempts_left = crate::MAX_REQUEST_ATTEMPTS;
            blocks.extend(verified);
        }

        blocks.truncate(count as usize);
        Ok(blocks)
    }
}

impl<C> CanonicalChainSource for HeaderChain<C>
where
    C: HeadersClient<Header: reth_primitives_traits::BlockHeader> + Sync,
{
    fn head(&self) -> BlockRef {
        *self.head.read().expect("head lock poisoned")
    }

    fn canonical_token(&self) -> u64 {
        self.token.load(Ordering::Acquire)
    }

    async fn ancestor(&self, from: B256, depth: u64) -> Result<BlockRef, ChainError> {
        Ok(self.walk_falling(from, depth + 1).await?.pop().expect("walk returned depth + 1 blocks"))
    }

    async fn segment(&self, ancestor: B256, head: B256) -> Result<Vec<BlockRef>, ChainError> {
        if ancestor == head {
            return Ok(Vec::new())
        }

        let anchor = self.block_by_hash(ancestor).await?;
        let top = self.block_by_hash(head).await?;
        if anchor.number >= top.number {
            return Err(ChainError::NotAnAncestor { ancestor, head })
        }

        // Walking down from the head by exactly the height difference either lands on the
        // ancestor or proves the two are on different chains; hashes decide, heights only size
        // the walk.
        let mut blocks = self.walk_falling(head, top.number - anchor.number).await?;
        if blocks.last().expect("walk returned at least one block").parent_hash != ancestor {
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
    use reth_network_p2p::test_utils::TestHeadersClient;
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

    /// Queues `headers` as one falling response.
    async fn queue_falling(client: &TestHeadersClient, headers: &[Header]) {
        client.extend(headers.iter().rev().cloned()).await;
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
        let chain = ProviderChain::new(factory, headers[5].hash_slow()).unwrap();

        assert_eq!(
            chain.ancestor(headers[5].hash_slow(), 2).await.unwrap(),
            block_ref(&headers[3])
        );
        assert_eq!(
            chain.segment(headers[2].hash_slow(), headers[5].hash_slow()).await.unwrap(),
            headers[3..6].iter().map(block_ref).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn ancestor_walks_parent_links() {
        let headers = header_chain(6);
        let client = TestHeadersClient::default();
        queue_falling(&client, &headers[3..6]).await;
        let chain = HeaderChain::new(client, block_ref(&headers[5]));

        let ancestor = chain.ancestor(headers[5].hash_slow(), 2).await.unwrap();

        assert_eq!(ancestor, block_ref(&headers[3]));
    }

    #[tokio::test]
    async fn a_response_for_the_wrong_block_is_rejected() {
        let headers = header_chain(6);
        let unrelated = Header { number: 5, ..Default::default() };
        let client = TestHeadersClient::default();
        client.extend([unrelated]).await;
        let chain = HeaderChain::new(client, block_ref(&headers[5]));

        // The only served header does not hash to the requested block, so no attempt can
        // succeed; accepting it would let a peer substitute an arbitrary chain.
        assert!(chain.ancestor(headers[5].hash_slow(), 2).await.is_err());
    }

    #[tokio::test]
    async fn segment_returns_ascending_blocks_between_anchor_and_head() {
        let headers = header_chain(6);
        let client = TestHeadersClient::default();
        // One response per request: the anchor, the head, then the walk down from the head.
        client.extend([headers[2].clone()]).await;
        client.extend([headers[5].clone()]).await;
        queue_falling(&client, &headers[3..6]).await;
        let chain = HeaderChain::new(client, block_ref(&headers[5]));

        let segment = chain.segment(headers[2].hash_slow(), headers[5].hash_slow()).await.unwrap();

        let expected: Vec<BlockRef> = headers[3..6].iter().map(block_ref).collect();
        assert_eq!(segment, expected);
    }

    #[tokio::test]
    async fn segment_rejects_an_anchor_from_another_chain() {
        let headers = header_chain(6);
        // Same height as headers[2], different identity.
        let foreign =
            Header { number: 2, state_root: B256::repeat_byte(0xff), ..Default::default() };
        let client = TestHeadersClient::default();
        client.extend([foreign.clone()]).await;
        client.extend([headers[5].clone()]).await;
        queue_falling(&client, &headers[3..6]).await;
        let chain = HeaderChain::new(client, block_ref(&headers[5]));

        let err = chain.segment(foreign.hash_slow(), headers[5].hash_slow()).await.unwrap_err();

        assert!(matches!(err, ChainError::NotAnAncestor { .. }));
    }

    #[tokio::test]
    async fn segment_of_a_block_to_itself_is_empty() {
        let headers = header_chain(2);
        let chain = HeaderChain::new(TestHeadersClient::default(), block_ref(&headers[1]));

        let hash = headers[1].hash_slow();
        assert_eq!(chain.segment(hash, hash).await.unwrap(), Vec::new());
    }

    #[tokio::test]
    async fn update_head_resolves_and_bumps_the_token() {
        let headers = header_chain(7);
        let client = TestHeadersClient::default();
        client.extend([headers[6].clone()]).await;
        let chain = HeaderChain::new(client, block_ref(&headers[5]));
        let token = chain.canonical_token();

        // Same head: nothing to resolve, nothing moved.
        chain.update_head(headers[5].hash_slow()).await.unwrap();
        assert_eq!(chain.canonical_token(), token);

        let moved = chain.update_head(headers[6].hash_slow()).await.unwrap();

        assert_eq!(moved, block_ref(&headers[6]));
        assert_eq!(chain.head(), block_ref(&headers[6]));
        assert_ne!(chain.canonical_token(), token);
    }
}
