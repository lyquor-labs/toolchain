use crate::pack::{Error as PackError, LazyError, LazyLyquidPack, LyquidPack, LyquidPackDigest, PackBlobReader};
use async_trait::async_trait;
use bytes::Bytes;
use docker_credential::{self, DockerCredential};
pub use oci_client::Reference as OCIReference;
pub use oci_client::client::ClientProtocol;
pub use oci_client::secrets::RegistryAuth as OCIRegistryAuth;
use oci_client::{Client, client::ClientConfig, manifest};
use std::{fmt, str::FromStr, sync::Arc};
use thiserror::Error;

/// Errors returned by OCI registry reference, push, and pull operations.
#[derive(Debug, Error)]
pub enum Error {
    #[error("Invalid registry address.")]
    InvalidRegistryAddress,
    #[error("Failed to interact with OCI distribution.\n└─Detail: {0}")]
    OciDistributionError(String),
    #[error("Failed to pull LyquidPack from registry.\n└─Detail: {0}")]
    PullError(String),
    #[error("Failed to push LyquidPack to registry.\n└─Detail: {0}")]
    PushError(String),
    #[error("Bad metadata.")]
    BadMetadata,
    #[error("Bad image digest.")]
    BadDigest,
    #[error("Bad image.\n└─Detail: {0}")]
    BadImage(String),
}

#[derive(Clone)]
struct RegistryBlobReader {
    registry: OCIRegistryClient,
    reference: OCIReference,
}

#[async_trait]
impl PackBlobReader for RegistryBlobReader {
    async fn read_blob(&self, digest: &str) -> Result<Bytes, LazyError> {
        let mut out = Vec::new();
        self.registry
            .client
            .pull_blob(&self.reference, digest, &mut out)
            .await
            .map_err(|e| LazyError::BlobRead {
                digest: digest.to_owned(),
                detail: e.to_string(),
            })?;
        Ok(Bytes::from(out))
    }
}

/// OCI registry client configured for one transport protocol.
#[derive(Clone)]
pub struct OCIRegistryClient {
    client: Client,
    explicit_auth: Option<OCIRegistryAuth>,
}

/// Parsed OCI reference plus explicit transport protocol.
#[derive(Clone, Debug)]
pub struct Reference {
    reference: OCIReference,
    protocol: ClientProtocol,
}

/// Digest-pinned Lyquid image reference.
#[derive(Clone, Debug)]
pub struct PinnedImage {
    reference: Reference,
    pub digest: LyquidPackDigest,
}

impl Reference {
    /// Create a reference from parsed OCI parts and protocol.
    pub fn new(reference: OCIReference, protocol: ClientProtocol) -> Self {
        Self { reference, protocol }
    }

    /// Return the underlying OCI reference.
    pub fn reference(&self) -> &OCIReference {
        &self.reference
    }

    /// Return the transport protocol for this reference.
    pub fn protocol(&self) -> ClientProtocol {
        self.protocol.clone()
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.protocol {
            ClientProtocol::Http => write!(f, "http://{}", self.reference),
            _ => write!(f, "{}", self.reference),
        }
    }
}

impl FromStr for Reference {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (protocol, value) = split_registry_transport(value)?;
        let reference = OCIReference::try_from(value).map_err(|_| Error::InvalidRegistryAddress)?;
        Ok(Self { reference, protocol })
    }
}

impl PinnedImage {
    /// Create a digest-pinned image from a repository hint and pack digest.
    pub fn new(repo_hint: impl Into<String>, digest: LyquidPackDigest) -> Result<Self, Error> {
        let repo_hint = repo_hint.into();
        let parsed = Reference::from_str(repo_hint.as_str())?;
        let reference = Reference::new(Self::canonical_repo_hint(parsed.reference()), parsed.protocol());
        Ok(Self { reference, digest })
    }

