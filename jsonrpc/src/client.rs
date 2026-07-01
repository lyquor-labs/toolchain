use derive_more::Debug;
use futures::Stream;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::core::client::SubscriptionClientT;
use jsonrpsee_client_transport::ws::{Url, WsHandshakeError};
use jsonrpsee_ws_client::{WsClient, WsClientBuilder};
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;
use std::pin::Pin;
use std::result::Result;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_util::future::FutureExt;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::Instrument;
use typed_builder::TypedBuilder;

use crate::types::{JsonRPCMsgT, JsonRPCSubscriptionMsgT};

mod connector;

type RawSubscription = jsonrpsee::core::client::Subscription<Box<RawValue>>;

pub use jsonrpsee::core::client::Error;

/// Configuration for the reconnecting JSON-RPC WebSocket client.
#[derive(Debug, Clone, TypedBuilder)]
pub struct ClientConfig {
    pub url: Url,
    #[builder(default = Duration::from_millis(5000))]
    pub connection_timeout: Duration,
    #[builder(default = Duration::from_millis(30000))]
    pub request_timeout: Duration,
    #[builder(default = Duration::from_millis(1000))]
    pub reconnect_interval: Duration,
    #[builder(default = 8)]
    pub subscription_buffer_size: usize,
}

impl ClientConfig {
    /// Creates a new JSON-RPC client with this configuration.
    /// Returns a ClientHandle that can be used to make requests and subscriptions.
    pub fn into_client(self, shutdown: CancellationToken) -> ClientHandle {
        ClientHandle::new(ClientInner::new(self, shutdown))
    }
}

/// Cloneable handle for JSON-RPC requests and subscriptions.
#[derive(Clone)]
pub struct ClientHandle {
    inner: Arc<ClientInner>,
}

impl ClientHandle {
    fn new(inner: Arc<ClientInner>) -> Self {
        Self { inner }
    }

    /// Return the configured endpoint URL.
    pub fn endpoint_url(&self) -> &Url {
        &self.inner.config.url
    }

    /// Send one JSON-RPC request and decode the response into `R`.
    #[tracing::instrument(level = "trace", skip(self, msg), fields(method = T::method()))]
    pub async fn request<T: JsonRPCMsgT, R: DeserializeOwned + Send + 'static>(&self, msg: T) -> Result<R, Error> {
        let client = self.inner.wait_for_connected().await?;
        let response: Box<RawValue> = client.request(T::method(), msg.into_params()).await?;
        let result: R = serde_json::from_str(response.get())?;
        Ok(result)
    }

    /// Start a JSON-RPC subscription and return a stream of decoded updates.
    #[tracing::instrument(level = "trace", skip(self, msg), fields(method = T::method()))]
    pub async fn subscribe<T: JsonRPCSubscriptionMsgT + 'static, R: DeserializeOwned + Send + 'static>(
        &self, msg: T,
    ) -> Result<Subscription<R>, Error> {
        let unsubscribe = self.inner.shutdown.child_token();

        let subscription = self.inner.initialize_subscription(msg).await?;
        let (sender, receiver) = mpsc::channel(self.inner.config.subscription_buffer_size);

        self.inner
            .task_tracker
            .spawn(ClientInner::subscription_loop::<T, R>(subscription, sender, unsubscribe.clone()).in_current_span());

        Ok(Subscription { receiver, unsubscribe })
    }

    /// Wait for all client background tasks to stop.
    pub async fn wait_for_shutdown(self) {
        self.inner.task_tracker.wait().await;
    }
}

#[derive(Debug)]
enum ClientState {
    Disconnected,
    Connecting,
    Connected(Arc<WsClient>),
}

struct ClientInner {
    config: ClientConfig,
    state: watch::Receiver<ClientState>,
    shutdown: CancellationToken,
    task_tracker: TaskTracker,
}

