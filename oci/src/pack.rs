use alloy_json_abi::JsonAbi;
use async_trait::async_trait;
use bytes::Bytes;
use lyquor_primitives::{B256, hex};
pub use oci_client::manifest::OciImageManifest;
use oci_client::{
    client::{Config, ImageLayer},
    manifest::{self, OciManifest},
};
use olpc_cjson::CanonicalFormatter;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
    str::FromStr,
    sync::Arc,
};
use tar::{Archive, Builder, EntryType, Header};
use thiserror::Error;
use xz2::{read::XzDecoder, write::XzEncoder};

/// OCI architecture value used for Lyquid WASM packs.
pub const LYQUID_PACK_METADATA_ARCHITECTURE_VALUE: &str = "wasm";
/// Default OCI OS value used to identify Lyquor runtime artifacts.
pub const LYQUID_PACK_METADATA_OS_DEFAULT: &str = "lyquor";
/// Default OCI OS version value for the current Lyquid pack format.
pub const LYQUID_PACK_METADATA_OS_VERSION_DEFAULT: &str = "v1";
const LYQUID_PACK_METADATA_AUTHOR_DEFAULT: &str = "Lyquid author(s)";
/// OCI layer annotation key for logical asset names.
pub const LYQUID_PACK_ASSET_NAME_KEY: &str = "assetName";
/// OCI layer annotation key for logical asset type.
pub const LYQUID_PACK_ASSET_TYPE_KEY: &str = "assetType";
/// Asset type annotation for EVM deployment bytecode.
pub const LYQUID_PACK_ASSET_TYPE_VALUE_EVM_DEPLOYMENT_BYTECODE: &str = "evm-deployment-bytecode";
/// Asset type annotation for auxiliary EVM bytecode.
pub const LYQUID_PACK_ASSET_TYPE_VALUE_EVM_AUXILIARY_BYTECODE: &str = "evm-auxiliary-bytecode";
/// Asset type annotation for the Lyquid WASM layer.
pub const LYQUID_PACK_ASSET_TYPE_VALUE_LYQUID: &str = "lyquid";
/// Asset type annotation for bundled static assets.
pub const LYQUID_PACK_ASSET_TYPE_VALUE_ASSETS: &str = "assets";
/// Asset type annotation for the Ethereum JSON ABI layer.
pub const LYQUID_PACK_ASSET_TYPE_VALUE_ETH_ABI: &str = "eth-abi";
/// Media type for the compressed Lyquid static-asset bundle layer.
pub const LYQUID_PACK_ASSETS_BUNDLE_MEDIA_TYPE: &str = "application/vnd.lyquor.lyquid.assets.v1.tar+xz";
/// Media type for the Ethereum JSON ABI layer.
pub const LYQUID_PACK_ETH_ABI_MEDIA_TYPE: &str = "application/vnd.lyquor.lyquid.eth-abi.v1+json";

const ASSET_BUNDLE_FILE_MODE: u32 = 0o644;
const ASSET_BUNDLE_UID: u64 = 0;
const ASSET_BUNDLE_GID: u64 = 0;
const ASSET_BUNDLE_MTIME: u64 = 0;
const ASSET_BUNDLE_XZ_PRESET: u32 = 6;

/// Logical layer categories used when reading Lyquid OCI manifests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LyquidPackLayerType {
    Lyquid,
    EvmBytecodes,
    Assets,
}

#[inline]
fn layer_asset_type(layer: &manifest::OciDescriptor) -> Option<&str> {
    layer
        .annotations
        .as_ref()
        .and_then(|a| a.get(LYQUID_PACK_ASSET_TYPE_KEY))
        .map(String::as_str)
}

#[inline]
fn layer_matches_asset_type(layer: &manifest::OciDescriptor, layer_type: LyquidPackLayerType) -> bool {
    match layer_type {
        LyquidPackLayerType::Lyquid => layer.media_type == manifest::WASM_LAYER_MEDIA_TYPE,
        LyquidPackLayerType::EvmBytecodes => {
            layer.media_type == manifest::IMAGE_LAYER_MEDIA_TYPE &&
                (layer_asset_type(layer) == Some(LYQUID_PACK_ASSET_TYPE_VALUE_EVM_DEPLOYMENT_BYTECODE) ||
                    layer_asset_type(layer) == Some(LYQUID_PACK_ASSET_TYPE_VALUE_EVM_AUXILIARY_BYTECODE))
        }
        LyquidPackLayerType::Assets => {
            layer.media_type == LYQUID_PACK_ASSETS_BUNDLE_MEDIA_TYPE &&
                layer_asset_type(layer) == Some(LYQUID_PACK_ASSET_TYPE_VALUE_ASSETS)
        }
    }
}

#[inline]
fn layer_asset_name(layer: &manifest::OciDescriptor) -> Option<&str> {
    layer
        .annotations
        .as_ref()
        .and_then(|a| a.get(LYQUID_PACK_ASSET_NAME_KEY))
        .map(String::as_str)
}

