use std::{convert::Infallible, net::SocketAddr, str::FromStr, sync::Arc};

use anyhow::Context as _;
use bytes::Bytes;
use http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri,
    header::{
        CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER,
        TRANSFER_ENCODING, UPGRADE,
    },
};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{
    body::Incoming,
    client::conn::http1 as client_http1,
    server::conn::http1 as server_http1,
    service::service_fn,
    upgrade::{OnUpgrade, Upgraded},
};
use hyper_util::rt::TokioIo;
use lyquor_primitives::{LyquidID, NodeID};
use lyquor_proto::node::v1::{GetNodeInfoRequest, node_service_client::NodeServiceClient};
use reqwest::Url;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// Default node endpoint used by `shaker serve`.
pub const DEFAULT_SERVE_ENDPOINT: &str = "ws://localhost:10087/ws";
/// Default local bind address used by `shaker serve`.
pub const DEFAULT_SERVE_LISTEN: &str = "127.0.0.1:8080";

const INFO_PATH: &str = "/lyquid/info";

type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// Options for the local Lyquid virtual-host proxy.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub lyquid_id: LyquidID,
    pub endpoint: String,
    pub listen: SocketAddr,
}

/// Bound `shaker serve` proxy.
pub struct ServeServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    config: Arc<ProxyConfig>,
}

#[derive(Debug, Clone)]
struct ProxyConfig {
    upstream: UpstreamEndpoint,
    virtual_host: String,
    virtual_host_header: HeaderValue,
}

#[derive(Debug, Clone)]
struct UpstreamEndpoint {
    base_url: Url,
    host: String,
    port: u16,
}

struct ResolvedNodeInfo {
    node_id: NodeID,
    version: String,
}

impl ServeServer {
    /// Resolve the target node and bind the local proxy listener.
    pub async fn bind(options: ServeOptions) -> anyhow::Result<Self> {
        let config = ProxyConfig::from_options(&options).await?;
        Self::bind_with_config(config, options.listen).await
    }

    async fn bind_with_config(config: ProxyConfig, listen: SocketAddr) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(listen)
            .await
            .with_context(|| format!("Failed to bind shaker serve listener at `{listen}`"))?;
        let local_addr = listener
            .local_addr()
            .context("Failed to read shaker serve listener address")?;
        Ok(Self {
            listener,
            local_addr,
            config: Arc::new(config),
        })
    }

    /// Local address selected for the proxy listener.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Browser URL for this proxy.
    pub fn local_url(&self) -> String {
        format!("http://{}/", self.local_addr)
    }

    /// Upstream node HTTP endpoint.
    pub fn upstream_base(&self) -> &str {
        self.config.upstream.base_url.as_str().trim_end_matches('/')
    }

    /// Virtual host sent to the node.
    pub fn virtual_host(&self) -> &str {
        &self.config.virtual_host
    }

    /// Serve requests until the shutdown token is cancelled.
    pub async fn run(self, shutdown: CancellationToken) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                accepted = self.listener.accept() => {
                    let (socket, peer_addr) = accepted.context("Failed to accept shaker serve connection")?;
                    let config = self.config.clone();
                    tokio::spawn(async move {
                        if let Err(err) = serve_connection(socket, config).await {
                            tracing::debug!(%peer_addr, error = ?err, "shaker serve connection failed");
                        }
                    });
                }
            }
        }
    }
}

impl ProxyConfig {
    async fn from_options(options: &ServeOptions) -> anyhow::Result<Self> {
        let upstream = UpstreamEndpoint::from_endpoint(&options.endpoint)?;
        let node_info = resolve_node_info(&upstream.base_url).await?;
        crate::warn_if_node_version_differs(&options.endpoint, &node_info.version);
        Self::new(upstream, virtual_host(options.lyquid_id, node_info.node_id))
    }

    fn new(upstream: UpstreamEndpoint, virtual_host: String) -> anyhow::Result<Self> {
        let virtual_host_header = HeaderValue::from_str(&virtual_host)
            .with_context(|| format!("Resolved Lyquid virtual host `{virtual_host}` is not a valid Host header"))?;
        Ok(Self {
            upstream,
            virtual_host,
            virtual_host_header,
        })
    }
}

