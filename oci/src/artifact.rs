use bytes::Bytes;
use oci_client::manifest::OciImageManifest;
use thiserror::Error;

use crate::pack::{LyquidPackDigest, sha256_digest};

/// Errors returned while validating or materializing an OCI artifact.
#[derive(Debug, Error)]
pub enum Error {
    #[error("Invalid OCI manifest.\nDetail: {0}")]
    InvalidManifest(String),
    #[error("OCI artifact layer count mismatch.\nExpected: {expected}\nActual: {actual}")]
    LayerCountMismatch { expected: usize, actual: usize },
    #[error("OCI blob digest mismatch.\nExpected: {expected}\nActual: {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("OCI blob size mismatch.\nExpected: {expected}\nActual: {actual}")]
    SizeMismatch { expected: i64, actual: i64 },
    #[error("Invalid Lyquid pack.\nDetail: {0}")]
    InvalidLyquidPack(String),
}

/// Exact bytes of a complete, content-addressed OCI image.
///
/// Direct artifact storage and transfer preserve these bytes. Converting through
/// [`crate::pack::LyquidPack`] preserves semantic content, but may produce a different canonical
/// byte representation and therefore a different digest when converted back.
#[derive(Clone, Debug)]
pub struct OciArtifact {
    digest: LyquidPackDigest,
    manifest: OciImageManifest,
    manifest_bytes: Bytes,
    config_bytes: Bytes,
    layer_bytes: Vec<Bytes>,
}

impl OciArtifact {
    /// Validate exact OCI bytes against the requested manifest digest and its descriptors.
    pub fn from_parts(
        digest: LyquidPackDigest, manifest_bytes: Bytes, config_bytes: Bytes, layer_bytes: Vec<Bytes>,
    ) -> Result<Self, Error> {
        verify_digest(&digest.to_oci_digest(), &manifest_bytes)?;
        let manifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| Error::InvalidManifest(format!("Cannot decode image manifest: {error}")))?;
        Self::from_parsed_parts(digest, manifest, manifest_bytes, config_bytes, layer_bytes)
    }

    pub(crate) fn from_parsed_parts(
        digest: LyquidPackDigest, manifest: OciImageManifest, manifest_bytes: Bytes, config_bytes: Bytes,
        layer_bytes: Vec<Bytes>,
    ) -> Result<Self, Error> {
        if manifest.layers.len() != layer_bytes.len() {
            return Err(Error::LayerCountMismatch {
                expected: manifest.layers.len(),
                actual: layer_bytes.len(),
            });
        }

        verify_descriptor(&manifest.config.digest, manifest.config.size, &config_bytes)?;
        for (descriptor, bytes) in manifest.layers.iter().zip(&layer_bytes) {
            verify_descriptor(&descriptor.digest, descriptor.size, bytes)?;
        }

        Ok(Self {
            digest,
            manifest,
            manifest_bytes,
            config_bytes,
            layer_bytes,
        })
    }

    /// Return the digest of the exact manifest bytes.
    pub fn digest(&self) -> &LyquidPackDigest {
        &self.digest
    }

    /// Return the parsed OCI image manifest.
    pub fn manifest(&self) -> &OciImageManifest {
        &self.manifest
    }

    /// Return the exact OCI manifest bytes.
    pub fn manifest_bytes(&self) -> &Bytes {
        &self.manifest_bytes
    }

    /// Return the exact OCI config bytes.
    pub fn config_bytes(&self) -> &Bytes {
        &self.config_bytes
    }

    /// Return the exact OCI layer bytes in manifest order.
    pub fn layer_bytes(&self) -> &[Bytes] {
        &self.layer_bytes
    }

    pub(crate) fn into_parts(self) -> (LyquidPackDigest, OciImageManifest, Bytes, Vec<Bytes>) {
        (self.digest, self.manifest, self.config_bytes, self.layer_bytes)
    }
}

fn verify_descriptor(digest: &str, size: i64, bytes: &[u8]) -> Result<(), Error> {
    verify_digest(digest, bytes)?;
    let actual = i64::try_from(bytes.len()).map_err(|_| Error::SizeMismatch {
        expected: size,
        actual: i64::MAX,
    })?;
    if size != actual {
        return Err(Error::SizeMismatch { expected: size, actual });
    }
    Ok(())
}

