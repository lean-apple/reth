//! Registry of connected `snap/2` peers and the per-peer request channel.

use parking_lot::Mutex;
use reth_eth_wire_types::snap::SnapProtocolMessage;
use reth_network_api::PeerId;
use reth_network_p2p::{error::PeerRequestResult, snap::client::SnapResponse};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{mpsc, oneshot};

/// A client request routed to a single `snap/2` peer connection.
#[derive(Debug)]
pub(crate) struct SnapPeerRequest {
    /// The request message to send to the peer.
    pub(crate) request: SnapProtocolMessage,
    /// Channel the peer connection uses to return the correlated response.
    pub(crate) response: oneshot::Sender<PeerRequestResult<SnapResponse>>,
}

/// Shared registry mapping each connected `snap/2` peer to its connection command channel.
///
/// Shared between the [`SnapProtocolHandler`](super::SnapProtocolHandler) (which registers a sender
/// per connection) and the [`SnapClient`](super::SnapClient) (which dispatches requests).
#[derive(Clone, Debug, Default)]
pub(crate) struct SnapPeers {
    inner: Arc<Mutex<HashMap<PeerId, mpsc::UnboundedSender<SnapPeerRequest>>>>,
}

impl SnapPeers {
    /// Registers the command channel for a newly negotiated `snap/2` peer.
    pub(crate) fn register(&self, peer: PeerId, tx: mpsc::UnboundedSender<SnapPeerRequest>) {
        self.inner.lock().insert(peer, tx);
    }

    /// Removes a peer once its connection closes.
    pub(crate) fn remove(&self, peer: &PeerId) {
        self.inner.lock().remove(peer);
    }

    /// Number of currently connected `snap/2` peers.
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Dispatches the request to any connected `snap/2` peer, pruning channels whose connection has
    /// gone away. Returns the request back if no peer accepted it.
    pub(crate) fn send_to_any(&self, mut req: SnapPeerRequest) -> Result<PeerId, SnapPeerRequest> {
        let mut guard = self.inner.lock();
        let peers: Vec<PeerId> = guard.keys().copied().collect();
        for peer in peers {
            let Some(tx) = guard.get(&peer).cloned() else { continue };
            match tx.send(req) {
                Ok(()) => return Ok(peer),
                Err(mpsc::error::SendError(returned)) => {
                    guard.remove(&peer);
                    req = returned;
                }
            }
        }
        Err(req)
    }
}