/// Errors returned while building or serializing full Lyquid packs.
#[derive(Debug, Error)]
pub enum Error {
    #[error("Fail to serialize")]
    SerializationError,
    #[error("Fail to deserialize")]
    DeserializationError,
    #[error("Invalid digest.")]
    InvalidDigest,
    #[error("Unsupported digest.")]
    UnsupportedDigest,
    #[error("Invalid asset path `{path}`.\nDetail: {detail}")]
    InvalidAssetPath { path: String, detail: String },
    #[error("Failed to build asset bundle.\nDetail: {0}")]
    AssetBundle(String),
}

/// Errors returned while lazily reading a Lyquid pack from OCI blobs.
#[derive(Debug, Error)]
pub enum LazyError {
    #[error("Invalid pack manifest.\nDetail: {0}")]
    InvalidManifest(String),
    #[error("Invalid pack metadata config.\nDetail: {0}")]
    InvalidMetadata(String),
    #[error("Invalid digest.")]
    InvalidDigest,
    #[error("Missing wasm layer in image manifest.")]
    MissingWasmLayer,
    #[error("Missing EVM bytecode layer in image manifest.")]
    MissingEvmBytecodeLayer,
    #[error("Missing EVM deployment bytecode.")]
    MissingEvmDeploymentBytecode,
    #[error("Asset `{0}` is not found in image manifest.")]
    AssetNotFound(String),
    #[error("Blob digest mismatch.\nExpected: {expected}\nActual: {actual}")]
    BlobDigestMismatch { expected: String, actual: String },
    #[error("Failed to read blob `{digest}`.\nDetail: {detail}")]
    BlobRead { digest: String, detail: String },
}

/// Calculates the SHA256 digest of bytes
/// Used for calculating layer digests
pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes)))
}

fn validate_asset_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("path is empty".to_owned());
    }
    if path.starts_with('/') {
        return Err("path must be relative".to_owned());
    }
    if path.ends_with('/') {
        return Err("path names a directory payload".to_owned());
    }
    if path.contains('\\') {
        return Err("path must use `/` separators".to_owned());
    }
    if path.contains('\0') {
        return Err("path contains a NUL byte".to_owned());
    }
    for component in path.split('/') {
        if component.is_empty() {
            return Err("path contains an empty component".to_owned());
        }
        if component == "." || component == ".." {
            return Err("path contains a traversal component".to_owned());
        }
    }
    Ok(())
}

fn append_asset_to_bundle(builder: &mut Builder<XzEncoder<Vec<u8>>>, path: &str, data: &Bytes) -> Result<(), Error> {
    validate_asset_path(path).map_err(|detail| Error::InvalidAssetPath {
        path: path.to_owned(),
        detail,
    })?;

    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_size(data.len() as u64);
    header.set_uid(ASSET_BUNDLE_UID);
    header.set_gid(ASSET_BUNDLE_GID);
    header.set_mtime(ASSET_BUNDLE_MTIME);
    header.set_mode(ASSET_BUNDLE_FILE_MODE);

    builder
        .append_data(&mut header, path, Cursor::new(data.as_ref()))
        .map_err(|e| Error::AssetBundle(e.to_string()))
}

fn build_asset_bundle(assets: &BTreeMap<String, Bytes>) -> Result<Bytes, Error> {
    let encoder = XzEncoder::new(Vec::new(), ASSET_BUNDLE_XZ_PRESET);
    let mut builder = Builder::new(encoder);
    for (path, data) in assets {
        append_asset_to_bundle(&mut builder, path, data)?;
    }

    let encoder = builder.into_inner().map_err(|e| Error::AssetBundle(e.to_string()))?;
    let bundle = encoder.finish().map_err(|e| Error::AssetBundle(e.to_string()))?;
    Ok(Bytes::from(bundle))
}

fn invalid_asset_bundle(detail: impl Into<String>) -> LazyError {
    LazyError::InvalidManifest(format!("Invalid asset bundle. {}", detail.into()))
}

fn unpack_asset_bundle(bundle: &[u8]) -> Result<BTreeMap<String, Bytes>, LazyError> {
    let decoder = XzDecoder::new(bundle);
    let mut archive = Archive::new(decoder);
    let mut assets = BTreeMap::<String, Bytes>::new();
    let entries = archive
        .entries()
        .map_err(|e| invalid_asset_bundle(format!("Failed to read archive entries: {e}")))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| invalid_asset_bundle(format!("Failed to read archive entry: {e}")))?;
        let entry_type = entry.header().entry_type();
        if !entry_type.is_file() {
            return Err(invalid_asset_bundle(format!(
                "Entry `{}` is not a regular file.",
                entry
                    .path()
                    .map_or_else(|_| "<invalid path>".to_owned(), |path| path.display().to_string())
            )));
        }

        let path_bytes = entry.path_bytes();
        let path = std::str::from_utf8(path_bytes.as_ref())
            .map_err(|_| invalid_asset_bundle("Entry path is not valid UTF-8."))?;
        validate_asset_path(path).map_err(|detail| invalid_asset_bundle(format!("Entry `{path}`: {detail}.")))?;
        let path = path.to_owned();
        if assets.contains_key(&path) {
            return Err(invalid_asset_bundle(format!("Entry `{path}` appears more than once.")));
        }

        let mut data = Vec::with_capacity(entry.size().try_into().unwrap_or_default());
        entry
            .read_to_end(&mut data)
            .map_err(|e| invalid_asset_bundle(format!("Failed to read entry `{path}`: {e}")))?;
        assets.insert(path, Bytes::from(data));
    }

    Ok(assets)
}

