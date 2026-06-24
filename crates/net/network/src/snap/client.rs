//! Network-facing `snap/2` sync client.

use crate::snap::peers::{SnapPeerRequest, SnapPeers};
use reth_eth_wire_types::snap::{
    GetAccountRangeMessage, GetBlockAccessListsMessage, GetByteCodesMessage,
    GetStorageRangesMessage, SnapProtocolMessage,
};
use reth_network_api::PeerId;
use reth_network_p2p::{
    download::DownloadClient,
    error::{PeerRequestResult, RequestError},
    priority::Priority,
    snap::client::{SnapClient as SnapClientTrait, SnapResponse},
};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::oneshot;

/// A [`SnapClient`](SnapClientTrait) that dispatches requests to connected `snap/2` peers.
///
/// Cloning shares the same peer registry. The stream implementation that negotiates snap support is
/// responsible for registering per-peer request channels in that registry.
#[derive(Clone, Debug)]
pub struct SnapClient {
    peers: SnapPeers,
}

impl SnapClient {
    pub(crate) const fn new(peers: SnapPeers) -> Self {
        Self { peers }
    }

    /// Dispatches a request to any connected `snap/2` peer.
    fn request(&self, request: SnapProtocolMessage) -> SnapResponseFuture {
        let (tx, rx) = oneshot::channel();
        let req = SnapPeerRequest { request, response: tx };
        match self.peers.send_to_any(req) {
            Ok(_peer) => SnapResponseFuture::pending(rx),
            // No peer with snap/2 is available to serve the request.
            Err(_) => SnapResponseFuture::ready_err(RequestError::UnsupportedCapability),
        }
    }
}

impl DownloadClient for SnapClient {
    fn report_bad_message(&self, _peer_id: PeerId) {
        // TODO(snap/2 client): wire peer reputation once the network handle is threaded in.
    }

    fn num_connected_peers(&self) -> usize {
        self.peers.len()
    }
}

impl SnapClientTrait for SnapClient {
    type Output = SnapResponseFuture;

    fn get_account_range_with_priority(
        &self,
        request: GetAccountRangeMessage,
        _priority: Priority,
    ) -> Self::Output {
        self.request(SnapProtocolMessage::GetAccountRange(request))
    }

    fn get_storage_ranges(&self, request: GetStorageRangesMessage) -> Self::Output {
        self.get_storage_ranges_with_priority(request, Priority::Normal)
    }

    fn get_storage_ranges_with_priority(
        &self,
        request: GetStorageRangesMessage,
        _priority: Priority,
    ) -> Self::Output {
        self.request(SnapProtocolMessage::GetStorageRanges(request))
    }

    fn get_byte_codes(&self, request: GetByteCodesMessage) -> Self::Output {
        self.get_byte_codes_with_priority(request, Priority::Normal)
    }

    fn get_byte_codes_with_priority(
        &self,
        request: GetByteCodesMessage,
        _priority: Priority,
    ) -> Self::Output {
        self.request(SnapProtocolMessage::GetByteCodes(request))
    }

    fn get_block_access_lists_with_priority(
        &self,
        request: GetBlockAccessListsMessage,
        _priority: Priority,
    ) -> Self::Output {
        self.request(SnapProtocolMessage::GetBlockAccessLists(request))
    }
}

/// Future resolving to a `snap/2` peer response.
///
/// Either awaits a peer connection's reply or resolves immediately with an error (e.g. when no
/// snap/2 peer is available).
pub struct SnapResponseFuture {
    rx: Option<oneshot::Receiver<PeerRequestResult<SnapResponse>>>,
    err: Option<RequestError>,
}

impl SnapResponseFuture {
    const fn pending(rx: oneshot::Receiver<PeerRequestResult<SnapResponse>>) -> Self {
        Self { rx: Some(rx), err: None }
    }

    const fn ready_err(err: RequestError) -> Self {
        Self { rx: None, err: Some(err) }
    }
}

impl fmt::Debug for SnapResponseFuture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapResponseFuture").finish_non_exhaustive()
    }
}

