//! Shared command-line support for Lyquor binaries.
//!
//! `lyquor-cli` keeps cross-binary concerns out of the node and tooling crates. It owns tracing
//! initialization, environment-driven log filtering, build-version display, and Cargo build-script
//! helpers used by binaries that otherwise have separate command surfaces. Command-specific parsing
//! and behavior remain in the crates that expose those binaries.

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

    let env_filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive("info".parse().unwrap())
        .with_env_var("LYQUOR_LOG")
        .from_env_lossy()
        .add_directive("foundry_compilers=warn".parse().unwrap())
        .add_directive("cranelift=info".parse().unwrap())
        .add_directive("wasmtime=info".parse().unwrap());

    let span_events = {
        use tracing_subscriber::fmt::format::FmtSpan;

        let mut span_events = FmtSpan::NONE;

        let s = std::env::var("LYQUOR_LOG_SPAN_EVENTS")
            .unwrap_or_else(|_| "new,close".into())
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

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_thread_ids(true)
        .with_writer(std::io::stderr)
        .with_span_events(span_events);

    let registry = tracing_subscriber::registry();

    #[cfg(feature = "tokio-console")]
    let registry = registry.with(console_subscriber::spawn());

    match std::env::var("LYQUOR_LOG_FORMAT")
        .unwrap_or_else(|_| "full".into())
        .to_lowercase()
        .as_str()
    {
        "compact" => registry.with(fmt_layer.compact().with_filter(env_filter)).init(),
        "pretty" => registry.with(fmt_layer.pretty().with_filter(env_filter)).init(),
        _ => registry.with(fmt_layer.with_filter(env_filter)).init(),
    };

    Ok(())
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