/// SHA-256 digest wrapper for a Lyquid OCI image manifest.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LyquidPackDigest {
    digest: B256,
}

impl LyquidPackDigest {
    /// digest should follow the format:  ALGO:DIGEST
    /// Example: sha256:affffdddd01e0000a18772cf00ffffe4da726000e07ffff5ff9eceb5f565ffffffff
    pub fn from_oci_digest(digest: &str) -> Result<Self, Error> {
        let (algo, digest) = digest.split_once(':').ok_or(Error::InvalidDigest)?;
        if digest.len() != 64 {
            return Err(Error::InvalidDigest);
        }

        if algo != "sha256" {
            return Err(Error::UnsupportedDigest);
        }

        Ok(Self {
            digest: B256::from_str(digest).map_err(|_| Error::InvalidDigest)?,
        })
    }

    /// Return the digest in OCI `sha256:<hex>` format.
    pub fn to_oci_digest(&self) -> String {
        format!("sha256:{}", hex::encode(self.digest))
    }

    /// Wrap a raw SHA-256 digest value.
    pub fn new(digest: B256) -> Self {
        Self { digest }
    }

    /// Return the raw digest bytes.
    pub fn digest(&self) -> &B256 {
        &self.digest
    }
}

// Reference: https://tag-runtime.cncf.io/wgs/wasm/deliverables/wasm-oci-artifact/
/// Metadata stored in the OCI config object for a Lyquid pack.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LyquidPackMetadata {
    /// Architecture (wasm fixed)
    architecture: String,
    /// Author of LyquidPack
    pub author: String,
    /// Description of LyquidPack
    pub description: String,
    /// Name of the Lyquid pack
    pub name: String,
    /// Used for identifing lyquor runtime
    pub os: String,
    /// Used for identifing lyquor version
    pub os_version: String,
    // TODO: component
}

impl LyquidPackMetadata {
    /// Create pack metadata, filling omitted fields with Lyquor defaults.
    pub fn new(
        name: &str, author: Option<&str>, desc: Option<&str>, os: Option<&str>, os_version: Option<&str>,
    ) -> Self {
        Self {
            architecture: LYQUID_PACK_METADATA_ARCHITECTURE_VALUE.to_owned(),
            name: name.to_owned(),
            author: author.unwrap_or(LYQUID_PACK_METADATA_AUTHOR_DEFAULT).to_owned(),
            description: desc.unwrap_or("").to_owned(),
            os: match os {
                Some(o) => o.to_owned(),
                None => LYQUID_PACK_METADATA_OS_DEFAULT.to_owned(),
            },
            os_version: match os_version {
                Some(ov) => ov.to_owned(),
                None => LYQUID_PACK_METADATA_OS_VERSION_DEFAULT.to_owned(),
            },
        }
    }

    /// Serialize metadata with canonical JSON formatting.
    pub fn to_json(&self) -> Result<Vec<u8>, Error> {
        serialize_canonical_json(self)
    }

    /// Parse metadata from JSON config bytes.
    pub fn from_json(config: &[u8]) -> Result<Self, Error> {
        serde_json::from_slice(config).map_err(|_| Error::DeserializationError)
    }
}

/// Complete Lyquid OCI pack with all blobs materialized in memory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LyquidPack {
    wasm: Bytes,
    evm_deployment: Bytes,
    evm_auxiliary: Option<BTreeMap<String, Bytes>>,
    assets: Option<BTreeMap<String, Bytes>>,
    eth_abi: Option<JsonAbi>,
    manifest: OciImageManifest,
    metadata: LyquidPackMetadata,
    digest: LyquidPackDigest,
}

impl LyquidPack {
    /// Build a full pack from materialized blobs and validate asset bundle construction.
    pub fn try_build_with_binary(
        wasm: Bytes, evm_deployment: Bytes, evm_auxiliary: Option<BTreeMap<String, Bytes>>,
        assets: Option<BTreeMap<String, Bytes>>, eth_abi: Option<JsonAbi>, metadata: LyquidPackMetadata,
    ) -> Result<Self, Error> {
        let eth_abi = eth_abi.filter(|abi| !abi.is_empty());
        let manifest = Self::build_oci_manifest(
            &wasm,
            &evm_deployment,
            evm_auxiliary.as_ref(),
            eth_abi.as_ref(),
            assets.as_ref(),
            &metadata,
        )?;
        let manifest_raw = Self::serialize_manifest_raw(&manifest);
        let digest = Self::manifest_digest(&manifest_raw);

        Ok(Self {
            wasm,
            evm_deployment,
            evm_auxiliary,
            assets,
            eth_abi,
            manifest,
            metadata,
            digest,
        })
    }

