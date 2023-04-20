use kaspa_utils::peer_id::PeerId;
use std::net::SocketAddr;

use crate::Router;

#[derive(Debug)]
pub struct Peer {
    identity: PeerId,
    net_address: SocketAddr,
    is_outbound: bool,
}

impl Peer {
    pub fn new(identity: PeerId, net_address: SocketAddr, is_outbound: bool) -> Self {
        Self { identity, net_address, is_outbound }
    }

    /// Internal identity of this peer
    pub fn identity(&self) -> PeerId {
        self.identity
    }

    /// The socket address of this peer
    pub fn net_address(&self) -> SocketAddr {
        self.net_address
    }

    /// Indicates whether this connection is an outbound connection
    pub fn is_outbound(&self) -> bool {
        self.is_outbound
    }
}

impl From<&Router> for Peer {
    fn from(router: &Router) -> Self {
        Self::new(router.identity(), router.net_address(), router.is_outbound())
    }
}