    fn from_reference(reference: &OCIReference, digest: LyquidPackDigest, protocol: ClientProtocol) -> Self {
        Self {
            reference: Reference::new(Self::canonical_repo_hint(reference), protocol),
            digest,
        }
    }

    /// Return the repository hint without the digest.
    pub fn repo_hint(&self) -> String {
        self.reference.to_string()
    }

    /// Return the transport protocol for this pinned image.
    pub fn protocol(&self) -> ClientProtocol {
        self.reference.protocol()
    }

    /// Return an OCI reference pinned to this image digest.
    pub fn pinned_reference(&self) -> OCIReference {
        self.reference
            .reference()
            .clone_with_digest(self.digest.to_oci_digest())
    }

    fn canonical_repo_hint(reference: &OCIReference) -> OCIReference {
        OCIReference::try_from(format!("{}/{}", reference.registry(), reference.repository()))
            .expect("existing OCI reference parts must produce a valid repository hint")
    }
}

fn split_registry_transport(value: &str) -> Result<(ClientProtocol, &str), Error> {
    if let Some(value) = value.strip_prefix("http://") {
        Ok((ClientProtocol::Http, value))
    } else if let Some(value) = value.strip_prefix("https://") {
        Ok((ClientProtocol::Https, value))
    } else if value.contains("://") {
        Err(Error::InvalidRegistryAddress)
    } else {
        Ok((ClientProtocol::Https, value))
    }
}

impl OCIRegistryClient {
    /// Create a registry client using `protocol` for distribution requests.
    ///
    /// Registry operations authenticate with Docker credentials when available, falling back to
    /// anonymous access if Docker has no credentials for the registry.
    pub fn new(protocol: ClientProtocol) -> Self {
        Self::from_explicit_auth(protocol, None)
    }

    /// Create a registry client using explicit credentials for every registry operation.
    pub fn with_auth(protocol: ClientProtocol, auth: OCIRegistryAuth) -> Self {
        Self::from_explicit_auth(protocol, Some(auth))
    }

    fn from_explicit_auth(protocol: ClientProtocol, explicit_auth: Option<OCIRegistryAuth>) -> Self {
        Self {
            client: Client::new(ClientConfig {
                protocol,
                platform_resolver: None,
                use_monolithic_push: true,
                ..Default::default()
            }),
            explicit_auth,
        }
    }

    #[inline]
    fn auth_for_reference(&self, reference: &OCIReference) -> OCIRegistryAuth {
        self.explicit_auth
            .clone()
            .unwrap_or_else(|| Self::docker_auth_or_anonymous(reference))
    }

    fn docker_auth_or_anonymous(reference: &OCIReference) -> OCIRegistryAuth {
        Self::docker_auth_or_anonymous_with(reference, docker_credential::get_credential)
    }

    fn docker_auth_or_anonymous_with(
        reference: &OCIReference,
        get_credential: impl FnOnce(&str) -> Result<DockerCredential, docker_credential::CredentialRetrievalError>,
    ) -> OCIRegistryAuth {
        match get_credential(reference.resolve_registry()) {
            Ok(DockerCredential::UsernamePassword(user, passwd)) => OCIRegistryAuth::Basic(user, passwd),
            Ok(DockerCredential::IdentityToken(token)) => OCIRegistryAuth::Bearer(token),
            Err(_) => OCIRegistryAuth::Anonymous,
        }
    }

    /// Resolve a tag or digest reference into a digest-pinned image.
    pub async fn pin_reference(&self, reference: &Reference) -> Result<PinnedImage, Error> {
        let oci_reference = reference.reference();
        let digest = match oci_reference.digest() {
            Some(digest) => LyquidPackDigest::from_oci_digest(digest).map_err(|_| Error::BadDigest)?,
            None => {
                let auth = self.auth_for_reference(oci_reference);
                let digest = self
                    .client
                    .fetch_manifest_digest(oci_reference, &auth)
                    .await
                    .map_err(|e| Error::OciDistributionError(e.to_string()))?;
                LyquidPackDigest::from_oci_digest(&digest).map_err(|_| Error::BadDigest)?
            }
        };
        Ok(PinnedImage::from_reference(oci_reference, digest, reference.protocol()))
    }