    /// Build a full pack from materialized blobs, panicking if assets are invalid.
    pub fn build_with_binary(
        wasm: Bytes, evm_deployment: Bytes, evm_auxiliary: Option<BTreeMap<String, Bytes>>,
        assets: Option<BTreeMap<String, Bytes>>, eth_abi: Option<JsonAbi>, metadata: LyquidPackMetadata,
    ) -> Self {
        Self::try_build_with_binary(wasm, evm_deployment, evm_auxiliary, assets, eth_abi, metadata)
            .expect("LyquidPack assets should produce a valid OCI bundle")
    }

    /// Return the manifest digest for this pack.
    pub fn digest(&self) -> &LyquidPackDigest {
        &self.digest
    }

    /// Return the pack metadata.
    pub fn metadata(&self) -> &LyquidPackMetadata {
        &self.metadata
    }

    /// Return the EVM deployment bytecode blob.
    pub fn evm_deployment_bytecode(&self) -> &Bytes {
        &self.evm_deployment
    }

    /// Return named auxiliary EVM bytecode blobs when present.
    pub fn evm_auxiliary_bytecodes(&self) -> Option<&BTreeMap<String, Bytes>> {
        self.evm_auxiliary.as_ref()
    }

    /// Return the Lyquid WASM blob.
    pub fn wasm(&self) -> &Bytes {
        &self.wasm
    }

    /// Consume the pack and return its WASM blob.
    pub fn into_wasm(self) -> Bytes {
        self.wasm
    }

    /// Return bundled static assets when present.
    pub fn assets(&self) -> Option<&BTreeMap<String, Bytes>> {
        self.assets.as_ref()
    }

    /// Return the Ethereum JSON ABI layer contents when present.
    pub fn eth_abi(&self) -> Option<&JsonAbi> {
        self.eth_abi.as_ref()
    }

    /// Return the OCI image manifest.
    pub fn manifest(&self) -> &OciImageManifest {
        &self.manifest
    }

    /// Serialize this full pack into the single-file `lyquid.pack` format written by build tools.
    pub fn to_repo_bytes(&self) -> Result<Vec<u8>, Error> {
        serialize_canonical_json(self)
    }

    /// Deserialize a full pack from `lyquid.pack` bytes produced by [`Self::to_repo_bytes`].
    pub fn from_repo_bytes(data: &[u8]) -> Result<Self, Error> {
        serde_json::from_slice(data).map_err(|_| Error::DeserializationError)
    }

    /// Convert this pack into OCI layers and config suitable for push.
    pub fn to_oci_push_parts(&self) -> Result<(Vec<ImageLayer>, Config), Error> {
        Ok((
            Self::build_oci_layers(
                &self.wasm,
                self.evm_deployment_bytecode(),
                self.evm_auxiliary_bytecodes(),
                self.eth_abi.as_ref(),
                self.assets.as_ref(),
            )?,
            Self::build_oci_config(&self.metadata)?,
        ))
    }

    fn build_oci_manifest(
        wasm: &Bytes, evm_deployment: &Bytes, evm_auxiliary: Option<&BTreeMap<String, Bytes>>,
        eth_abi: Option<&JsonAbi>, assets: Option<&BTreeMap<String, Bytes>>, metadata: &LyquidPackMetadata,
    ) -> Result<OciImageManifest, Error> {
        let layers = Self::build_oci_layers(wasm, evm_deployment, evm_auxiliary, eth_abi, assets)?;
        let config = Self::build_oci_config(metadata).expect("LyquidPack metadata serialization should not fail");

        let mut oci_manifest = OciImageManifest::build(&layers, &config, None);
        oci_manifest.media_type = Some(manifest::OCI_IMAGE_MEDIA_TYPE.to_owned());
        Ok(oci_manifest)
    }

    fn build_oci_layers(
        wasm: &Bytes, evm_deployment: &Bytes, evm_auxiliary: Option<&BTreeMap<String, Bytes>>,
        eth_abi: Option<&JsonAbi>, assets: Option<&BTreeMap<String, Bytes>>,
    ) -> Result<Vec<ImageLayer>, Error> {
        let mut layers = Vec::new();
        layers.push(ImageLayer {
            data: wasm.clone(),
            media_type: manifest::WASM_LAYER_MEDIA_TYPE.to_owned(),
            annotations: Some(BTreeMap::from([(
                LYQUID_PACK_ASSET_TYPE_KEY.to_owned(),
                LYQUID_PACK_ASSET_TYPE_VALUE_LYQUID.to_owned(),
            )])),
        });
        if let Some(eth_abi) = eth_abi.filter(|abi| !abi.is_empty()) {
            layers.push(ImageLayer {
                data: serialize_canonical_json(eth_abi)?.into(),
                media_type: LYQUID_PACK_ETH_ABI_MEDIA_TYPE.to_owned(),
                annotations: Some(BTreeMap::from([(
                    LYQUID_PACK_ASSET_TYPE_KEY.to_owned(),
                    LYQUID_PACK_ASSET_TYPE_VALUE_ETH_ABI.to_owned(),
                )])),
            });
        }
        layers.push(ImageLayer {
            data: evm_deployment.clone(),
            media_type: manifest::IMAGE_LAYER_MEDIA_TYPE.to_owned(),
            annotations: Some(BTreeMap::from([(
                LYQUID_PACK_ASSET_TYPE_KEY.to_owned(),
                LYQUID_PACK_ASSET_TYPE_VALUE_EVM_DEPLOYMENT_BYTECODE.to_owned(),
            )])),
        });
        for (name, bytecode) in evm_auxiliary.into_iter().flatten() {
            layers.push(ImageLayer {
                data: bytecode.clone(),
                media_type: manifest::IMAGE_LAYER_MEDIA_TYPE.to_owned(),
                annotations: Some(BTreeMap::from([
                    (
                        LYQUID_PACK_ASSET_TYPE_KEY.to_owned(),
                        LYQUID_PACK_ASSET_TYPE_VALUE_EVM_AUXILIARY_BYTECODE.to_owned(),
                    ),
                    (LYQUID_PACK_ASSET_NAME_KEY.to_owned(), name.clone()),
                ])),
            });
        }
        if let Some(assets) = assets.filter(|assets| !assets.is_empty()) {
            let bundle = build_asset_bundle(assets)?;
            layers.push(ImageLayer {
                data: bundle,
                media_type: LYQUID_PACK_ASSETS_BUNDLE_MEDIA_TYPE.to_owned(),
                annotations: Some(BTreeMap::from([(
                    LYQUID_PACK_ASSET_TYPE_KEY.to_owned(),
                    LYQUID_PACK_ASSET_TYPE_VALUE_ASSETS.to_owned(),
                )])),
            });
        }
        Ok(layers)
    }

