use anyhow::Context;
use lyquor_oci::pack::LyquidPack;
use lyquor_oci::registry::{ClientProtocol, OCIReference, OCIRegistryClient, Reference as RegistryReference};
use lyquor_primitives::B256;
use reqwest::Url;

/// Pushes a Lyquid pack to an OCI registry reference and returns its digest.
pub async fn push_lyquid(
    pack: LyquidPack, registry: &OCIRegistryClient, reference: &OCIReference,
) -> anyhow::Result<B256> {
    let digest = registry
        .push_reference(pack, reference)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(*digest.digest())
}

/// Pushes a Lyquid pack to the local registry derived from a node API endpoint.
pub async fn push_lyquid_to_endpoint(pack: LyquidPack, endpoint: &str) -> anyhow::Result<B256> {
    let reference = local_oci_target_from_api_endpoint(endpoint)?;
    let oci = crate::oci_registry_from_reference(&reference, None, None, None);
    push_lyquid(pack, &oci, reference.reference()).await
}

fn local_oci_target_from_api_endpoint(endpoint: &str) -> anyhow::Result<RegistryReference> {
    let url = Url::parse(endpoint).with_context(|| format!("Invalid node API endpoint `{endpoint}`"))?;
    let registry = registry_host_from_api_endpoint(&url)?;
    let reference = OCIReference::with_tag(registry, "lyquids/local".into(), "latest".into());
    let transport = match url.scheme() {
        "http" | "ws" => ClientProtocol::Http,
        "https" | "wss" => ClientProtocol::Https,
        scheme => anyhow::bail!("Unsupported node API endpoint scheme `{scheme}`"),
    };
    Ok(RegistryReference::new(reference, transport))
}

fn registry_host_from_api_endpoint(url: &Url) -> anyhow::Result<String> {
    let host = url.host_str().context("Node API endpoint is missing a host")?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let port = url
        .port_or_known_default()
        .context("Node API endpoint is missing a port and has no known default")?;
    Ok(format!("{host}:{port}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lyquor_test::test;

    #[test]
    fn local_reference_derives_target_from_supported_endpoint_forms() {
        let cases = [
            (
                "ws://127.0.0.1:10087/ws",
                "127.0.0.1:10087/lyquids/local:latest",
                ClientProtocol::Http,
            ),
            (
                "http://localhost:10087/api",
                "localhost:10087/lyquids/local:latest",
                ClientProtocol::Http,
            ),
            ("ws://lyquor/ws", "lyquor:80/lyquids/local:latest", ClientProtocol::Http),
            (
                "wss://lyquor/ws",
                "lyquor:443/lyquids/local:latest",
                ClientProtocol::Https,
            ),
        ];

        for (endpoint, expected_reference, expected_transport) in cases {
            let reference = local_oci_target_from_api_endpoint(endpoint).unwrap();
            assert_eq!(reference.reference().to_string(), expected_reference);
            assert_eq!(reference.protocol(), expected_transport);
        }
    }
}
