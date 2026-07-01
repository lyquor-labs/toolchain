//! OCI representation of Lyquid artifacts.
//!
//! `lyquor-oci` defines how a Lyquid pack is assembled, serialized, lazily read, and addressed by
//! digest or registry reference. Build and publish tools use it to create packs, the image store
//! uses it to persist OCI-compatible data, and hosting uses it to materialize WASM plus bundled
//! assets before VM startup.

/// Lyquid pack layout, manifests, and digest helpers.
pub mod pack;
/// OCI registry client helpers and reference parsing.
pub mod registry;