    fn build_oci_config(metadata: &LyquidPackMetadata) -> Result<Config, Error> {
        Ok(Config {
            data: metadata.to_json()?.into(),
            media_type: manifest::WASM_CONFIG_MEDIA_TYPE.to_owned(),
            annotations: None,
        })
    }

    /// Serialize an OCI image manifest using the canonical bytes used for digesting.
    pub fn serialize_manifest_raw(manifest: &OciImageManifest) -> Vec<u8> {
        // OCI manifest digests are over the exact manifest bytes stored by the registry, not just
        // the semantic manifest fields. `oci_client` canonicalizes manifest JSON before upload per
        // https://github.com/opencontainers/image-spec/blob/main/considerations.md#json, so local
        // digest generation must use the same formatter to keep `LyquidPack::digest()` aligned
        // with the digest returned by the registry for the same manifest.
        serialize_canonical_json(&OciManifest::Image(manifest.clone()))
            .expect("OCI manifest serialization should not fail")
    }

    fn manifest_digest(manifest_bytes: &[u8]) -> LyquidPackDigest {
        let digest = sha256_digest(manifest_bytes);
        LyquidPackDigest::from_oci_digest(&digest).expect("Generated SHA256 digest should always be valid")
    }
}

/// Serialize a value using the canonical JSON formatter used for Lyquid OCI config and manifests.
pub fn serialize_canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    // Keep all JSON payloads that feed digest-sensitive OCI paths or future OCI-aligned node
    // transport on the same canonical serializer to avoid byte-level drift across producers.
    let mut body = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut body, CanonicalFormatter::new());
    value
        .serialize(&mut serializer)
        .map_err(|_| Error::SerializationError)?;
    Ok(body)
}

/// Async blob reader used by lazy Lyquid pack materialization.
#[async_trait]
pub trait PackBlobReader: Send + Sync {
    /// Read an OCI blob by digest.
    async fn read_blob(&self, digest: &str) -> Result<Bytes, LazyError>;
}

/// Lazy Lyquid pack backed by an OCI manifest and on-demand blob reader.
#[derive(Clone)]
pub struct LazyLyquidPack {
    manifest: OciImageManifest,
    metadata: LyquidPackMetadata,
    digest: LyquidPackDigest,
    reader: Arc<dyn PackBlobReader>,
}

impl std::fmt::Debug for LazyLyquidPack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyLyquidPack")
            .field("manifest", &self.manifest)
            .field("metadata", &self.metadata)
            .field("digest", &self.digest)
            .finish()
    }
}

impl LazyLyquidPack {
    /// Create a lazy pack from parsed manifest, metadata, digest, and blob reader.
    pub fn new(
        manifest: OciImageManifest, metadata: LyquidPackMetadata, digest: LyquidPackDigest,
        reader: Arc<dyn PackBlobReader>,
    ) -> Self {
        Self {
            manifest,
            metadata,
            digest,
            reader,
        }
    }

    /// Parse metadata config bytes and create a lazy pack.
    pub fn from_manifest_and_config(
        manifest: OciImageManifest, metadata_config: &[u8], digest: LyquidPackDigest, reader: Arc<dyn PackBlobReader>,
    ) -> Result<Self, LazyError> {
        let metadata =
            LyquidPackMetadata::from_json(metadata_config).map_err(|e| LazyError::InvalidMetadata(e.to_string()))?;
        Ok(Self::new(manifest, metadata, digest, reader))
    }

    /// Return the manifest digest for this pack.
    pub fn digest(&self) -> &LyquidPackDigest {
        &self.digest
    }

    /// Return parsed pack metadata.
    pub fn metadata(&self) -> &LyquidPackMetadata {
        &self.metadata
    }

