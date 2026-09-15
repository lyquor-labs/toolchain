//! Shared command-line support for Lyquor binaries.
//!
//! `lyquor-cli` keeps cross-binary concerns out of the node and tooling crates. It owns tracing
//! initialization, environment-driven log filtering, build-version display, and Cargo build-script
//! helpers used by binaries that otherwise have separate command surfaces. Command-specific parsing
//! and behavior remain in the crates that expose those binaries.

use anyhow::Context as _;
use tonic::transport::{Channel, Endpoint};
use url::Url;

use std::io::IsTerminal as _;

use tracing_subscriber::{Layer as _, filter::EnvFilter, fmt::format::FmtSpan, registry::LookupSpan};

/// Cargo build-script helpers shared by Lyquor binaries.
pub mod script;

#[macro_export]
macro_rules! build_version {
    () => {
        env!("LYQUOR_BUILD_VERSION")
    };
}

/// Install the process-wide tracing subscriber from Lyquor logging environment variables.
pub fn setup_tracing() -> anyhow::Result<()> {
    use tracing_subscriber::prelude::*;

    let env_filter = EnvFilter::builder()
        .with_default_directive("info".parse().unwrap())
        .with_env_var("LYQUOR_LOG")
        .from_env_lossy()
        .add_directive("foundry_compilers=warn".parse().unwrap())
        .add_directive("cranelift=info".parse().unwrap())
        .add_directive("wasmtime=info".parse().unwrap());

    let span_events = {
        let mut span_events = FmtSpan::NONE;

        // Default to no span lifecycle events: spans decorate the events that fire inside them,
        // so info-level cause spans stay free at steady state (see developer/debugging.md).
        let s = std::env::var("LYQUOR_LOG_SPAN_EVENTS")
            .unwrap_or_else(|_| "none".into())
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .collect::<Vec<_>>();
        for fmt_span in s {
            match fmt_span.as_str() {
                "new" => span_events |= FmtSpan::NEW,
                "close" => span_events |= FmtSpan::CLOSE,
                "enter" => span_events |= FmtSpan::ENTER,
                "exit" => span_events |= FmtSpan::EXIT,
                "active" => span_events |= FmtSpan::ACTIVE,
                "full" => span_events |= FmtSpan::FULL,
                _ => (),
            }
        }
        span_events
    };

    let registry = tracing_subscriber::registry();

    #[cfg(feature = "tokio-console")]
    let registry = registry.with(console_subscriber::spawn());

    let format = match std::env::var("LYQUOR_LOG_FORMAT")
        .unwrap_or_else(|_| "full".into())
        .to_lowercase()
        .as_str()
    {
        "compact" => LogFormat::Compact,
        "pretty" => LogFormat::Pretty,
        "json" => LogFormat::Json,
        _ => LogFormat::Full,
    };
    let ansi = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty());
    registry
        .with(format_layer(format, ansi, span_events, std::io::stderr, env_filter))
        .init();

    Ok(())
}

#[derive(Clone, Copy)]
enum LogFormat {
    Full,
    Compact,
    Pretty,
    Json,
}

fn format_layer<S, W>(
    format: LogFormat, ansi: bool, span_events: FmtSpan, writer: W, env_filter: EnvFilter,
) -> Box<dyn tracing_subscriber::Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
    W: for<'writer> tracing_subscriber::fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    let layer = tracing_subscriber::fmt::layer()
        .with_thread_ids(true)
        .with_writer(writer)
        .with_span_events(span_events)
        .with_ansi(ansi);

    match format {
        LogFormat::Compact => layer.compact().with_filter(env_filter).boxed(),
        LogFormat::Pretty => layer.pretty().with_filter(env_filter).boxed(),
        LogFormat::Json => layer
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .with_filter(env_filter)
            .boxed(),
        LogFormat::Full => layer.with_filter(env_filter).boxed(),
    }
}

/// Render the startup banner using the supplied build version string.
pub fn format_logo_banner(version: &str) -> String {
    const LOGO: &str = r"
     __    _  _   __   _  _   __  ____    _o/_
    (..)  (.\/.) /  \ / )( \ /  \(  _ \   \##/
    /.(_/\ )../ (  O )) \/ ((  O ))   /    ||
    \..../(../te \__\)\____/ \__/(__\_)um _||_";

    format!(
        "{LOGO}         

    Version: {version:>33}
    =========================================\n",
    )
}

/// Converts a node websocket or HTTP endpoint into the base gRPC HTTP endpoint.
pub fn grpc_api_endpoint(endpoint: &str) -> anyhow::Result<String> {
    let mut url =
        Url::parse(endpoint).map_err(|err| anyhow::anyhow!("Invalid node API endpoint `{endpoint}`: {err}"))?;
    let scheme = match url.scheme() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        other => anyhow::bail!("Unsupported node API endpoint scheme `{other}`"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("Failed to convert API endpoint scheme for `{endpoint}`"))?;
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

/// Builds the tonic endpoint for a node gRPC API endpoint.
pub fn grpc_api_channel_endpoint(endpoint: &str) -> anyhow::Result<(String, Endpoint)> {
    let grpc_endpoint = grpc_api_endpoint(endpoint)?;
    if grpc_endpoint.starts_with("https://") {
        // Tonic's rustls transport needs a process-level crypto provider. If another provider is
        // already installed, keep it.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
    let endpoint =
        Endpoint::new(grpc_endpoint.clone()).with_context(|| format!("Invalid gRPC endpoint `{grpc_endpoint}`"))?;
    Ok((grpc_endpoint, endpoint))
}

/// Connects to a node gRPC API endpoint with tonic's HTTP and HTTPS transport support.
pub async fn connect_grpc_api_channel(endpoint: &str, service_name: &str) -> anyhow::Result<(String, Channel)> {
    let (grpc_endpoint, endpoint) = grpc_api_channel_endpoint(endpoint)?;
    let channel = endpoint
        .connect()
        .await
        .with_context(|| format!("Failed to connect to {service_name} at `{grpc_endpoint}`"))?;
    Ok((grpc_endpoint, channel))
}

#[cfg(test)]
mod tests {
    use tracing_subscriber::prelude::*;

    use super::*;

    #[test]
    fn json_format_accepts_structured_event_in_span() {
        let subscriber = tracing_subscriber::registry().with(format_layer(
            LogFormat::Json,
            true,
            FmtSpan::NONE,
            std::io::sink,
            EnvFilter::new("trace"),
        ));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("request", request_id = 7);
            let _entered = span.enter();
            tracing::info!(answer = 42, "processed request");
        });
    }
}
