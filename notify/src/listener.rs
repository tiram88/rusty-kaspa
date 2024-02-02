use std::fmt::Debug;
extern crate derive_more;
use crate::{
    error::Result,
    subscription::{context::SubscriptionContext, DynSubscription, MutateSingle, MutationOutcome, MutationPolicies},
};

use super::{
    connection::Connection,
    events::EventArray,
    subscription::{array::ArrayBuilder, Mutation},
};

pub type ListenerId = u64;

/// A listener of [`super::notifier::Notifier`] notifications.
#[derive(Debug)]
pub(crate) struct Listener<C>
where
    C: Connection,
{
    connection: C,
    pub(crate) subscriptions: EventArray<DynSubscription>,
}

impl<C> Listener<C>
where
    C: Connection,
{
    pub fn new(id: ListenerId, connection: C) -> Self {
        Self { connection, subscriptions: ArrayBuilder::single(id) }
    }

    pub fn connection(&self) -> C {
        self.connection.clone()
    }

    /// Apply a mutation to the subscriptions.
    ///
    /// Return Some mutations to be applied to a compounded state if any change occurred
    /// in the subscription state and None otherwise.
    pub fn mutate(
        &mut self,
        mutation: Mutation,
        policies: MutationPolicies,
        context: &SubscriptionContext,
    ) -> Result<MutationOutcome> {
        let event_type = mutation.event_type();
        self.subscriptions[event_type].mutate(mutation, policies, context)
    }

    pub fn close(&self) {
        if !self.is_closed() {
            self.connection.close();
        }
    }

    pub fn is_closed(&self) -> bool {
        self.connection.is_closed()
    }
}
