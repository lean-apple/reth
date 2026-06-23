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
/// Obtained from [`SnapProtocolHandler::client`](super::SnapProtocolHandler::client); cloning
/// shares the same peer registry.
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
    use reth_eth_wire_types::snap::GetBlockAccessListsMessage;

    #[tokio::test]
    async fn request_without_peers_is_unsupported() {
        let client = SnapClient::new(SnapPeers::default());
        let res = client
            .get_block_access_lists(GetBlockAccessListsMessage {
                request_id: 1,
                block_hashes: vec![],
                response_bytes: 0,
            })
            .await;
        assert!(matches!(res, Err(RequestError::UnsupportedCapability)));
    }
}
