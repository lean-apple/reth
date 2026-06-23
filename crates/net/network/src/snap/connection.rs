//! snap/2 satellite connection: serves inbound requests and correlates client responses.

use crate::{
    eth_requests::{MAX_BLOCK_ACCESS_LISTS_SERVE, SOFT_RESPONSE_LIMIT},
    snap::peers::{SnapPeerRequest, SnapPeers},
};
use alloy_primitives::bytes::BytesMut;
use futures::{Stream, StreamExt};
use reth_eth_wire::multiplex::ProtocolConnection;
use reth_eth_wire_types::{
    snap::{
        AccountRangeMessage, BlockAccessListsMessage, ByteCodesMessage, GetBlockAccessListsMessage,
        SnapProtocolMessage, StorageRangesMessage,
    },
    BlockAccessLists, SnapVersion,
};
use reth_network_api::PeerId;
use reth_network_p2p::{error::PeerRequestResult, snap::client::SnapResponse};
use reth_network_peers::WithPeerId;
use reth_storage_api::{BalProvider, GetBlockAccessListLimit};
use std::{
    collections::{HashMap, VecDeque},
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::oneshot;
use tokio_stream::wrappers::UnboundedReceiverStream;

/// A negotiated `snap/2` connection.
///
/// Serves inbound requests from the provider and correlates inbound responses back to client
/// requests by `request_id`. Outbound messages are emitted `snap/2`-relative; the multiplexer
/// applies the capability offset before they reach the wire.
#[derive(Debug)]
pub struct SnapConnection<P, In = ProtocolConnection> {
    /// Inbound stream of `snap/2`-relative messages from the peer.
    conn: In,
    /// Outbound client requests for this peer.
    commands: UnboundedReceiverStream<SnapPeerRequest>,
    /// Provider used to serve inbound requests.
    provider: P,
    /// The peer this connection serves.
    peer_id: PeerId,
    /// Registry to deregister from on close.
    peers: SnapPeers,
    /// Outstanding client requests awaiting a response, keyed by `request_id`.
    inflight: HashMap<u64, oneshot::Sender<PeerRequestResult<SnapResponse>>>,
    /// Encoded messages waiting to be returned to the multiplexer.
    outbound: VecDeque<BytesMut>,
}

impl<P, In> SnapConnection<P, In> {
    pub(crate) fn new(
        conn: In,
        commands: UnboundedReceiverStream<SnapPeerRequest>,
        provider: P,
        peer_id: PeerId,
        peers: SnapPeers,
    ) -> Self {
        Self {
            conn,
            commands,
            provider,
            peer_id,
            peers,
            inflight: HashMap::new(),
            outbound: VecDeque::new(),
        }
    }
}

impl<P, In> SnapConnection<P, In>
where
    P: BalProvider,
{
    /// Queues a client request: tracks it for correlation and encodes it for the wire.
    fn on_command(&mut self, req: SnapPeerRequest) {
        self.inflight.insert(req.request.request_id(), req.response);
        self.outbound.push_back(BytesMut::from(req.request.encode().as_ref()));
    }

    /// Handles an inbound message: routes responses to their waiting request, serves requests.
    fn on_inbound(&mut self, bytes: &[u8]) {
        let Some(msg) = decode_snap2(bytes) else { return };
        let request_id = msg.request_id();

        match SnapResponse::try_from(msg) {
            Ok(resp) => {
                if let Some(tx) = self.inflight.remove(&request_id) {
                    let _ = tx.send(Ok(WithPeerId::new(self.peer_id, resp)));
                }
            }
            Err(request) => {
                if let Some(out) = serve(&self.provider, request) {
                    self.outbound.push_back(BytesMut::from(out.encode().as_ref()));
                }
            }
        }
    }
}

impl<P, In> Stream for SnapConnection<P, In>
where
    P: BalProvider + Unpin,
    In: Stream<Item = BytesMut> + Unpin,
{
    type Item = BytesMut;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(msg) = this.outbound.pop_front() {
                return Poll::Ready(Some(msg));
            }

            if let Poll::Ready(Some(req)) = this.commands.poll_next_unpin(cx) {
                this.on_command(req);
                continue;
            }

            match this.conn.poll_next_unpin(cx) {
                Poll::Ready(Some(bytes)) => this.on_inbound(&bytes),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<P, In> Drop for SnapConnection<P, In> {
    fn drop(&mut self) {
        self.peers.remove(&self.peer_id);
    }
}

/// Validates and decodes a single inbound `snap/2` message.
///
/// Returns `None` for empty/malformed payloads and for message IDs not valid in snap/2 (the removed
/// trie-node messages `0x06`/`0x07`).
fn decode_snap2(bytes: &[u8]) -> Option<SnapProtocolMessage> {
    let id = *bytes.first()?;
    if !SnapVersion::V2.supports_message_id(id) {
        return None;
    }
    SnapProtocolMessage::decode(id, &mut &bytes[1..]).ok()
}

/// Serves a single `snap/2` request, returning the response message.
///
/// Missing data is returned as empty entries and responses preserve request order; bulk-state
/// ranges are answered empty until the range server lands.
fn serve<P: BalProvider>(provider: &P, msg: SnapProtocolMessage) -> Option<SnapProtocolMessage> {
    match msg {
        SnapProtocolMessage::GetBlockAccessLists(req) => {
            Some(SnapProtocolMessage::BlockAccessLists(serve_block_access_lists(provider, req)))
        }
        // TODO(snap/2 bulk-state server): serve real ranges. Empty responses are protocol-valid.
        SnapProtocolMessage::GetAccountRange(req) => {
            Some(SnapProtocolMessage::AccountRange(AccountRangeMessage {
                request_id: req.request_id,
                accounts: Vec::new(),
                proof: Vec::new(),
            }))
        }
        SnapProtocolMessage::GetStorageRanges(req) => {
            Some(SnapProtocolMessage::StorageRanges(StorageRangesMessage {
                request_id: req.request_id,
                slots: Vec::new(),
                proof: Vec::new(),
            }))
        }
        SnapProtocolMessage::GetByteCodes(req) => {
            Some(SnapProtocolMessage::ByteCodes(ByteCodesMessage {
                request_id: req.request_id,
                codes: Vec::new(),
            }))
        }
        // Response messages have no server reply.
        SnapProtocolMessage::AccountRange(_) |
        SnapProtocolMessage::StorageRanges(_) |
        SnapProtocolMessage::ByteCodes(_) |
        SnapProtocolMessage::BlockAccessLists(_) => None,
    }
}

/// Serves a `GetBlockAccessLists` request from the BAL store.
///
/// Entries are returned in request order with unavailable BALs as empty entries, truncating from
/// the tail once the soft byte limit is exceeded (EIP-8189).
fn serve_block_access_lists<P: BalProvider>(
    provider: &P,
    mut req: GetBlockAccessListsMessage,
) -> BlockAccessListsMessage {
    req.block_hashes.truncate(MAX_BLOCK_ACCESS_LISTS_SERVE);
    let soft_limit = (req.response_bytes as usize).min(SOFT_RESPONSE_LIMIT);
    let limit = GetBlockAccessListLimit::ResponseSizeSoftLimit(soft_limit);
    let lists =
        provider.bal_store().get_by_hashes_with_limit(&req.block_hashes, limit).unwrap_or_default();
    BlockAccessListsMessage {
        request_id: req.request_id,
        block_access_lists: BlockAccessLists(lists),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use reth_eth_wire_types::snap::SnapProtocolMessage;
    use reth_storage_api::BalStoreHandle;
    use tokio::sync::mpsc;

    /// Provider backed by the no-op BAL store: every hash resolves to a missing (empty) entry.
    #[derive(Debug, Clone, Default)]
    struct NoopProvider {
        store: BalStoreHandle,
    }

    impl BalProvider for NoopProvider {
        fn bal_store(&self) -> &BalStoreHandle {
            &self.store
        }
    }

    #[test]
    fn serves_bal_in_request_order_with_missing_as_empty() {
        let provider = NoopProvider::default();
        let hashes =
            vec![B256::with_last_byte(1), B256::with_last_byte(2), B256::with_last_byte(3)];
        let SnapProtocolMessage::BlockAccessLists(resp) = serve(
            &provider,
            SnapProtocolMessage::GetBlockAccessLists(GetBlockAccessListsMessage {
                request_id: 42,
                block_hashes: hashes.clone(),
                response_bytes: u64::MAX,
            }),
        )
        .unwrap() else {
            panic!("expected BlockAccessLists");
        };

        assert_eq!(resp.request_id, 42);
        assert_eq!(resp.block_access_lists.0.len(), hashes.len());
        assert!(resp.block_access_lists.0.iter().all(Option::is_none));
    }

    #[test]
    fn serves_bal_truncates_from_tail_on_soft_limit() {
        let provider = NoopProvider::default();
        let SnapProtocolMessage::BlockAccessLists(resp) = serve(
            &provider,
            SnapProtocolMessage::GetBlockAccessLists(GetBlockAccessListsMessage {
                request_id: 1,
                block_hashes: vec![
                    B256::with_last_byte(1),
                    B256::with_last_byte(2),
                    B256::with_last_byte(3),
                ],
                response_bytes: 1,
            }),
        )
        .unwrap() else {
            panic!("expected BlockAccessLists");
        };
        assert_eq!(resp.block_access_lists.0.len(), 2);
    }

    #[tokio::test]
    async fn client_request_is_correlated_to_response() {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (wire_tx, wire_rx) = mpsc::unbounded_channel::<BytesMut>();
        let peer = PeerId::random();

        let conn = SnapConnection::new(
            UnboundedReceiverStream::new(wire_rx),
            UnboundedReceiverStream::new(cmd_rx),
            NoopProvider::default(),
            peer,
            SnapPeers::default(),
        );

        // forward outbound bytes the connection emits to a channel so we can inspect them
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut conn = conn;
            while let Some(out) = conn.next().await {
                let _ = out_tx.send(out);
            }
        });

        // client issues a BAL request
        let (resp_tx, resp_rx) = oneshot::channel();
        cmd_tx
            .send(SnapPeerRequest {
                request: SnapProtocolMessage::GetBlockAccessLists(GetBlockAccessListsMessage {
                    request_id: 9,
                    block_hashes: vec![B256::with_last_byte(1)],
                    response_bytes: u64::MAX,
                }),
                response: resp_tx,
            })
            .unwrap();

        // the connection encodes and emits the request
        let out = out_rx.recv().await.unwrap();
        assert!(matches!(
            decode_snap2(&out),
            Some(SnapProtocolMessage::GetBlockAccessLists(req)) if req.request_id == 9
        ));

        // peer answers; the connection correlates it back to the awaiting request
        let answer = SnapProtocolMessage::BlockAccessLists(BlockAccessListsMessage {
            request_id: 9,
            block_access_lists: BlockAccessLists(vec![None]),
        });
        wire_tx.send(BytesMut::from(answer.encode().as_ref())).unwrap();

        let resp = resp_rx.await.unwrap().unwrap();
        assert_eq!(resp.peer_id(), peer);
        assert!(matches!(resp.into_data(), SnapResponse::BlockAccessLists(m) if m.request_id == 9));
    }
}
