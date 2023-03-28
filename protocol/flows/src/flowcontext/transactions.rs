use super::process_queue::ProcessQueue;
use kaspa_consensus_core::tx::TransactionId;
use kaspa_p2p_lib::{
    common::ProtocolError,
    make_message,
    pb::{kaspad_message::Payload, InvTransactionsMessage},
    Router,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const REBROADCAST_INTERVAL: Duration = Duration::from_secs(30);

const BROADCAST_INTERVAL: Duration = Duration::from_millis(500);

const MAX_INV_PER_TX_INV_MSG: usize = 131_072;

#[allow(dead_code)]
pub struct TransactionsSpread {
    last_rebroadcast_time: Instant,
    transaction_ids: ProcessQueue<TransactionId>,
    last_broadcast_time: Instant,
}

impl TransactionsSpread {
    pub fn new() -> Self {
        Self { last_rebroadcast_time: Instant::now(), transaction_ids: ProcessQueue::new(), last_broadcast_time: Instant::now() }
    }

    /// Returns true if the time for a rebroadcast of the mempool high priority transactions has come.
    ///
    /// If true, the instant of the call is registered as the last rebroadcast time.
    pub fn should_rebroadcast(&mut self) -> bool {
        let now = Instant::now();
        if now - self.last_rebroadcast_time < REBROADCAST_INTERVAL {
            return false;
        }
        self.last_rebroadcast_time = now;
        true
    }

    /// Add the given transactions IDs to a set of IDs to broadcast. The IDs will be broadcasted to all peers
    /// within transaction Inv messages.
    ///
    /// The broadcast itself may happen only during a subsequent call to this function since it is done at most
    /// every [`BROADCAST_INTERVAL`] milliseconds or when the queue length is larger than the Inv message
    /// capacity.
    pub async fn broadcast(
        &mut self,
        transaction_ids: Vec<TransactionId>,
    ) -> Result<(), ProtocolError> {
        self.transaction_ids.enqueue_chunk(transaction_ids.into_iter());

        let now = Instant::now();
        if now - self.last_broadcast_time < BROADCAST_INTERVAL && self.transaction_ids.len() < MAX_INV_PER_TX_INV_MSG {
            return Ok(());
        }

        while !self.transaction_ids.is_empty() {
            // FIXME
            // let ids = self.transaction_ids.drain(self.transaction_ids.len().min(MAX_INV_PER_TX_INV_MSG)).map(|x| x.into()).collect();
            // if !self.hub.clone().broadcast(make_message!(Payload::InvTransactions, InvTransactionsMessage { ids })).await {
            //     return Err(ProtocolError::ConnectionClosed);
            // }
        }

        self.last_broadcast_time = Instant::now();
        Ok(())
    }
}

impl Default for TransactionsSpread {
    fn default() -> Self {
        Self::new()
    }
}