impl UpstreamEndpoint {
    fn from_endpoint(endpoint: &str) -> anyhow::Result<Self> {
        let base_url = plaintext_http_base_endpoint(endpoint)?;
        let host = base_url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("Node endpoint `{endpoint}` is missing a host"))?
            .to_owned();
        let port = base_url
            .port_or_known_default()
            .ok_or_else(|| anyhow::anyhow!("Node endpoint `{endpoint}` is missing a port"))?;
        Ok(Self { base_url, host, port })
    }
}

/// Convert a localnet node endpoint into the plaintext HTTP listener base.
fn plaintext_http_base_endpoint(endpoint: &str) -> anyhow::Result<Url> {
    let mut url =
        Url::parse(endpoint).map_err(|err| anyhow::anyhow!("Invalid node API endpoint `{endpoint}`: {err}"))?;
    match url.scheme() {
        "ws" => {
            url.set_scheme("http")
                .map_err(|_| anyhow::anyhow!("Failed to convert websocket endpoint `{endpoint}` to HTTP"))?;
        }
        "http" => {}
        "wss" | "https" => {
            anyhow::bail!(
                "`shaker serve` only supports plaintext localnet endpoints in v1; use ws:// or http:// instead of `{}`",
                url.scheme()
            );
        }
        other => anyhow::bail!("Unsupported node API endpoint scheme `{other}`"),
    }
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// Build the Host header accepted by the node's per-Lyquid virtual-host router.
fn virtual_host(lyquid_id: LyquidID, node_id: NodeID) -> String {
    format!("{}.{}", lyquid_id.as_dns_label(), node_id.as_dns_label())
}

async fn resolve_node_info(base_url: &Url) -> anyhow::Result<ResolvedNodeInfo> {
    let (grpc_endpoint, channel) = crate::connect_grpc_api_channel(base_url.as_str(), "NodeService").await?;
    let mut client = NodeServiceClient::new(channel);
    let response = client
        .get_node_info(GetNodeInfoRequest {})
        .await
        .with_context(|| format!("Failed to get node info from `{grpc_endpoint}`"))?
        .into_inner();
    let node_id = response
        .node_id
        .context("NodeService GetNodeInfo response did not include node_id")?;
    let node_id =
        NodeID::try_from(node_id).map_err(|err| anyhow::anyhow!("Invalid node_id returned by GetNodeInfo: {err}"))?;
    Ok(ResolvedNodeInfo {
        node_id,
        version: response.version,
    })
}

async fn serve_connection(socket: TcpStream, config: Arc<ProxyConfig>) -> anyhow::Result<()> {
    let service = service_fn(move |req| proxy_request(req, config.clone()));
    server_http1::Builder::new()
        .serve_connection(TokioIo::new(socket), service)
        .with_upgrades()
        .await
        .context("HTTP proxy connection failed")
}

async fn proxy_request(req: Request<Incoming>, config: Arc<ProxyConfig>) -> Result<Response<ProxyBody>, Infallible> {
    if req.uri().path() == INFO_PATH {
        return match forward_info_request(req, &config).await {
            Ok(response) => Ok(response),
            Err(err) => {
                tracing::warn!(error = ?err, "shaker serve /lyquid/info request failed");
                Ok(text_response(StatusCode::BAD_GATEWAY, "Bad gateway"))
            }
        };
    }

    match forward_request(req, &config).await {
        Ok(response) => Ok(response),
        Err(err) => {
            tracing::warn!(error = ?err, "shaker serve request failed");
            Ok(text_response(StatusCode::BAD_GATEWAY, "Bad gateway"))
        }
    }
}

async fn forward_request(mut req: Request<Incoming>, config: &ProxyConfig) -> anyhow::Result<Response<ProxyBody>> {
    let upgrade = is_upgrade_request(&req);
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));
    let upstream_req = rewrite_request(req, config, upgrade)?;

    let mut response = send_upstream_request(upstream_req, config).await?;
    let upgraded = client_upgrade.is_some() && response.status() == StatusCode::SWITCHING_PROTOCOLS;
    rewrite_response_headers(response.headers_mut(), upgraded);
    if upgraded && let Some(client_upgrade) = client_upgrade {
        let upstream_upgrade = hyper::upgrade::on(&mut response);
        tokio::spawn(tunnel_upgrades(client_upgrade, upstream_upgrade));
    }

    Ok(response.map(http_body_util::BodyExt::boxed))
}