fn verify_digest(expected: &str, bytes: &[u8]) -> Result<(), Error> {
    let actual = sha256_digest(bytes);
    if expected != actual {
        return Err(Error::DigestMismatch {
            expected: expected.to_owned(),
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::pack::{LyquidPack, LyquidPackMetadata};

    use super::*;

    fn sample_pack() -> LyquidPack {
        LyquidPack::build_with_binary(
            Bytes::from_static(&[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]),
            Bytes::from_static(b"deployment-bytecode"),
            Some(BTreeMap::from([(
                "helper".to_owned(),
                Bytes::from_static(b"auxiliary-bytecode"),
            )])),
            Some(BTreeMap::from([(
                "index.html".to_owned(),
                Bytes::from_static(b"hello"),
            )])),
            None,
            LyquidPackMetadata::new("artifact-test", Some("Lyquor Labs"), Some("round trip"), None, None),
        )
    }

    fn assert_same_semantics(expected: &LyquidPack, actual: &LyquidPack) {
        assert_eq!(actual.wasm(), expected.wasm());
        assert_eq!(actual.evm_deployment_bytecode(), expected.evm_deployment_bytecode());
        assert_eq!(actual.evm_auxiliary_bytecodes(), expected.evm_auxiliary_bytecodes());
        assert_eq!(actual.assets(), expected.assets());
        assert_eq!(actual.eth_abi(), expected.eth_abi());
        assert_eq!(actual.metadata().name, expected.metadata().name);
        assert_eq!(actual.metadata().author, expected.metadata().author);
        assert_eq!(actual.metadata().description, expected.metadata().description);
        assert_eq!(actual.metadata().os, expected.metadata().os);
        assert_eq!(actual.metadata().os_version, expected.metadata().os_version);
    }

    #[test]
    fn pack_and_artifact_convert_both_directions() {
        let pack = sample_pack();
        let artifact = OciArtifact::try_from(pack.clone()).expect("pack must convert to an artifact");
        let materialized = LyquidPack::try_from(artifact).expect("artifact must convert to a pack");

        assert_same_semantics(&pack, &materialized);
    }

    #[test]
    fn artifact_pack_artifact_round_trip_may_change_bytes_and_digest() {
        let canonical = OciArtifact::try_from(sample_pack()).expect("pack must convert to an artifact");
        let config: serde_json::Value = serde_json::from_slice(canonical.config_bytes()).expect("config must decode");
        let config_bytes = Bytes::from(serde_json::to_vec_pretty(&config).expect("config must encode"));
        let mut manifest = canonical.manifest().clone();
        manifest.config.digest = sha256_digest(&config_bytes);
        manifest.config.size = i64::try_from(config_bytes.len()).expect("config size must fit");
        let manifest_bytes = Bytes::from(serde_json::to_vec_pretty(&manifest).expect("manifest must encode"));
        let digest = LyquidPackDigest::from_oci_digest(&sha256_digest(&manifest_bytes)).expect("digest must parse");
        let noncanonical = OciArtifact::from_parts(
            digest,
            manifest_bytes.clone(),
            config_bytes.clone(),
            canonical.layer_bytes().to_vec(),
        )
        .expect("noncanonical artifact must validate");

        let pack = LyquidPack::try_from(noncanonical).expect("artifact must materialize");
        let rebuilt = OciArtifact::try_from(pack).expect("pack must convert back to an artifact");

        assert_ne!(rebuilt.manifest_bytes(), &manifest_bytes);
        assert_ne!(rebuilt.config_bytes(), &config_bytes);
        assert_ne!(rebuilt.digest().to_oci_digest(), sha256_digest(&manifest_bytes));
    }

    #[test]
    fn artifact_rejects_manifest_descriptor_size_and_layer_count_mismatches() {
        let artifact = OciArtifact::try_from(sample_pack()).expect("pack must convert to an artifact");
        let bad_digest = LyquidPackDigest::new([0xff; 32].into());
        assert!(matches!(
            OciArtifact::from_parts(
                bad_digest,
                artifact.manifest_bytes().clone(),
                artifact.config_bytes().clone(),
                artifact.layer_bytes().to_vec(),
            ),
            Err(Error::DigestMismatch { .. })
        ));

        let mut manifest = artifact.manifest().clone();
        manifest.config.digest = sha256_digest(b"different config bytes");
        let manifest_bytes = Bytes::from(serde_json::to_vec(&manifest).expect("manifest must encode"));
        let digest = LyquidPackDigest::from_oci_digest(&sha256_digest(&manifest_bytes)).expect("digest must parse");
        assert!(matches!(
            OciArtifact::from_parts(
                digest,
                manifest_bytes,
                artifact.config_bytes().clone(),
                artifact.layer_bytes().to_vec(),
            ),
            Err(Error::DigestMismatch { .. })
        ));

        let mut manifest = artifact.manifest().clone();
        manifest.config.size += 1;
        let manifest_bytes = Bytes::from(serde_json::to_vec(&manifest).expect("manifest must encode"));
        let digest = LyquidPackDigest::from_oci_digest(&sha256_digest(&manifest_bytes)).expect("digest must parse");
        assert!(matches!(
            OciArtifact::from_parts(
                digest,
                manifest_bytes,
                artifact.config_bytes().clone(),
                artifact.layer_bytes().to_vec(),
            ),
            Err(Error::SizeMismatch { .. })
        ));

        assert!(matches!(
            OciArtifact::from_parts(
                artifact.digest().clone(),
                artifact.manifest_bytes().clone(),
                artifact.config_bytes().clone(),
                artifact.layer_bytes()[1..].to_vec(),
            ),
            Err(Error::LayerCountMismatch { .. })
        ));
    }
}