    /// Return the OCI image manifest.
    pub fn manifest(&self) -> &OciImageManifest {
        &self.manifest
    }

    async fn load_blob_by_digest(&self, oci_digest: &str) -> Result<Bytes, LazyError> {
        let bytes = self.reader.read_blob(oci_digest).await?;
        let actual = sha256_digest(&bytes);
        if actual != oci_digest {
            return Err(LazyError::BlobDigestMismatch {
                expected: oci_digest.to_owned(),
                actual,
            });
        }
        Ok(bytes)
    }

    fn find_layer_by_asset_type(
        &self, layer_type: LyquidPackLayerType, missing: LazyError, duplicate_msg: &'static str,
    ) -> Result<&manifest::OciDescriptor, LazyError> {
        let mut found: Option<&manifest::OciDescriptor> = None;
        for layer in &self.manifest.layers {
            if !layer_matches_asset_type(layer, layer_type) {
                continue;
            }
            if found.replace(layer).is_some() {
                return Err(LazyError::InvalidManifest(duplicate_msg.to_owned()));
            }
        }
        found.ok_or(missing)
    }

    fn asset_bundle_layer(&self) -> Result<Option<&manifest::OciDescriptor>, LazyError> {
        let mut found = None;
        for layer in &self.manifest.layers {
            if layer_asset_type(layer) != Some(LYQUID_PACK_ASSET_TYPE_VALUE_ASSETS) {
                continue;
            }
            if layer.media_type != LYQUID_PACK_ASSETS_BUNDLE_MEDIA_TYPE {
                return Err(LazyError::InvalidManifest(format!(
                    "Asset bundle layer has unsupported media type `{}`.",
                    layer.media_type
                )));
            }
            if found.replace(layer).is_some() {
                return Err(LazyError::InvalidManifest(
                    "Image contains duplicate asset bundle layers.".to_owned(),
                ));
            }
        }
        Ok(found)
    }

    fn eth_abi_layer(&self) -> Result<Option<&manifest::OciDescriptor>, LazyError> {
        let mut found = None;
        for layer in &self.manifest.layers {
            if layer_asset_type(layer) != Some(LYQUID_PACK_ASSET_TYPE_VALUE_ETH_ABI) {
                continue;
            }
            if layer.media_type != LYQUID_PACK_ETH_ABI_MEDIA_TYPE {
                return Err(LazyError::InvalidManifest(format!(
                    "Ethereum ABI layer has unsupported media type `{}`.",
                    layer.media_type
                )));
            }
            if found.replace(layer).is_some() {
                return Err(LazyError::InvalidManifest(
                    "Image contains duplicate Ethereum ABI layers.".to_owned(),
                ));
            }
        }
        Ok(found)
    }