async fn forward_info_request(req: Request<Incoming>, config: &ProxyConfig) -> anyhow::Result<Response<ProxyBody>> {
    let rewrite_info = req.method() == Method::GET;
    let upstream_req = rewrite_request(req, config, false)?;
    let mut response = send_upstream_request(upstream_req, config).await?;
    rewrite_response_headers(response.headers_mut(), false);

    if !rewrite_info || !response.status().is_success() {
        return Ok(response.map(http_body_util::BodyExt::boxed));
    }

    let (mut parts, body) = response.into_parts();
    let bytes = body
        .collect()
        .await
        .context("Failed to read upstream /lyquid/info response body")?
        .to_bytes();
    let Ok(mut info) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(Response::from_parts(parts, full_body(bytes)));
    };
    let Some(info_object) = info.as_object_mut() else {
        return Ok(Response::from_parts(parts, full_body(bytes)));
    };

    let node_base_url = config.upstream.base_url.as_str().trim_end_matches('/').to_owned();
    info_object.insert("node_base_url".to_owned(), serde_json::Value::String(node_base_url));
    let rewritten = serde_json::to_vec(&info).context("Failed to serialize rewritten /lyquid/info response")?;
    let length = rewritten.len().to_string();
    parts
        .headers
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    parts.headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&length).expect("length should be a valid header value"),
    );

    Ok(Response::from_parts(parts, full_body(rewritten.into())))
}

async fn send_upstream_request(
    upstream_req: Request<Incoming>, config: &ProxyConfig,
) -> anyhow::Result<Response<Incoming>> {
    let stream = TcpStream::connect((config.upstream.host.as_str(), config.upstream.port))
        .await
        .with_context(|| format!("Failed to connect to upstream node `{}`", config.upstream.base_url))?;
    let (mut sender, connection) = client_http1::handshake(TokioIo::new(stream)).await.with_context(|| {
        format!(
            "Failed to start upstream HTTP connection to `{}`",
            config.upstream.base_url
        )
    })?;
    tokio::spawn(async move {
        if let Err(err) = connection.with_upgrades().await {
            tracing::debug!(error = ?err, "shaker serve upstream connection closed with error");
        }
    });

    sender.send_request(upstream_req).await.with_context(|| {
        format!(
            "Failed to forward request to upstream node `{}`",
            config.upstream.base_url
        )
    })
}

fn rewrite_request(req: Request<Incoming>, config: &ProxyConfig, upgrade: bool) -> anyhow::Result<Request<Incoming>> {
    let (mut parts, body) = req.into_parts();
    let path = parts.uri.path_and_query().map_or("/", http::uri::PathAndQuery::as_str);
    parts.uri = Uri::from_str(path).with_context(|| format!("Invalid request path `{path}`"))?;
    rewrite_request_headers(&mut parts.headers, &config.virtual_host_header, upgrade);
    Ok(Request::from_parts(parts, body))
}

fn rewrite_request_headers(headers: &mut HeaderMap, virtual_host: &HeaderValue, upgrade: bool) {
    strip_hop_by_hop_headers(headers, upgrade);
    headers.insert(HOST, virtual_host.clone());
}

fn rewrite_response_headers(headers: &mut HeaderMap, upgrade: bool) {
    strip_hop_by_hop_headers(headers, upgrade);
}

fn strip_hop_by_hop_headers(headers: &mut HeaderMap, upgrade: bool) {
    remove_connection_header_targets(headers, upgrade);

    for name in [
        HeaderName::from_static("keep-alive"),
        HeaderName::from_static("proxy-connection"),
        PROXY_AUTHENTICATE,
        PROXY_AUTHORIZATION,
        TE,
        TRAILER,
        TRANSFER_ENCODING,
    ] {
        headers.remove(name);
    }

    if !upgrade {
        headers.remove(CONNECTION);
        headers.remove(UPGRADE);
    }
}

fn remove_connection_header_targets(headers: &mut HeaderMap, upgrade: bool) {
    let mut targets = Vec::new();
    for value in headers.get_all(CONNECTION) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for token in value.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if upgrade && token.eq_ignore_ascii_case("upgrade") {
                continue;
            }
            if let Ok(name) = HeaderName::from_bytes(token.as_bytes()) {
                targets.push(name);
            }
        }
    }

    for name in targets {
        headers.remove(name);
    }
}

fn is_upgrade_request(req: &Request<Incoming>) -> bool {
    header_contains_token(req.headers(), CONNECTION, "upgrade") && req.headers().contains_key(UPGRADE)
}

