//! Shared command-line support for Lyquor binaries.
//!
//! `lyquor-cli` keeps cross-binary concerns out of the node and tooling crates. It owns tracing
//! initialization, environment-driven log filtering, build-version display, and Cargo build-script
//! helpers used by binaries that otherwise have separate command surfaces. Command-specific parsing
//! and behavior remain in the crates that expose those binaries.

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