    fn bytecode_layers(&self) -> impl Iterator<Item = &manifest::OciDescriptor> + '_ {
        self.manifest
            .layers
            .iter()
            .filter(|layer| layer_matches_asset_type(layer, LyquidPackLayerType::EvmBytecodes))
    }

    /// Return the raw digest for the WASM layer without loading its blob.
    pub fn wasm_digest(&self) -> Result<B256, LazyError> {
        let layer = self.find_layer_by_asset_type(
            LyquidPackLayerType::Lyquid,
            LazyError::MissingWasmLayer,
            "Image contains duplicate wasm layers.",
        )?;
        let digest = LyquidPackDigest::from_oci_digest(&layer.digest).map_err(|_| LazyError::InvalidDigest)?;
        Ok(*digest.digest())
    }

    /// Load and verify the WASM blob.
    pub async fn load_wasm(&self) -> Result<Bytes, LazyError> {
        let layer = self.find_layer_by_asset_type(
            LyquidPackLayerType::Lyquid,
            LazyError::MissingWasmLayer,
            "Image contains duplicate wasm layers.",
        )?;
        self.load_blob_by_digest(&layer.digest).await
    }

    /// Load and verify the optional Ethereum JSON ABI layer.
    pub async fn load_eth_abi(&self) -> Result<Option<JsonAbi>, LazyError> {
        let Some(layer) = self.eth_abi_layer()? else {
            return Ok(None);
        };
        let data = self.load_blob_by_digest(&layer.digest).await?;
        let eth_abi = serde_json::from_slice(&data)
            .map_err(|e| LazyError::InvalidManifest(format!("Invalid Ethereum ABI layer JSON: {e}")))?;
        Ok(Some(eth_abi))
    }

    /// Load and verify deployment plus auxiliary EVM bytecode blobs.
    pub async fn load_evm_bytecodes(&self) -> Result<(Bytes, Option<BTreeMap<String, Bytes>>), LazyError> {
        let mut deployment = Bytes::new();
        let mut auxiliary = BTreeMap::<String, Bytes>::new();
        for layer in self.bytecode_layers() {
            let asset_type = layer_asset_type(layer)
                .ok_or_else(|| LazyError::InvalidManifest("Asset layer has a bad `assetType` annotation.".to_owned()))?
                .to_owned();
            if asset_type == LYQUID_PACK_ASSET_TYPE_VALUE_EVM_DEPLOYMENT_BYTECODE {
                let data = self.load_blob_by_digest(&layer.digest).await?;
                if deployment.is_empty() {
                    deployment = data;
                } else {
                    return Err(LazyError::InvalidManifest(
                        "EVM_Bytecode layer has duplicated EVM deployment bytecode.".to_owned(),
                    ));
                }
            } else {
                let asset_name = layer_asset_name(layer)
                    .ok_or_else(|| {
                        LazyError::InvalidManifest("Asset layer is missing `assetName` annotation.".to_owned())
                    })?
                    .to_owned();
                let data = self.load_blob_by_digest(&layer.digest).await?;
                auxiliary.insert(asset_name, data);
            }
        }

        if deployment.is_empty() {
            return Err(LazyError::MissingEvmDeploymentBytecode);
        }

        Ok((deployment, if !auxiliary.is_empty() { Some(auxiliary) } else { None }))
    }

    /// Load one bundled static asset by name.
    pub async fn load_asset(&self, asset_name: &str) -> Result<Bytes, LazyError> {
        let mut assets = self.load_assets().await?;
        assets
            .remove(asset_name)
            .ok_or_else(|| LazyError::AssetNotFound(asset_name.to_owned()))
    }

    /// Load all bundled static assets.
    pub async fn load_assets(&self) -> Result<BTreeMap<String, Bytes>, LazyError> {
        let Some(layer) = self.asset_bundle_layer()? else {
            return Ok(BTreeMap::new());
        };
        let data = self.load_blob_by_digest(&layer.digest).await?;
        unpack_asset_bundle(&data)
    }

    /// Load all blobs and return a fully materialized pack.
    pub async fn materialize_full(&self) -> Result<LyquidPack, LazyError> {
        let wasm = self.load_wasm().await?;
        let (evm_deployment, evm_auxiliary) = self.load_evm_bytecodes().await?;
        let eth_abi = self.load_eth_abi().await?;
        let assets = self.load_assets().await?;
        let assets = if assets.is_empty() { None } else { Some(assets) };

        Ok(LyquidPack {
            wasm,
            evm_deployment,
            evm_auxiliary,
            assets,
            eth_abi,
            metadata: self.metadata.clone(),
            manifest: self.manifest.clone(),
            digest: self.digest.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lyquor_test::test;
    use std::io::Write;

    #[derive(Clone)]
    struct MemoryBlobReader {
        blobs: BTreeMap<String, Bytes>,
    }

    #[async_trait]
    impl PackBlobReader for MemoryBlobReader {
        async fn read_blob(&self, digest: &str) -> Result<Bytes, LazyError> {
            self.blobs.get(digest).cloned().ok_or_else(|| LazyError::BlobRead {
                digest: digest.to_owned(),
                detail: "missing test blob".to_owned(),
            })
        }
    }

    fn sample_pack(assets: Option<BTreeMap<String, Bytes>>) -> LyquidPack {
        LyquidPack::build_with_binary(
            Bytes::from_static(&[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]),
            Bytes::from_static(&[0x60, 0x80, 0x60, 0x40, 0x52]),
            None,
            assets,
            None,
            LyquidPackMetadata::new("asset-test", Some("Lyquor"), Some("Asset bundle test"), None, None),
        )
    }

    fn asset_bundle_payload(pack: &LyquidPack) -> Bytes {
        let (layers, _) = pack.to_oci_push_parts().expect("push parts");
        layers
            .into_iter()
            .find(|layer| layer.media_type == LYQUID_PACK_ASSETS_BUNDLE_MEDIA_TYPE)
            .expect("asset bundle layer")
            .data
    }

    #[test(tokio::test)]
    async fn eth_abi_layer_is_optional() {
        let pack = sample_pack(None);
        assert!(pack.eth_abi().is_none());
        assert!(
            !pack
                .manifest()
                .layers
                .iter()
                .any(|layer| layer_asset_type(layer) == Some(LYQUID_PACK_ASSET_TYPE_VALUE_ETH_ABI))
        );

        let lazy = lazy_pack_from_pack(&pack);
        assert!(
            lazy.load_eth_abi()
                .await
                .expect("missing ABI layer should be accepted")
                .is_none()
        );
    }

    fn lazy_pack_from_pack(pack: &LyquidPack) -> LazyLyquidPack {
        let (layers, _) = pack.to_oci_push_parts().expect("push parts");
        let blobs = pack
            .manifest()
            .layers
            .iter()
            .zip(layers)
            .map(|(descriptor, layer)| (descriptor.digest.clone(), layer.data))
            .collect();
        LazyLyquidPack::new(
            pack.manifest().clone(),
            pack.metadata().clone(),
            pack.digest().clone(),
            Arc::new(MemoryBlobReader { blobs }),
        )
    }

    fn tar_archive_with_entry(path: &str, entry_type: EntryType) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        let mut header = Header::new_gnu();
        header.set_entry_type(entry_type);
        header.set_size(0);
        header.set_uid(ASSET_BUNDLE_UID);
        header.set_gid(ASSET_BUNDLE_GID);
        header.set_mtime(ASSET_BUNDLE_MTIME);
        header.set_mode(ASSET_BUNDLE_FILE_MODE);
        builder
            .append_data(&mut header, path, Cursor::new(Vec::<u8>::new()))
            .expect("test archive entry");
        builder.into_inner().expect("finish test archive")
    }

    fn xz_bytes(data: &[u8]) -> Bytes {
        let mut encoder = XzEncoder::new(Vec::new(), ASSET_BUNDLE_XZ_PRESET);
        encoder.write_all(data).expect("write test xz");
        Bytes::from(encoder.finish().expect("finish test xz"))
    }

    fn xz_archive_with_entry(path: &str, entry_type: EntryType) -> Bytes {
        xz_bytes(&tar_archive_with_entry(path, entry_type))
    }

    fn xz_archive_with_rewritten_path(original_path: &str, rewritten_path: &str) -> Bytes {
        assert_eq!(original_path.len(), rewritten_path.len());
        let mut archive = tar_archive_with_entry(original_path, EntryType::Regular);
        archive[..100].fill(0);
        archive[..rewritten_path.len()].copy_from_slice(rewritten_path.as_bytes());
        archive[148..156].fill(b' ');
        let checksum = archive[..512].iter().fold(0u32, |sum, byte| sum + u32::from(*byte));
        let checksum_field = format!("{checksum:06o}\0 ");
        archive[148..156].copy_from_slice(checksum_field.as_bytes());
        xz_bytes(&archive)
    }

    #[test]
    fn asset_bundle_layer_layout_and_headers_are_stable() {
        let assets = BTreeMap::from([
            ("nested/app.js".to_owned(), Bytes::from_static(b"console.log('ok');")),
            ("index.html".to_owned(), Bytes::from_static(b"<html></html>")),
        ]);

        let pack = sample_pack(Some(assets));

        let asset_layers = pack
            .manifest()
            .layers
            .iter()
            .filter(|layer| layer_asset_type(layer) == Some(LYQUID_PACK_ASSET_TYPE_VALUE_ASSETS))
            .collect::<Vec<_>>();
        assert_eq!(asset_layers.len(), 1);
        assert_eq!(asset_layers[0].media_type, LYQUID_PACK_ASSETS_BUNDLE_MEDIA_TYPE);
        assert_eq!(
            asset_layers[0]
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get(LYQUID_PACK_ASSET_NAME_KEY)),
            None
        );

        let bundle = asset_bundle_payload(&pack);

        let mut archive = Archive::new(XzDecoder::new(bundle.as_ref()));
        let mut paths = Vec::new();
        for entry in archive.entries().expect("archive entries") {
            let entry = entry.expect("archive entry");
            assert!(entry.header().entry_type().is_file());
            assert_eq!(entry.header().uid().expect("uid"), ASSET_BUNDLE_UID);
            assert_eq!(entry.header().gid().expect("gid"), ASSET_BUNDLE_GID);
            assert_eq!(entry.header().mtime().expect("mtime"), ASSET_BUNDLE_MTIME);
            assert_eq!(entry.header().mode().expect("mode"), ASSET_BUNDLE_FILE_MODE);
            paths.push(entry.path().expect("path").to_string_lossy().into_owned());
        }
        assert_eq!(paths, vec!["index.html", "nested/app.js"]);
    }

    #[test(tokio::test)]
    async fn lazy_load_assets_unpacks_nested_asset_bundle() {
        let assets = BTreeMap::from([
            ("index.html".to_owned(), Bytes::from_static(b"<html></html>")),
            ("nested/app.js".to_owned(), Bytes::from_static(b"console.log('ok');")),
        ]);
        let pack = sample_pack(Some(assets.clone()));
        let lazy = lazy_pack_from_pack(&pack);

        assert_eq!(lazy.load_assets().await.expect("load assets"), assets);
        assert_eq!(
            lazy.load_asset("nested/app.js").await.expect("load nested asset"),
            Bytes::from_static(b"console.log('ok');")
        );
    }

    #[test]
    fn try_build_rejects_directory_asset_path() {
        let err = LyquidPack::try_build_with_binary(
            Bytes::from_static(&[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]),
            Bytes::from_static(&[0x60, 0x80, 0x60, 0x40, 0x52]),
            None,
            Some(BTreeMap::from([("nested/".to_owned(), Bytes::from_static(b""))])),
            None,
            LyquidPackMetadata::new("bad-asset", None, None, None, None),
        )
        .expect_err("directory asset path must be rejected");

        assert!(matches!(
            err,
            Error::InvalidAssetPath { path, .. } if path == "nested/"
        ));
    }

    #[test]
    fn unpack_asset_bundle_rejects_non_files_and_traversal() {
        let symlink_bundle = xz_archive_with_entry("linked.txt", EntryType::Symlink);
        let symlink_err = unpack_asset_bundle(&symlink_bundle).expect_err("symlink entry must be rejected");
        assert!(matches!(symlink_err, LazyError::InvalidManifest(_)));

        let traversal_bundle = xz_archive_with_rewritten_path("xx/secret.txt", "../secret.txt");
        let traversal_err = unpack_asset_bundle(&traversal_bundle).expect_err("traversal path must be rejected");
        assert!(matches!(traversal_err, LazyError::InvalidManifest(_)));
    }
}