fn header_contains_token(headers: &HeaderMap, name: HeaderName, token: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value
            .to_str()
            .is_ok_and(|value| value.split(',').any(|part| part.trim().eq_ignore_ascii_case(token)))
    })
}

async fn tunnel_upgrades(client_upgrade: OnUpgrade, upstream_upgrade: OnUpgrade) {
    match tokio::try_join!(client_upgrade, upstream_upgrade) {
        Ok((client, upstream)) => tunnel_upgraded_io(client, upstream).await,
        Err(err) => tracing::debug!(error = ?err, "shaker serve upgrade failed"),
    }
}

async fn tunnel_upgraded_io(client: Upgraded, upstream: Upgraded) {
    let mut client = TokioIo::new(client);
    let mut upstream = TokioIo::new(upstream);
    if let Err(err) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        tracing::debug!(error = ?err, "shaker serve upgrade tunnel closed with error");
    }
}

fn text_response(status: StatusCode, body: &'static str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full_body(Bytes::from_static(body.as_bytes())))
        .expect("text response should be valid")
}

fn full_body(body: Bytes) -> ProxyBody {
    Full::new(body).map_err(|never| match never {}).boxed()
}

#[cfg(test)]
fn empty_body() -> ProxyBody {
    http_body_util::Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;
    use lyquor_test::test;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    #[test]
    fn endpoint_to_http_base_accepts_plaintext_only() {
        let cases = [
            ("ws://127.0.0.1:10087/ws", "http://127.0.0.1:10087/"),
            ("http://localhost:10087/api?x=1", "http://localhost:10087/"),
        ];
        for (input, expected) in cases {
            assert_eq!(plaintext_http_base_endpoint(input).unwrap().as_str(), expected);
        }

        for input in ["wss://example.test/ws", "https://example.test/api"] {
            let err = plaintext_http_base_endpoint(input).unwrap_err().to_string();
            assert!(
                err.contains("only supports plaintext localnet endpoints"),
                "unexpected error for {input}: {err}"
            );
        }
    }

    #[test]
    fn virtual_host_uses_raw_dns_labels() {
        let lyquid_id = LyquidID::from(42_u64);
        let node_id = NodeID::from(7_u64);
        let host = virtual_host(lyquid_id, node_id);

        assert_eq!(host, format!("{}.{}", lyquid_id.as_dns_label(), node_id.as_dns_label()));
        assert!(!host.contains("Lyquid-"));
        assert!(!host.contains("Node-"));
    }

    #[test(tokio::test)]
    async fn local_info_proxies_upstream_and_rewrites_node_base_url() -> anyhow::Result<()> {
        let upstream = TcpListener::bind("127.0.0.1:0").await?;
        let upstream_addr = upstream.local_addr()?;
        let (record_tx, record_rx) = oneshot::channel();
        let record_tx = Arc::new(Mutex::new(Some(record_tx)));

        let upstream_task = tokio::spawn(async move {
            let (socket, _) = upstream.accept().await?;
            let service = service_fn(move |req: Request<Incoming>| {
                let record_tx = record_tx.clone();
                async move {
                    let (parts, _body) = req.into_parts();
                    if let Some(record_tx) = record_tx
                        .lock()
                        .expect("record sender lock should not be poisoned")
                        .take()
                    {
                        let _ = record_tx.send((parts.method, parts.uri.to_string(), parts.headers.get(HOST).cloned()));
                    }

                    let body = serde_json::to_vec(&serde_json::json!({
                        "lyquid_id": "Lyquid-upstream",
                        "node_base_url": "http://upstream-node.example.test",
                        "backend_contract": "0x0000000000000000000000000000000000000042",
                        "sequence_backend": "0x1111111111111111111111111111111111111111111111111111111111111111"
                    }))
                    .expect("test JSON should serialize");
                    Ok::<_, hyper::Error>(
                        Response::builder()
                            .status(StatusCode::OK)
                            .header(CONTENT_TYPE, "application/json")
                            .header(CONTENT_LENGTH, body.len().to_string())
                            .header(CONNECTION, "x-remove")
                            .header("x-remove", "secret")
                            .body(full_body(body.into()))
                            .expect("test response should be valid"),
                    )
                }
            });
            server_http1::Builder::new()
                .serve_connection(TokioIo::new(socket), service)
                .await?;
            anyhow::Ok(())
        });

        let config = ProxyConfig::new(
            UpstreamEndpoint::from_endpoint(&format!("http://{upstream_addr}/"))?,
            "lyquid-label.node-label".to_owned(),
        )?;
        let server = ServeServer::bind_with_config(config, "127.0.0.1:0".parse()?).await?;
        let local_addr = server.local_addr();
        let shutdown = CancellationToken::new();
        let proxy_task = tokio::spawn(server.run(shutdown.clone()));

        let response = reqwest::get(format!("http://{local_addr}/lyquid/info")).await?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        assert!(response.headers().get(CONNECTION).is_none());
        assert!(response.headers().get("x-remove").is_none());
        let body: serde_json::Value = response.json().await?;
        let expected_node_base_url = format!("http://{upstream_addr}");
        assert_eq!(
            body.get("lyquid_id").and_then(|value| value.as_str()),
            Some("Lyquid-upstream")
        );
        assert_eq!(
            body.get("node_base_url").and_then(|value| value.as_str()),
            Some(expected_node_base_url.as_str())
        );
        assert_eq!(
            body.get("backend_contract").and_then(|value| value.as_str()),
            Some("0x0000000000000000000000000000000000000042")
        );
        assert_eq!(
            body.get("sequence_backend").and_then(|value| value.as_str()),
            Some("0x1111111111111111111111111111111111111111111111111111111111111111")
        );

        let (method, uri, host) = record_rx.await?;
        assert_eq!(method, Method::GET);
        assert_eq!(uri, "/lyquid/info");
        assert_eq!(
            host.as_ref().and_then(|value| value.to_str().ok()),
            Some("lyquid-label.node-label")
        );

        shutdown.cancel();
        proxy_task.await??;
        upstream_task.await??;
        Ok(())
    }

    #[test(tokio::test)]
    async fn http_proxy_rewrites_host_and_preserves_request() -> anyhow::Result<()> {
        let upstream = TcpListener::bind("127.0.0.1:0").await?;
        let upstream_addr = upstream.local_addr()?;
        let (record_tx, record_rx) = oneshot::channel();
        let record_tx = Arc::new(Mutex::new(Some(record_tx)));

        let upstream_task = tokio::spawn(async move {
            let (socket, _) = upstream.accept().await?;
            let service = service_fn(move |req: Request<Incoming>| {
                let record_tx = record_tx.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let body = body.collect().await?.to_bytes();
                    if let Some(record_tx) = record_tx
                        .lock()
                        .expect("record sender lock should not be poisoned")
                        .take()
                    {
                        let _ = record_tx.send((
                            parts.method,
                            parts.uri.to_string(),
                            parts.headers.get(HOST).cloned(),
                            body,
                        ));
                    }
                    Ok::<_, hyper::Error>(
                        Response::builder()
                            .status(StatusCode::CREATED)
                            .header("x-upstream", "ok")
                            .header(CONNECTION, "x-remove")
                            .header("x-remove", "secret")
                            .body(full_body(Bytes::from_static(b"proxied")))
                            .expect("test response should be valid"),
                    )
                }
            });
            server_http1::Builder::new()
                .serve_connection(TokioIo::new(socket), service)
                .await?;
            anyhow::Ok(())
        });

        let config = ProxyConfig::new(
            UpstreamEndpoint::from_endpoint(&format!("http://{upstream_addr}/"))?,
            "lyquid-label.node-label".to_owned(),
        )?;
        let server = ServeServer::bind_with_config(config, "127.0.0.1:0".parse()?).await?;
        let local_addr = server.local_addr();
        let shutdown = CancellationToken::new();
        let proxy_task = tokio::spawn(server.run(shutdown.clone()));

        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://{local_addr}/api?cache=false"))
            .body("request-body")
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::CREATED);
        assert_eq!(
            response
                .headers()
                .get("x-upstream")
                .and_then(|value| value.to_str().ok()),
            Some("ok")
        );
        assert!(response.headers().get(CONNECTION).is_none());
        assert!(response.headers().get("x-remove").is_none());
        assert_eq!(response.text().await?, "proxied");

        let (method, uri, host, body) = record_rx.await?;
        assert_eq!(method, Method::POST);
        assert_eq!(uri, "/api?cache=false");
        assert_eq!(
            host.as_ref().and_then(|value| value.to_str().ok()),
            Some("lyquid-label.node-label")
        );
        assert_eq!(body, Bytes::from_static(b"request-body"));

        shutdown.cancel();
        proxy_task.await??;
        upstream_task.abort();
        Ok(())
    }

    #[test(tokio::test)]
    async fn websocket_upgrade_tunnels_bytes() -> anyhow::Result<()> {
        let upstream = TcpListener::bind("127.0.0.1:0").await?;
        let upstream_addr = upstream.local_addr()?;
        let (host_tx, host_rx) = oneshot::channel();
        let (bytes_tx, bytes_rx) = oneshot::channel();
        let host_tx = Arc::new(Mutex::new(Some(host_tx)));
        let bytes_tx = Arc::new(Mutex::new(Some(bytes_tx)));

        let upstream_task = tokio::spawn(async move {
            let (socket, _) = upstream.accept().await?;
            let service = service_fn(move |mut req: Request<Incoming>| {
                let host_tx = host_tx.clone();
                let bytes_tx = bytes_tx.clone();
                async move {
                    if let Some(host_tx) = host_tx.lock().expect("host sender lock should not be poisoned").take() {
                        let _ = host_tx.send(req.headers().get(HOST).cloned());
                    }
                    let upgrade = hyper::upgrade::on(&mut req);
                    tokio::spawn(async move {
                        let upgraded = upgrade.await.expect("upstream upgrade should complete");
                        let mut upgraded = TokioIo::new(upgraded);
                        let mut buf = [0_u8; 4];
                        upgraded
                            .read_exact(&mut buf)
                            .await
                            .expect("upstream should read tunneled bytes");
                        if let Some(bytes_tx) = bytes_tx
                            .lock()
                            .expect("bytes sender lock should not be poisoned")
                            .take()
                        {
                            let _ = bytes_tx.send(buf);
                        }
                        upgraded
                            .write_all(b"pong")
                            .await
                            .expect("upstream should write tunneled bytes");
                    });
                    Ok::<_, hyper::Error>(
                        Response::builder()
                            .status(StatusCode::SWITCHING_PROTOCOLS)
                            .header(CONNECTION, "keep-alive, upgrade")
                            .header(UPGRADE, "websocket")
                            .header("keep-alive", "timeout=5")
                            .header("sec-websocket-accept", "accept-value")
                            .body(empty_body())
                            .expect("upgrade response should be valid"),
                    )
                }
            });
            server_http1::Builder::new()
                .serve_connection(TokioIo::new(socket), service)
                .with_upgrades()
                .await?;
            anyhow::Ok(())
        });

        let config = ProxyConfig::new(
            UpstreamEndpoint::from_endpoint(&format!("http://{upstream_addr}/"))?,
            "lyquid-label.node-label".to_owned(),
        )?;
        let server = ServeServer::bind_with_config(config, "127.0.0.1:0".parse()?).await?;
        let local_addr = server.local_addr();
        let shutdown = CancellationToken::new();
        let proxy_task = tokio::spawn(server.run(shutdown.clone()));

        let mut client = TcpStream::connect(local_addr).await?;
        client
            .write_all(
                b"GET /lyquid/ws HTTP/1.1\r\n\
                  Host: localhost\r\n\
                  Connection: Upgrade\r\n\
                  Upgrade: websocket\r\n\
                  Sec-WebSocket-Key: test-key\r\n\
                  Sec-WebSocket-Version: 13\r\n\
                  \r\n",
            )
            .await?;

        let mut response = Vec::new();
        let mut byte = [0_u8; 1];
        while !response.ends_with(b"\r\n\r\n") {
            client.read_exact(&mut byte).await?;
            response.push(byte[0]);
        }
        let response = String::from_utf8(response)?;
        assert!(
            response.starts_with("HTTP/1.1 101"),
            "unexpected upgrade response: {response}"
        );
        let lower_response = response.to_ascii_lowercase();
        assert!(lower_response.contains("sec-websocket-accept: accept-value"));
        assert!(!lower_response.contains("keep-alive: timeout=5"));

        client.write_all(b"ping").await?;
        let mut tunneled = [0_u8; 4];
        client.read_exact(&mut tunneled).await?;
        assert_eq!(&tunneled, b"pong");

        let host = host_rx.await?;
        assert_eq!(
            host.as_ref().and_then(|value| value.to_str().ok()),
            Some("lyquid-label.node-label")
        );
        assert_eq!(bytes_rx.await?, *b"ping");

        shutdown.cancel();
        proxy_task.await??;
        upstream_task.abort();
        Ok(())
    }
}