impl ClientInner {
    fn new(config: ClientConfig, shutdown: CancellationToken) -> Arc<Self> {
        let (state_updater, state) = watch::channel(ClientState::Disconnected);
        let task_tracker = TaskTracker::new();
        let shutdown_task_tracker = task_tracker.clone();

        tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                shutdown.cancelled().await;
                shutdown_task_tracker.close();
            }
        });

        task_tracker.spawn(
            Self::connection_loop(state_updater, config.clone())
                .in_current_span()
                .with_cancellation_token_owned(shutdown.clone()),
        );

        Arc::new(Self {
            config,
            state,
            shutdown,
            task_tracker,
        })
    }

    async fn connect(config: &ClientConfig) -> Result<WsClient, WsHandshakeError> {
        let (tx, rx) = connector::connect(&config.url, config.connection_timeout).await?;
        Ok(WsClientBuilder::default()
            .request_timeout(config.request_timeout)
            .build_with_transport(tx, rx))
    }

    async fn connection_loop(state: watch::Sender<ClientState>, config: ClientConfig) {
        loop {
            // Attempt to connect to the WebSocket endpoint
            'conn: {
                let _ = state.send(ClientState::Connecting);
                let client = match Self::connect(&config).await {
                    Ok(client) => {
                        let client = Arc::new(client);
                        let _ = state.send(ClientState::Connected(client.clone()));
                        client
                    }
                    Err(e) => {
                        tracing::error!(
                            "Failed to connect to WebSocket: {e}, retrying in {:?}",
                            config.reconnect_interval
                        );
                        break 'conn;
                    }
                };

                client.on_disconnect().await;
            }
            tracing::debug!("WebSocket connection closed, retrying...");
            let _ = state.send(ClientState::Disconnected);

            tokio::time::sleep(config.reconnect_interval).await;
        }
    }

    #[inline(always)]
    fn wait_for_connected(&self) -> impl Future<Output = Result<Arc<WsClient>, Error>> + 'static {
        let mut state = self.state.clone();
        let connection_timeout = self.config.connection_timeout;
        async move {
            let c = state
                .wait_for(|state| matches!(state, ClientState::Connected(_)))
                .timeout(connection_timeout)
                .await
                .map_err(|_| Error::RequestTimeout)?
                .map_err(|_| Error::Custom("Client state channel closed".to_string()))?;
            match &*c {
                ClientState::Connected(client) => Ok(client.clone()),
                _ => unreachable!("wait_for_connected should return a connected state"),
            }
        }
    }

    async fn initialize_subscription<T: JsonRPCSubscriptionMsgT + 'static>(
        &self, request: T,
    ) -> Result<RawSubscription, Error> {
        let client = self.wait_for_connected().await?;
        let response: RawSubscription = client
            .subscribe(T::method(), request.into_params(), T::unsubscribe_method())
            .await?;
        Ok(response)
    }

    async fn subscription_loop<T: JsonRPCSubscriptionMsgT + 'static, R: DeserializeOwned + Send + 'static>(
        mut subscription: RawSubscription, sender: mpsc::Sender<R>, unsubscribe: CancellationToken,
    ) {
        let mut should_unsubscribe = false;

        loop {
            tokio::select! {
                _ = unsubscribe.cancelled() => {
                    tracing::debug!("Subscription loop received shutdown signal, unsubscribing...");
                    should_unsubscribe = true;
                    break;
                }
                payload = subscription.next() => {
                    match payload {
                        Some(payload) => {
                            let payload: Result<R, serde_json::Error> = payload.and_then(|p| serde_json::from_str(p.get()));

                            match payload {
                                Ok(payload) => {
                                    if sender.send(payload).await.is_err() {
                                        tracing::debug!("Subscriber has been dropped, ending subscription");
                                        should_unsubscribe = true;
                                        break;
                                    }
                                }
                                Err(e) => {
                                    // Skip this iteration on error
                                    tracing::warn!("Failed to deserialize subscription payload: {:?}", e);
                                }
                            }
                        }
                        None => {
                            break;
                        }
                    }
                }
            }
        }

        drop(sender); // Close the sender to signal the subscription stream is ending

        if should_unsubscribe {
            if let Err(err) = subscription.unsubscribe().await {
                tracing::debug!("Failed to unsubscribe from {}: {:?}", T::method(), err);
            }
        } else if let Some(reason) = subscription.close_reason() {
            tracing::debug!("Subscription closed: {:?}", reason);
        }

        tracing::debug!("Subscription loop ended for {}", T::method());
    }
}

/// Stream handle for one JSON-RPC subscription.
pub struct Subscription<R: DeserializeOwned + Send + 'static> {
    receiver: mpsc::Receiver<R>,
    unsubscribe: CancellationToken,
}

impl<R: DeserializeOwned + Send + 'static> Subscription<R> {
    /// Receive the next subscription item.
    pub async fn next(&mut self) -> Option<R> {
        self.receiver.recv().await
    }

    /// Request remote unsubscription and stop the local subscription loop.
    pub fn unsubscribe(self) {
        self.unsubscribe.cancel();
    }
}

impl<R: DeserializeOwned + Send + 'static> Stream for Subscription<R> {
    type Item = R;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Pin::new(&mut this.receiver).poll_recv(cx)
    }
}