impl Future for SnapResponseFuture {
    type Output = PeerRequestResult<SnapResponse>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(err) = this.err.take() {
            return Poll::Ready(Err(err));
        }
        match this.rx.as_mut() {
            Some(rx) => match Pin::new(rx).poll(cx) {
                Poll::Ready(Ok(res)) => Poll::Ready(res),
                Poll::Ready(Err(_)) => Poll::Ready(Err(RequestError::ChannelClosed)),
                Poll::Pending => Poll::Pending,
            },
            None => Poll::Ready(Err(RequestError::ChannelClosed)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_eth_wire_types::{snap::BlockAccessListsMessage, BlockAccessLists};
    use reth_network_peers::WithPeerId;
    use tokio::sync::mpsc;

    fn empty_account_range_request(request_id: u64) -> GetAccountRangeMessage {
        GetAccountRangeMessage {
            request_id,
            root_hash: Default::default(),
            starting_hash: Default::default(),
            limit_hash: Default::default(),
            response_bytes: 0,
        }
    }

    fn empty_storage_ranges_request(request_id: u64) -> GetStorageRangesMessage {
        GetStorageRangesMessage {
            request_id,
            root_hash: Default::default(),
            account_hashes: Vec::new(),
            starting_hash: Default::default(),
            limit_hash: Default::default(),
            response_bytes: 0,
        }
    }

    fn empty_byte_codes_request(request_id: u64) -> GetByteCodesMessage {
        GetByteCodesMessage { request_id, hashes: Vec::new(), response_bytes: 0 }
    }

    fn empty_block_access_lists_request(request_id: u64) -> GetBlockAccessListsMessage {
        GetBlockAccessListsMessage { request_id, block_hashes: Vec::new(), response_bytes: 0 }
    }

    #[tokio::test]
    async fn request_without_peers_is_unsupported() {
        let client = SnapClient::new(SnapPeers::default());
        let res = client.get_block_access_lists(empty_block_access_lists_request(1)).await;
        assert!(matches!(res, Err(RequestError::UnsupportedCapability)));
    }

    #[tokio::test]
    async fn registered_peer_receives_block_access_lists_request() {
        let peers = SnapPeers::default();
        let client = SnapClient::new(peers.clone());
        let peer = PeerId::random();
        let (tx, mut rx) = mpsc::unbounded_channel();
        peers.register(peer, tx);

        let response = client.get_block_access_lists(empty_block_access_lists_request(9));
        let request = rx.recv().await.expect("request delivered to registered peer");
        assert!(matches!(
            request.request,
            SnapProtocolMessage::GetBlockAccessLists(GetBlockAccessListsMessage {
                request_id: 9,
                ..
            })
        ));

        request
            .response
            .send(Ok(WithPeerId::new(
                peer,
                SnapResponse::BlockAccessLists(BlockAccessListsMessage {
                    request_id: 9,
                    block_access_lists: BlockAccessLists(Vec::new()),
                }),
            )))
            .unwrap();

        let response = response.await.unwrap();
        assert_eq!(response.peer_id(), peer);
        assert!(matches!(
            response.into_data(),
            SnapResponse::BlockAccessLists(BlockAccessListsMessage { request_id: 9, .. })
        ));
    }

    #[tokio::test]
    async fn stale_peer_sender_is_pruned() {
        let peers = SnapPeers::default();
        let client = SnapClient::new(peers.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        peers.register(PeerId::random(), tx);
        assert_eq!(client.num_connected_peers(), 1);

        drop(rx);

        let res = client.get_block_access_lists(empty_block_access_lists_request(1)).await;
        assert!(matches!(res, Err(RequestError::UnsupportedCapability)));
        assert_eq!(client.num_connected_peers(), 0);
    }

    #[tokio::test]
    async fn response_future_returns_channel_closed_if_sender_drops() {
        let (tx, rx) = oneshot::channel();
        let response = SnapResponseFuture::pending(rx);

        drop(tx);

        let err = response.await.unwrap_err();
        assert_eq!(err, RequestError::ChannelClosed);
    }

    #[tokio::test]
    async fn request_methods_wrap_expected_snap_messages() {
        let peers = SnapPeers::default();
        let client = SnapClient::new(peers.clone());
        let peer = PeerId::random();
        let (tx, mut rx) = mpsc::unbounded_channel();
        peers.register(peer, tx);

        let _account = client.get_account_range(empty_account_range_request(1));
        assert!(matches!(
            rx.recv().await.unwrap().request,
            SnapProtocolMessage::GetAccountRange(GetAccountRangeMessage { request_id: 1, .. })
        ));

        let _storage = client.get_storage_ranges(empty_storage_ranges_request(2));
        assert!(matches!(
            rx.recv().await.unwrap().request,
            SnapProtocolMessage::GetStorageRanges(GetStorageRangesMessage { request_id: 2, .. })
        ));

        let _byte_codes = client.get_byte_codes(empty_byte_codes_request(3));
        assert!(matches!(
            rx.recv().await.unwrap().request,
            SnapProtocolMessage::GetByteCodes(GetByteCodesMessage { request_id: 3, .. })
        ));

        let _bals = client.get_block_access_lists(empty_block_access_lists_request(4));
        assert!(matches!(
            rx.recv().await.unwrap().request,
            SnapProtocolMessage::GetBlockAccessLists(GetBlockAccessListsMessage {
                request_id: 4,
                ..
            })
        ));
    }
}
