use crate::notifier::DynNotify;

use super::{
    error::{Error, Result},
    notification::Notification,
};
use async_channel::{Receiver, Sender};
use async_trait::async_trait;
use core::fmt::Debug;
use futures::{
    future::FutureExt, // for `.fuse()`
    pin_mut,
    select,
};
use futures_util::stream::StreamExt;
use kaspa_core::trace;
use kaspa_utils::channel::Channel;
use kaspa_utils::triggers::DuplexTrigger;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub type CollectorNotificationChannel<T> = Channel<T>;
pub type CollectorNotificationSender<T> = Sender<T>;
pub type CollectorNotificationReceiver<T> = Receiver<T>;

/// A notification collector, relaying notifications to a [`Notifier`].
///
/// A [`Collector`] is responsible for collecting notifications of
/// a specific form from a specific source, convert them if necessary
/// into `N`s and forward them to the [Notifier] provided
/// to `Collector::start`.
#[async_trait]
pub trait Collector<N>: Send + Sync + Debug
where
    N: Notification,
{
    /// Start collecting notifications for `notifier`
    fn start(self: Arc<Self>, notifier: DynNotify<N>);
    /// Stop collecting notifications
    async fn stop(self: Arc<Self>) -> Result<()>;
}

pub type DynCollector<N> = Arc<dyn Collector<N>>;

/// A notification [`Collector`] that receives `I` from a channel,
/// converts it into a `N` and sends it to a [`DynNotify<N>`].
#[derive(Debug)]
pub struct CollectorFrom<I, N>
where
    N: Notification,
    I: Send + Sync + 'static + Sized + Debug,
{
    recv_channel: CollectorNotificationReceiver<I>,

    /// Has this collector been started?
    is_started: Arc<AtomicBool>,

    collect_shutdown: Arc<DuplexTrigger>,

    _notification: PhantomData<N>,
}

impl<I, N> CollectorFrom<I, N>
where
    N: Notification,
    I: Send + Sync + 'static + Sized + Debug,
    I: Into<N>,
{
    pub fn new(recv_channel: CollectorNotificationReceiver<I>) -> Self {
        Self {
            recv_channel,
            collect_shutdown: Arc::new(DuplexTrigger::new()),
            is_started: Arc::new(AtomicBool::new(false)),
            _notification: PhantomData,
        }
    }

    fn spawn_collecting_task(self: Arc<Self>, notifier: DynNotify<N>) {
        // The task can only be spawned once
        if self.is_started.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return;
        }
        let collect_shutdown = self.collect_shutdown.clone();
        let recv_channel = self.recv_channel.clone();

        workflow_core::task::spawn(async move {
            trace!("[Collector] collecting_task start");

            let shutdown = collect_shutdown.request.listener.clone().fuse();
            pin_mut!(shutdown);

            let notifications = recv_channel.fuse();
            pin_mut!(notifications);

            loop {
                select! {
                    _ = shutdown => { break; }
                    notification = notifications.next().fuse() => {
                        match notification {
                            Some(notification) => {
                                match notifier.notify(notification.into()) {
                                    Ok(_) => (),
                                    Err(err) => {
                                        trace!("[Collector] notification sender error: {:?}", err);
                                    },
                                }
                            },
                            None => {
                                trace!("[Collector] notifications returned None. This should never happen");
                            }
                        }
                    }
                }
            }
            collect_shutdown.response.trigger.trigger();
            trace!("[Collector] collecting_task end");
        });
    }

    async fn stop_collecting_task(self: &Arc<Self>) -> Result<()> {
        if self.is_started.compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return Err(Error::AlreadyStoppedError);
        }
        self.collect_shutdown.request.trigger.trigger();
        self.collect_shutdown.response.listener.clone().await;
        Ok(())
    }
}

#[async_trait]
impl<I, N> Collector<N> for CollectorFrom<I, N>
where
    N: Notification,
    I: Send + Sync + 'static + Sized + Debug,
    I: Into<N>,
{
    fn start(self: Arc<Self>, notifier: DynNotify<N>) {
        self.spawn_collecting_task(notifier);
    }

    async fn stop(self: Arc<Self>) -> Result<()> {
        self.stop_collecting_task().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        events::EventType,
        notifier::test_helpers::NotifyMock,
        subscription::single::{OverallSubscription, UtxosChangedSubscription, VirtualSelectedParentChainChangedSubscription},
    };
    use derive_more::Display;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum IncomingNotification {
        A,
        B,
    }

    #[derive(Clone, Debug, Display, PartialEq, Eq)]
    enum OutgoingNotification {
        A,
        B,
    }

    impl From<IncomingNotification> for OutgoingNotification {
        fn from(value: IncomingNotification) -> Self {
            match value {
                IncomingNotification::A => OutgoingNotification::A,
                IncomingNotification::B => OutgoingNotification::B,
            }
        }
    }

    impl crate::notification::Notification for OutgoingNotification {
        fn apply_overall_subscription(&self, _: &OverallSubscription) -> Option<Self> {
            unimplemented!()
        }

        fn apply_virtual_selected_parent_chain_changed_subscription(
            &self,
            _: &VirtualSelectedParentChainChangedSubscription,
        ) -> Option<Self> {
            unimplemented!()
        }

        fn apply_utxos_changed_subscription(&self, _: &UtxosChangedSubscription) -> Option<Self> {
            unimplemented!()
        }

        fn event_type(&self) -> EventType {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn test_collector_from() {
        let incoming = Channel::default();
        let collector: Arc<CollectorFrom<IncomingNotification, OutgoingNotification>> =
            Arc::new(CollectorFrom::new(incoming.receiver()));
        let outgoing = Channel::default();
        let notifier = Arc::new(NotifyMock::new(outgoing.sender()));
        collector.clone().start(notifier);

        assert!(incoming.send(IncomingNotification::A).await.is_ok());
        assert!(incoming.send(IncomingNotification::B).await.is_ok());
        assert!(incoming.send(IncomingNotification::A).await.is_ok());

        assert_eq!(outgoing.recv().await.unwrap(), OutgoingNotification::A);
        assert_eq!(outgoing.recv().await.unwrap(), OutgoingNotification::B);
        assert_eq!(outgoing.recv().await.unwrap(), OutgoingNotification::A);

        assert!(collector.stop().await.is_ok());
    }
}
