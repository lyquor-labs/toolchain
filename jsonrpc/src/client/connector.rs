use std::{io, sync::Arc, time::Duration};

use jsonrpsee::core::client::{TransportReceiverT, TransportSenderT};
use jsonrpsee_client_transport::ws::{Url, WsHandshakeError, WsTransportClientBuilder};
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tokio_util::either::Either;

type ConnectedStream = Either<TcpStream, TlsStream<TcpStream>>;

/// Connect to a WebSocket endpoint using a custom TCP/TLS path (rustls with the OS platform
/// certificate verifier) instead of jsonrpsee's default transport.
pub(super) async fn connect(
    url: &Url, connection_timeout: Duration,
) -> Result<(impl TransportSenderT + Send, impl TransportReceiverT + Send), WsHandshakeError> {
    tokio::time::timeout(connection_timeout, connect_inner(url, connection_timeout))
        .await
        .map_err(|_| WsHandshakeError::Timeout(connection_timeout))?
}

async fn connect_inner(
    url: &Url, connection_timeout: Duration,
) -> Result<(impl TransportSenderT + Send, impl TransportReceiverT + Send), WsHandshakeError> {
    let stream = connect_stream(url).await?;
    let mut handshake_url = url.clone();
    if handshake_url.scheme() == "wss" {
        handshake_url.set_scheme("ws").expect("ws is a valid URL scheme");
    }

    // Keep jsonrpsee's WebSocket handshake/framing, but bypass its default DNS/TCP/TLS path:
    // it resolves via Url::socket_addrs and its TLS feature installs the ring provider.
    WsTransportClientBuilder::default()
        .connection_timeout(connection_timeout)
        .build_with_stream(handshake_url, stream)
        .await
}

async fn connect_stream(url: &Url) -> Result<ConnectedStream, WsHandshakeError> {
    let tls = match url.scheme() {
        "ws" => false,
        "wss" => true,
        scheme => {
            return Err(WsHandshakeError::Url(
                format!("`{scheme}` not supported, expects 'ws' or 'wss'").into(),
            ));
        }
    };
    let host = url
        .host_str()
        .ok_or_else(|| WsHandshakeError::Url("Invalid host".into()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| WsHandshakeError::Url("Invalid port".into()))?;

    let stream = TcpStream::connect((host, port)).await.map_err(WsHandshakeError::Io)?;
    if let Err(err) = stream.set_nodelay(true) {
        tracing::warn!("set nodelay failed: {:?}", err);
    }

    if !tls {
        return Ok(Either::Left(stream));
    }

    let server_name = rustls_pki_types::ServerName::try_from(host.to_owned())
        .map_err(|err| WsHandshakeError::Url(format!("Invalid host: {host} {err:?}").into()))?;
    tls_connector()?
        .connect(server_name, stream)
        .await
        .map(Either::Right)
        .map_err(other_error)
}

fn tls_connector() -> Result<TlsConnector, WsHandshakeError> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(other_error)?
        .with_platform_verifier()
        .map_err(other_error)?
        .with_no_client_auth();

    Ok(Arc::new(config).into())
}

fn other_error(error: impl std::error::Error + Send + Sync + 'static) -> WsHandshakeError {
    WsHandshakeError::Io(io::Error::other(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lyquor_test::test;

    #[test(tokio::test)]
    async fn unsupported_scheme_is_rejected() {
        let url = Url::parse("http://example.com/ws").unwrap();

        let err = connect_stream(&url).await.unwrap_err();

        assert!(err.to_string().contains("not supported"));
    }

    #[test(tokio::test)]
    async fn wss_scheme_is_accepted_by_connector_path() {
        let url = Url::parse("wss://127.0.0.1:0/ws").unwrap();

        let err = connect_stream(&url).await.unwrap_err();

        assert!(matches!(err, WsHandshakeError::Io(_)));
    }
}
