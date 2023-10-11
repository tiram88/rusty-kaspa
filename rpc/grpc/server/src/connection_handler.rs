use crate::{
    collector::{GrpcServiceCollector, GrpcServiceConverter},
    connection::Connection,
    manager::ManagerEvent,
};
use futures::{FutureExt, Stream};
use kaspa_core::{debug, info, warn};
use kaspa_grpc_core::{
    protowire::{
        rpc_server::{Rpc, RpcServer},
        KaspadRequest, KaspadResponse,
    },
    RPC_MAX_MESSAGE_SIZE,
};
use kaspa_notify::{connection::ChannelType, events::EVENT_TYPE_ARRAY, notifier::Notifier, subscriber::Subscriber};
use kaspa_rpc_core::{
    api::rpc::DynRpcService,
    notify::{channel::NotificationChannel, connection::ChannelConnection},
    Notification, RpcResult,
};
use kaspa_utils::networking::NetAddress;
use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::mpsc::{channel as mpsc_channel, Sender as MpscSender};
use tokio::{
    sync::oneshot::{channel as oneshot_channel, Sender as OneshotSender},
    time::timeout,
};
use tokio_stream::{wrappers::ReceiverStream, StreamExt};
use tonic::{codec::CompressionEncoding, transport::Server as TonicServer, Request, Response};

/// A protowire gRPC connections handler.
#[derive(Clone)]
pub struct ConnectionHandler {
    manager_sender: MpscSender<ManagerEvent>,
    core_service: DynRpcService,
    notifier: Arc<Notifier<Notification, Connection>>,
    running: Arc<AtomicBool>,
}

const GRPC_SERVER: &str = "grpc-server";

impl ConnectionHandler {
    pub(crate) fn new(
        manager_sender: MpscSender<ManagerEvent>,
        core_service: DynRpcService,
        core_notifier: Arc<Notifier<Notification, ChannelConnection>>,
    ) -> Self {
        // Prepare core objects
        let core_channel = NotificationChannel::default();
        let core_listener_id =
            core_notifier.register_new_listener(ChannelConnection::new(core_channel.sender(), ChannelType::Closable));

        // Prepare internals
        let core_events = EVENT_TYPE_ARRAY[..].into();
        let converter = Arc::new(GrpcServiceConverter::new());
        let collector = Arc::new(GrpcServiceCollector::new(GRPC_SERVER, core_channel.receiver(), converter));
        let subscriber = Arc::new(Subscriber::new(GRPC_SERVER, core_events, core_notifier, core_listener_id));
        let notifier: Arc<Notifier<Notification, Connection>> =
            Arc::new(Notifier::new(GRPC_SERVER, core_events, vec![collector], vec![subscriber], 10));
        let running = Default::default();

        Self { manager_sender, core_service, notifier, running }
    }

    /// Launches a gRPC server listener loop
    pub(crate) fn serve(self: &Arc<Self>, serve_address: NetAddress) -> OneshotSender<()> {
        let (termination_sender, termination_receiver) = oneshot_channel::<()>();
        let connection_handler = self.clone();
        info!("GRPC Server starting on: {}", serve_address);
        tokio::spawn(async move {
            let protowire_server = RpcServer::from_arc(connection_handler.clone())
                .send_compressed(CompressionEncoding::Gzip)
                .accept_compressed(CompressionEncoding::Gzip)
                .max_decoding_message_size(RPC_MAX_MESSAGE_SIZE);

            // TODO: check whether we should set tcp_keepalive
            let serve_result = TonicServer::builder()
                .add_service(protowire_server)
                .serve_with_shutdown(serve_address.into(), termination_receiver.map(drop))
                .await;

            match serve_result {
                Ok(_) => info!("GRPC Server stopped on: {}", serve_address),
                Err(err) => panic!("GRPC Server {serve_address} stopped with error: {err:?}"),
            }
        });
        termination_sender
    }

    #[inline(always)]
    pub fn notifier(&self) -> Arc<Notifier<Notification, Connection>> {
        self.notifier.clone()
    }

    pub fn start(&self) {
        debug!("GRPC: Starting the connection handler");

        // Start the internal notifier
        self.notifier().start();

        // Accept new incoming connections
        self.running.store(true, Ordering::SeqCst);
    }

    pub async fn stop(&self) -> RpcResult<()> {
        debug!("GRPC: Stopping the connection handler");

        // Refuse new incoming connections
        self.running.store(false, Ordering::SeqCst);

        // Wait for the internal notifier to stop
        // Note that this requires the core service it is listening to to have closed its listener
        match timeout(Duration::from_millis(100), self.notifier().join()).await {
            Ok(_) => {
                debug!("GRPC: Stopped the connection handler");
            }
            Err(_) => {
                warn!("GRPC: Stopped the connection handler forcefully");
            }
        }

        Ok(())
    }

    pub fn outgoing_route_channel_size() -> usize {
        128
    }
}

impl Drop for ConnectionHandler {
    fn drop(&mut self) {
        debug!("GRPC: dropping connection handler, refs {}", Arc::strong_count(&self.running));
    }
}

#[tonic::async_trait]
impl Rpc for ConnectionHandler {
    type MessageStreamStream = Pin<Box<dyn Stream<Item = Result<KaspadResponse, tonic::Status>> + Send + Sync + 'static>>;

    /// Handle the new arriving client connection
    async fn message_stream(
        &self,
        request: Request<tonic::Streaming<KaspadRequest>>,
    ) -> Result<Response<Self::MessageStreamStream>, tonic::Status> {
        const SERVICE_IS_DOWN: &str = "The gRPC service is down";

        if !self.running.load(Ordering::SeqCst) {
            return Err(tonic::Status::new(tonic::Code::Unavailable, SERVICE_IS_DOWN));
        }

        let remote_address = request.remote_addr().ok_or_else(|| {
            tonic::Status::new(tonic::Code::InvalidArgument, "Incoming connection opening request has no remote address")
        })?;

        // Bound to max allowed connections
        let (is_full_sender, is_full_receiver) = oneshot_channel();
        match self.manager_sender.send(ManagerEvent::IsFull(is_full_sender)).await {
            Ok(_) => match is_full_receiver.await {
                Ok(true) => {
                    return Err(tonic::Status::new(
                        tonic::Code::PermissionDenied,
                        "The gRPC service has reached full capacity and accepts no new connection",
                    ));
                }
                Ok(false) => {}
                Err(_) => {
                    return Err(tonic::Status::new(tonic::Code::Unavailable, SERVICE_IS_DOWN));
                }
            },
            Err(_) => {
                return Err(tonic::Status::new(tonic::Code::Unavailable, SERVICE_IS_DOWN));
            }
        }

        debug!("GRPC: incoming message stream from {:?}", remote_address);

        // Build the in/out pipes
        let (outgoing_route, outgoing_receiver) = mpsc_channel(Self::outgoing_route_channel_size());
        let incoming_stream = request.into_inner();

        // Build the connection object
        let connection = Connection::new(
            remote_address,
            self.core_service.clone(),
            self.manager_sender.clone(),
            self.notifier(),
            incoming_stream,
            outgoing_route,
        );

        // Notify the central Manager about the new connection
        self.manager_sender
            .send(ManagerEvent::NewConnection(connection))
            .await
            .expect("manager receiver should never drop before senders");

        // Give tonic a receiver stream (messages sent to it will be forwarded to the client)
        Ok(Response::new(Box::pin(ReceiverStream::new(outgoing_receiver).map(Ok)) as Self::MessageStreamStream))
    }
}