    /// Pull a reference as a digest-pinned lazy pack.
    pub async fn pull_lazy_reference(&self, reference: &Reference) -> Result<(PinnedImage, LazyLyquidPack), Error> {
        let pinned = self.pin_reference(reference).await?;
        let lazy = self.pull_lazy_pinned(&pinned).await?;
        Ok((pinned, lazy))
    }

    /// Push a full pack to a mutable OCI reference and return the registry manifest digest.
    pub async fn push_reference(&self, pack: LyquidPack, reference: &OCIReference) -> Result<LyquidPackDigest, Error> {
        if reference.digest().is_some() {
            return Err(Error::PushError("Cannot push to a digest-pinned reference.".to_owned()));
        }
        let wasm = pack.wasm();

        if wasm.is_empty() {
            return Err(Error::BadImage("Missing wasm binary.".into()));
        }

        let manifest = pack.manifest();
        let (layers, config) = pack.to_oci_push_parts().map_err(|e| match e {
            PackError::SerializationError => Error::BadMetadata,
            _ => Error::PushError(format!("Failed to prepare OCI payload: {e}")),
        })?;

        let auth = self.auth_for_reference(reference);

        let _ = self
            .client
            .push(reference, &layers, config, &auth, Some(manifest.clone()))
            .await
            .map_err(|e| Error::PushError(e.to_string()))?;

        let digest = self
            .client
            .fetch_manifest_digest(reference, &auth)
            .await
            .map_err(|e| Error::OciDistributionError(e.to_string()))?;
        LyquidPackDigest::from_oci_digest(&digest).map_err(|_| Error::BadDigest)
    }

    #[inline]
    fn validate_manifest_digest(
        manifest: &manifest::OciImageManifest, requested_digest: &LyquidPackDigest,
    ) -> Result<(), Error> {
        // `oci-client` already verified the pulled raw bytes against the requested digest.
        // This adds the stricter invariant we want here: our typed manifest must
        // canonical-reserialize back to that same requested digest.
        let actual_digest = LyquidPackDigest::from_oci_digest(&crate::pack::sha256_digest(
            &LyquidPack::serialize_manifest_raw(manifest),
        ))
        .map_err(|_| Error::BadDigest)?;
        if requested_digest.to_oci_digest() != actual_digest.to_oci_digest() {
            return Err(Error::PullError(format!(
                "Pulled manifest digest mismatch. expected={}, got={}",
                requested_digest.to_oci_digest(),
                actual_digest.to_oci_digest()
            )));
        }
        Ok(())
    }

    /// Pull a digest-pinned image as a lazy pack without loading all blobs.
    pub async fn pull_lazy_pinned(&self, image: &PinnedImage) -> Result<LazyLyquidPack, Error> {
        let reference = &image.pinned_reference();
        let requested_digest = &image.digest;
        let auth = self.auth_for_reference(reference);
        let (image_manifest, _, metadata_blob) = self
            .client
            .pull_manifest_and_config(reference, &auth)
            .await
            .map_err(|e| Error::PullError(e.to_string()))?;
        Self::validate_manifest_digest(&image_manifest, requested_digest)?;
        // `pull_manifest_and_config` stores `auth` in oci-client's shared auth store before pulling
        // the config blob. `RegistryBlobReader` clones this client, so later lazy layer pulls reuse
        // the same stored credentials instead of doing unauthenticated blob requests.
        let reader = Arc::new(RegistryBlobReader {
            registry: self.clone(),
            reference: reference.clone(),
        });

        LazyLyquidPack::from_manifest_and_config(
            image_manifest,
            metadata_blob.as_bytes(),
            requested_digest.clone(),
            reader,
        )
        .map_err(|e| Error::BadImage(e.to_string()))
    }

