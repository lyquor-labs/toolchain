#![doc = include_str!("../README.md")]

use alloy_json_abi::Param;
use anyhow::Context as _;
use lyquor_eth::{LocalSigner, signer_from_hex};
pub use lyquor_jsonrpc::client::ClientHandle as Client;
use lyquor_oci::registry::{OCIRegistryAuth, OCIRegistryClient, Reference as RegistryReference};
use lyquor_primitives::{Address, alloy_primitives};
use reqwest::Url;
use semver::Version;
use tonic::transport::{Channel, Endpoint};

/// Lyquid build pipeline, Solidity generation, and pack creation.
pub mod build;
mod deploy;
mod publish;
/// Cargo build-script helpers for crates that build nested Lyquids.
pub mod script;
mod serve;
mod toolchain;

pub use build::{BuildOptions, build_lyquid, generate_solidity_sequencer, generate_solidity_sequencer_from_file};
pub use deploy::{DeployOptions, LyquidDeployment, deploy_lazy_lyquid, deploy_lyquid};
pub use publish::{push_lyquid, push_lyquid_to_endpoint};
pub use serve::{DEFAULT_SERVE_ENDPOINT, DEFAULT_SERVE_LISTEN, ServeOptions, ServeServer};

/// Returns the deterministic local signer used by the default development Anvil node.
pub fn devnet_signer() -> anyhow::Result<LocalSigner> {
    signer_from_hex("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80")
}

/// Creates an OCI registry client using CLI credentials when provided, otherwise Docker credentials.
pub fn oci_registry_from_reference(
    reference: &RegistryReference, auth_token: Option<&str>, auth_username: Option<&str>, auth_password: Option<&str>,
) -> OCIRegistryClient {
    let explicit_auth = auth_token
        .map(|token| OCIRegistryAuth::Basic("oauth2accesstoken".to_owned(), token.to_owned()))
        .or_else(|| {
            auth_username.map(|username| {
                OCIRegistryAuth::Basic(username.to_owned(), auth_password.unwrap_or_default().to_owned())
            })
        });

    match explicit_auth {
        Some(auth) => OCIRegistryClient::with_auth(reference.protocol(), auth),
        None => OCIRegistryClient::new(reference.protocol()),
    }
}

/// Parses a CLI hex string into alloy bytes.
pub fn parse_hex_bytes(input: &str) -> Result<alloy_primitives::Bytes, String> {
    input
        .parse::<alloy_primitives::Bytes>()
        .map_err(|e| format!("Invalid hex: {e}"))
}

/// Parses a CLI Ethereum address.
pub fn parse_address(input: &str) -> Result<Address, String> {
    use std::str::FromStr;
    lyquor_primitives::Address::from_str(input).map_err(|e| format!("{e}"))
}

/// Formats ABI parameter types as a Solidity tuple shape, such as `(string,address[])`.
pub fn format_abi_param_tuple(params: &[Param]) -> String {
    let types = params
        .iter()
        .map(|param| param.selector_type().into_owned())
        .collect::<Vec<_>>()
        .join(",");
    format!("({types})")
}

/// Parses a CLI Lyquid ID.
pub fn parse_lyquid_id(input: &str) -> Result<lyquor_primitives::LyquidID, String> {
    use std::str::FromStr;
    lyquor_primitives::LyquidID::from_str(input).map_err(|e| format!("{e:?}"))
}

pub fn node_versions_differ(shaker_version: &str, node_version: &str) -> bool {
    if shaker_version == node_version {
        return false;
    }

    let (Ok(shaker_version), Ok(node_version)) = (Version::parse(shaker_version), Version::parse(node_version)) else {
        return true;
    };

    let has_dev_prerelease = |version: &Version| version.pre.as_str().split('.').any(|identifier| identifier == "dev");
    if has_dev_prerelease(&shaker_version) || has_dev_prerelease(&node_version) {
        return true;
    }

    shaker_version.major != node_version.major ||
        shaker_version.minor != node_version.minor ||
        shaker_version.patch != node_version.patch ||
        shaker_version.pre != node_version.pre
}

pub fn warn_if_node_version_differs(endpoint: &str, node_version: &str) {
    let shaker_version = lyquor_cli::build_version!();

    if node_versions_differ(shaker_version, node_version) {
        tracing::warn!("shaker version {shaker_version} differs from node version {node_version} at {endpoint}");
    }
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