impl<R: DeserializeOwned + Send + 'static> Drop for Subscription<R> {
    fn drop(&mut self) {
        self.unsubscribe.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EthAccounts, EthAccountsResp};
    use derive_more::Debug;
    use jsonrpsee::core::middleware::RpcServiceBuilder;
    use jsonrpsee::core::traits::ToRpcParams;
    use jsonrpsee::server::Server;
    use jsonrpsee::{RpcModule, rpc_params};
    use lyquor_test::test;
    use serde::Deserialize;
    use std::net::SocketAddr;
    use std::vec;

    async fn run_server<Fn>(f: Fn) -> anyhow::Result<SocketAddr>
    where
        Fn: FnOnce(&mut RpcModule<()>) -> anyhow::Result<()>,
    {
        let rpc_middleware = RpcServiceBuilder::new().rpc_logger(1024);
        let server = Server::builder()
            .set_rpc_middleware(rpc_middleware)
            .build("127.0.0.1:0")
            .await?;
        let addr = server.local_addr()?;
        let mut module = RpcModule::new(());
        f(&mut module)?;

        let handle = server.start(module);

        // In this example we don't care about doing shutdown so let's it run forever.
        // You may use the `ServerHandle` to shut it down or manage it yourself.
        tokio::spawn(handle.stopped());

        Ok(addr)
    }

    async fn wait_for_state(state: &mut watch::Receiver<ClientState>, expected: fn(&ClientState) -> bool) {
        loop {
            if expected(&state.borrow()) {
                return;
            }

            if state.changed().await.is_err() {
                return;
            }
        }
    }

    #[test(tokio::test)]
    async fn test_client_request() -> anyhow::Result<()> {
        let addr = run_server(|module| {
            module.register_method("eth_accounts", |_, _, _| {
                vec!["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266".to_string()]
            })?;
            Ok(())
        })
        .await?;

        let shutdown = CancellationToken::new();

        let config = ClientConfig::builder()
            .url(Url::parse(&format!("ws://{addr}")).unwrap())
            .build();
        let client = config.into_client(shutdown.clone());

        {
            let span = tracing::span!(tracing::Level::INFO, "test_client_request");
            let _enter = span.enter();

            let resp: EthAccountsResp = client
                .request(EthAccounts)
                .in_current_span()
                .await
                .expect("Failed to get accounts");

            assert_eq!(
                resp.0,
                vec![lyquor_primitives::address!(
                    "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
                )]
            );
        }

        let mut state = client.inner.state.clone();
        shutdown.cancel();

        wait_for_state(&mut state, |state| matches!(state, ClientState::Disconnected)).await;
        client.wait_for_shutdown().await;

        Ok(())
    }

    use lyquor_primitives::Serialize;
    /// Test-only subscription request used by JSON-RPC client tests.
    #[derive(Serialize, Debug, Clone)]
    pub struct EthSubscribeTest(String);

    impl JsonRPCSubscriptionMsgT for EthSubscribeTest {
        fn method() -> &'static str {
            "eth_subscribe"
        }

        fn unsubscribe_method() -> &'static str {
            "eth_unsubscribe"
        }

        fn into_params(self) -> impl ToRpcParams + Send + 'static {
            rpc_params![self.0]
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    struct SubscriptionUpdate {
        number: u64,
    }

    #[test(tokio::test)]
    async fn test_client_subscription() -> anyhow::Result<()> {
        let addr = run_server(move |module| {
            module.register_subscription(
                "eth_subscribe",
                "eth_subscription",
                "eth_unsubscribe",
                |params, pending, _ctx, _| async move {
                    let _new_heads: String = params.one().unwrap();
                    let sink = pending.accept().await?;
                    let update: SubscriptionUpdate = SubscriptionUpdate { number: 1 };
                    let msg = serde_json::value::to_raw_value(&update).unwrap();
                    sink.send(msg).await?;
                    sink.closed().await;
                    Ok(())
                },
            )?;
            Ok(())
        })
        .await?;

        let shutdown = CancellationToken::new();
        let config = ClientConfig::builder()
            .url(Url::parse(&format!("ws://{addr}")).unwrap())
            .build();
        let client = config.into_client(shutdown.clone());
        let mut sub: Subscription<SubscriptionUpdate> = client
            .subscribe(EthSubscribeTest("newHeads".to_string()))
            .await
            .expect("Failed to subscribe");

        let update = sub.next().await.expect("Subscription stream closed");
        assert_eq!(update, SubscriptionUpdate { number: 1 });

        shutdown.cancel();

        let next = sub.next().await;
        assert!(next.is_none(), "subscription stream should close after shutdown");

        Ok(())
    }
}