    /// Pull a digest-pinned image and materialize all blobs into a full pack.
    pub async fn pull_full_pinned(&self, image: &PinnedImage) -> Result<LyquidPack, Error> {
        let lazy = self.pull_lazy_pinned(image).await?;
        lazy.materialize_full().await.map_err(|err| match err {
            LazyError::BlobRead { .. } => Error::PullError(err.to_string()),
            _ => Error::BadImage(err.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lyquor_test::test;

    #[test]
    fn registry_reference_transport_is_scheme_driven() {
        let http = Reference::from_str("http://registry.example:5000/lyquids:latest").unwrap();
        let https = Reference::from_str("https://registry.example/lyquids:latest").unwrap();
        let bare = Reference::from_str("registry.example/lyquids:latest").unwrap();

        assert_eq!(http.protocol(), ClientProtocol::Http);
        assert_eq!(https.protocol(), ClientProtocol::Https);
        assert_eq!(bare.protocol(), ClientProtocol::Https);
        assert_eq!(http.reference().to_string(), "registry.example:5000/lyquids:latest");
        assert_eq!(https.reference().to_string(), "registry.example/lyquids:latest");
    }

    #[test]
    fn registry_reference_rejects_unknown_schemes() {
        let err =
            Reference::from_str("ftp://registry.example/lyquids:latest").expect_err("unknown schemes must be rejected");
        assert!(matches!(err, Error::InvalidRegistryAddress));
    }

    #[test]
    fn pinned_reference_uses_digest_and_ignores_repo_hint_tag() {
        let digest = LyquidPackDigest::new([7u8; 32].into());
        let pinned =
            PinnedImage::new("registry.example/lyquids/hello:definitely-not-real", digest.clone()).expect("valid hint");

        assert_eq!(
            pinned.pinned_reference().to_string(),
            format!("registry.example/lyquids/hello@{}", digest.to_oci_digest())
        );
    }

    #[test]
    fn pinned_image_preserves_explicit_http_transport() {
        let digest = LyquidPackDigest::new([9u8; 32].into());
        let pinned = PinnedImage::new("http://registry.example:5000/lyquids/hello:latest", digest).unwrap();

        assert_eq!(pinned.protocol(), ClientProtocol::Http);
        assert_eq!(pinned.repo_hint(), "http://registry.example:5000/lyquids/hello:latest");
        assert_eq!(
            pinned.pinned_reference().to_string(),
            "registry.example:5000/lyquids/hello@sha256:0909090909090909090909090909090909090909090909090909090909090909"
        );
    }

    #[test]
    fn explicit_auth_uses_configured_credentials() {
        let reference = OCIReference::from_str("registry.example/lyquids/hello:latest").unwrap();
        let auth = OCIRegistryAuth::Basic("cli-user".into(), "cli-secret".into());
        let client = OCIRegistryClient::with_auth(ClientProtocol::Https, auth.clone());

        assert_eq!(client.auth_for_reference(&reference), auth);
    }

    #[test]
    fn docker_auth_reads_docker_credentials() {
        let reference = OCIReference::from_str("registry.example/lyquids/hello:latest").unwrap();

        let auth = OCIRegistryClient::docker_auth_or_anonymous_with(&reference, |registry| {
            assert_eq!(registry, "registry.example");
            Ok(DockerCredential::UsernamePassword(
                "docker-user".into(),
                "docker-secret".into(),
            ))
        });

        assert_eq!(
            auth,
            OCIRegistryAuth::Basic("docker-user".into(), "docker-secret".into())
        );
    }

    #[test]
    fn docker_auth_falls_back_to_anonymous() {
        let reference = OCIReference::from_str("registry.example/lyquids/hello:latest").unwrap();

        let auth = OCIRegistryClient::docker_auth_or_anonymous_with(&reference, |_| {
            Err(docker_credential::CredentialRetrievalError::NoCredentialConfigured)
        });

        assert_eq!(auth, OCIRegistryAuth::Anonymous);
    }
}
