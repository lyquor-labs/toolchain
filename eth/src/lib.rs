//! Ethereum integration layer for Lyquor tooling and sequencing.
//!
//! This crate contains the Ethereum-specific pieces that sit outside the core Lyquor runtime:
//! Solidity primitive bindings, generated contract artifacts, signing helpers, transaction
//! submission, and contract deployment utilities. Sequencing backends and deployment tools use this
//! crate when they need Ethereum wire formats or JSON-RPC transaction flows.

use k256::ecdsa::SigningKey;

pub use alloy_signer::Signer;

mod submitter;
pub use submitter::{EthSubmitter, EthSubmitterError};

pub mod eth {
    alloy_sol_types::sol! {
        "src/lib/primitives.sol"
    }
}

/// Extract a named Solidity template section from `template.sol`.
pub fn extract_code_sections(marker: &str) -> String {
    let mut defs = String::new();
    let mut recording = false;
    let begin_marker = format!("//// {marker}-begin");
    let end_marker = format!("//// {marker}-end");

    for line in String::from(include_str!("./template.sol")).lines() {
        if recording {
            if line.contains(&end_marker) {
                recording = false;
                continue;
            }
            defs.push_str(line);
            defs.push('\n');
        } else if line.contains(&begin_marker) {
            assert!(!recording, "Incorrect template markers.");
            recording = true;
        }
    }
    defs
}

/// Write bundled Solidity library files into `dest_dir/lib`.
pub fn write_library_files(dest_dir: &std::path::Path) -> std::io::Result<()> {
    let lib_dir = dest_dir.join("lib");
    std::fs::create_dir_all(&lib_dir)?;

    const LIBS: &[(&str, &str)] = &[
        ("primitives.sol", include_str!("./lib/primitives.sol")),
        ("ed25519.sol", include_str!("./lib/ed25519.sol")),
        ("crypto.sol", include_str!("./lib/crypto.sol")),
        ("oracle.sol", include_str!("./lib/oracle.sol")),
    ];

    for (name, content) in LIBS {
        std::fs::write(lib_dir.join(name), content)?;
    }
    Ok(())
}

/// Local secp256k1 signer type used by Ethereum transaction helpers.
pub type LocalSigner = alloy_signer_local::LocalSigner<SigningKey>;

/// Helper function to create SigningKey from Bytes
pub fn signer_from_bytes(raw: Vec<u8>) -> anyhow::Result<LocalSigner> {
    let key = (|| {
        let secret_bytes: [u8; 32] = raw.try_into().ok()?;
        SigningKey::from_bytes(&secret_bytes.into()).ok()
    })()
    .ok_or_else(|| anyhow::anyhow!("Invalid private key"))?;
    Ok(alloy_signer_local::LocalSigner::from_signing_key(key))
}

/// Helper function to create SigningKey from hex
pub fn signer_from_hex(hex: &str) -> anyhow::Result<LocalSigner> {
    let key = (|| {
        let hex = hex.strip_prefix("0x").unwrap_or(hex);
        let secret_bytes: [u8; 32] = lyquor_primitives::hex::decode(hex).ok()?.try_into().ok()?;
        SigningKey::from_bytes(&secret_bytes.into()).ok()
    })()
    .ok_or_else(|| anyhow::anyhow!("Invalid private key"))?;
    Ok(alloy_signer_local::LocalSigner::from_signing_key(key))
}
