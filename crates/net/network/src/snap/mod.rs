//! The `snap/2` satellite `RLPx` sub-protocol (EIP-8189).
//!
//! snap/2 is negotiated as its own `RLPx` capability and multiplexed alongside `eth` on the same
//! connection. It serves bulk state ranges and block access lists (BALs) for snap sync; trie nodes
//! (`0x06`/`0x07`) are not part of snap/2.

pub mod bal;

mod client;
pub use client::{SnapClient, SnapResponseFuture};

mod connection;
pub use connection::SnapConnection;

mod peers;
use peers::SnapPeers;

pub mod sync;
pub mod verify;

use crate::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_eth_wire::{
    capability::SharedCapabilities, multiplex::ProtocolConnection, protocol::Protocol,
};
use reth_network_api::{Direction, PeerId};
use reth_storage_api::BalProvider;
use std::{fmt, net::SocketAddr};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

/// Announces and serves the `snap/2` satellite protocol, and dispatches its sync client.
///
/// Registered via [`NetworkConfigBuilder::add_rlpx_sub_protocol`](crate::NetworkConfigBuilder),
/// which both advertises the capability and installs the per-connection handler.
#[derive(Debug, Clone)]
pub struct SnapProtocolHandler<P> {
    /// Provider used to serve snap/2 requests.
    provider: P,
    /// Registry of connected snap/2 peers, shared with the [`SnapClient`].
    peers: SnapPeers,
}

impl<P> SnapProtocolHandler<P> {
    /// Creates a new handler backed by the given provider.
    pub fn new(provider: P) -> Self {
        Self { provider, peers: SnapPeers::default() }
    }

    /// Returns a [`SnapClient`] that dispatches requests to peers served by this handler.
    pub fn client(&self) -> SnapClient {
        SnapClient::new(self.peers.clone())
    }
}

impl<P> ProtocolHandler for SnapProtocolHandler<P>
where
    P: BalProvider + Clone + fmt::Debug + Send + Sync + Unpin + 'static,
{
    type ConnectionHandler = SnapConnectionHandler<P>;

    fn on_incoming(&self, _socket_addr: SocketAddr) -> Option<Self::ConnectionHandler> {
        Some(SnapConnectionHandler { provider: self.provider.clone(), peers: self.peers.clone() })
    }

    fn on_outgoing(
        &self,
        _socket_addr: SocketAddr,
        _peer_id: PeerId,
    ) -> Option<Self::ConnectionHandler> {
        Some(SnapConnectionHandler { provider: self.provider.clone(), peers: self.peers.clone() })
    }
}

/// Per-connection handler that negotiates `snap/2` and, once established, serves and dispatches
/// requests.
#[derive(Debug)]
pub struct SnapConnectionHandler<P> {
    provider: P,
    peers: SnapPeers,
}

impl<P> ConnectionHandler for SnapConnectionHandler<P>
where
    P: BalProvider + Clone + fmt::Debug + Send + Sync + Unpin + 'static,
{
    type Connection = SnapConnection<P>;

    fn protocol(&self) -> Protocol {
        Protocol::snap_2()
    }

    fn on_unsupported_by_peer(
        self,
        _supported: &SharedCapabilities,
        _direction: Direction,
        _peer_id: PeerId,
    ) -> OnNotSupported {
        // A peer without snap/2 keeps its eth connection; snap is optional.
        OnNotSupported::KeepAlive
    }

    fn into_connection(
        self,
        _direction: Direction,
        peer_id: PeerId,
        conn: ProtocolConnection,
    ) -> Self::Connection {
        let (tx, rx) = mpsc::unbounded_channel();
        self.peers.register(peer_id, tx);
        SnapConnection::new(
            conn,
            UnboundedReceiverStream::new(rx),
            self.provider,
            peer_id,
            self.peers,
        )
    }
}
