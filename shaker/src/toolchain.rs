use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use directories::BaseDirs;
use lyquor_primitives::hex;
use sha2::{Digest, Sha256};

const RUSTFLAGS_SEPARATOR: &str = "\u{1f}";
const REQUIRED_TARGET_LIB_PREFIXES: [&str; 3] = ["libcore-", "liballoc-", "libcompiler_builtins-"];

/// Lyquid guest Rust toolchain and custom rust-std configuration.
#[derive(Clone, Copy, Debug)]
pub struct ToolchainSpec {
    pub required_toolchain: &'static str,
    pub required_rust_version: &'static str,
    pub target: &'static str,
    pub rust_std_release: &'static str,
    pub rust_std_url: &'static str,
    pub rust_std_archive_sha256: &'static str,
    pub rust_std_archive_name: &'static str,
    pub rust_std_extracted_root_dir: &'static str,
    pub rust_std_sysroot_dir: &'static str,
    pub base_rustflags: &'static [&'static str],
}

impl ToolchainSpec {
    /// Returns the default atomics-enabled Lyquid WASM toolchain spec.
    pub const fn lyquid_default() -> Self {
        Self {
            required_toolchain: "1.96.0",
            required_rust_version: "rustc 1.96.0 (ac68faa20 2026-05-25)",
            target: "wasm32-unknown-unknown",
            rust_std_release: "atomics-20260610",
            rust_std_url: "https://github.com/lyquor-labs/rust-toolchain/releases/download/atomics-20260610/rust-std-1.96.0-wasm32-unknown-unknown.tar.xz",
            rust_std_archive_sha256: "sha256:f766b1eb16fb28bdfcdef387c628cca91c23eab0086acecf08fc6cedc392d240",
            rust_std_archive_name: "rust-std-1.96.0-wasm32-unknown-unknown.tar.xz",
            rust_std_extracted_root_dir: "rust-std-1.96.0-wasm32-unknown-unknown",
            rust_std_sysroot_dir: "rust-std-wasm32-unknown-unknown",
            base_rustflags: &[
                "-Ctarget-feature=+atomics",
                "-Clink-arg=--export=__stack_pointer",
                "-Clink-arg=--import-memory",
                "-Clink-arg=--shared-memory",
                "-Clink-arg=--export=__wasm_init_tls",
                "-Clink-arg=--export=__tls_size",
                "-Clink-arg=--export=__tls_align",
                "-Clink-arg=--export=__tls_base",
            ],
        }
    }

    /// Returns the unwind-capable Lyquid WASM toolchain spec.
    pub const fn lyquid_unwind() -> Self {
        Self {
            required_toolchain: "1.96.0",
            required_rust_version: "rustc 1.96.0 (ac68faa20 2026-05-25)",
            target: "wasm32-unknown-unknown",
            rust_std_release: "unwind-20260610",
            rust_std_url: "https://github.com/lyquor-labs/rust-toolchain/releases/download/unwind-20260610/rust-std-1.96.0-wasm32-unknown-unknown.tar.xz",
            rust_std_archive_sha256: "sha256:2244256cc8cb79e954393494ab70e11de9597a405f545dc99e6df51c4afa73c2",
            rust_std_archive_name: "rust-std-1.96.0-wasm32-unknown-unknown.tar.xz",
            rust_std_extracted_root_dir: "rust-std-1.96.0-wasm32-unknown-unknown",
            rust_std_sysroot_dir: "rust-std-wasm32-unknown-unknown",
            base_rustflags: &[
                "-Ctarget-feature=+atomics,+exception-handling",
                "-Cpanic=unwind",
                "-Cllvm-args=-wasm-use-legacy-eh=false",
                "-Cllvm-args=-wasm-enable-eh",
                "-Clink-arg=--export=__stack_pointer",
                "-Clink-arg=--import-memory",
                "-Clink-arg=--shared-memory",
                "-Clink-arg=--export=__wasm_init_tls",
                "-Clink-arg=--export=__tls_size",
                "-Clink-arg=--export=__tls_align",
                "-Clink-arg=--export=__tls_base",
            ],
        }
    }

    /// Selects the Lyquid toolchain spec from `LYQUID_TOOLCHAIN_SPEC`.
    pub fn from_env() -> anyhow::Result<Self> {
        let spec = std::env::var("LYQUID_TOOLCHAIN_SPEC")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "default".to_string());

        match spec.as_str() {
            "default" | "atomics" => Ok(Self::lyquid_default()),
            "unwind" => Ok(Self::lyquid_unwind()),
            _ => Err(anyhow::anyhow!(
                "Unsupported LYQUID_TOOLCHAIN_SPEC '{spec}'. Expected one of: default, atomics, unwind."
            )),
        }
    }

    /// Returns whether a stripped `LYQUID_` env key should be forwarded to nested Cargo.
    pub fn should_forward_lyquid_env(self, stripped_env_key: &str) -> bool {
        stripped_env_key != "RUSTFLAGS" &&
            stripped_env_key != "CARGO_ENCODED_RUSTFLAGS" &&
            stripped_env_key != "TOOLCHAIN_SPEC"
    }

    /// Returns whether this spec needs the release opt-level workaround.
    pub fn requires_release_opt_level_workaround(self) -> bool {
        self.rust_std_release == "unwind-20260610"
    }

    /// Resolves the Rust toolchain name and verifies the exact compiler version.
    pub fn resolve_toolchain(self) -> anyhow::Result<String> {
        let toolchain = match std::env::var("LYQUID_TOOLCHAIN") {
            Ok(value) if !value.trim().is_empty() => value,
            _ => self.required_toolchain.into(),
        };

        if !check_rustc_version(&toolchain, self.required_rust_version)? {
            let found = rustc_version(&toolchain).unwrap_or_else(|_| "<unknown>".to_string());
            return Err(anyhow::anyhow!(
                "Rust compiler version mismatch for toolchain '{}': expected '{}', found '{}'.",
                toolchain,
                self.required_rust_version,
                found
            ));
        }

        Ok(toolchain)
    }

    /// Builds `CARGO_ENCODED_RUSTFLAGS` for the Lyquid guest compilation.
    pub fn encoded_rustflags(self, custom_sysroot: &Path) -> String {
        let mut rustflags = self
            .base_rustflags
            .iter()
            .map(|flag| (*flag).to_string())
            .collect::<Vec<_>>();
        rustflags.push(format!("--sysroot={}", custom_sysroot.display()));

        if let Ok(value) = std::env::var("LYQUID_RUSTFLAGS") {
            rustflags.extend(value.split_whitespace().map(std::string::ToString::to_string));
        }
        if let Ok(value) = std::env::var("LYQUID_CARGO_ENCODED_RUSTFLAGS") {
            rustflags.extend(
                value
                    .split(RUSTFLAGS_SEPARATOR)
                    .filter(|flag| !flag.is_empty())
                    .map(std::string::ToString::to_string),
            );
        }

        rustflags.join(RUSTFLAGS_SEPARATOR)
    }

    /// Verifies that a custom rust-std sysroot contains the required target libraries.
    pub fn verify_custom_rust_std_sysroot(self, sysroot_path: &Path) -> anyhow::Result<()> {
        if self.has_custom_rust_std(sysroot_path) {
            return Ok(());
        }

        let target_lib_dir = self.target_lib_dir(sysroot_path);
        Err(anyhow::anyhow!(
            "Custom rust-std sysroot is incomplete at {}. Expected {} to contain files prefixed with: {}",
            sysroot_path.display(),
            target_lib_dir.display(),
            REQUIRED_TARGET_LIB_PREFIXES.join(", ")
        ))
    }

    /// Downloads and installs the configured custom rust-std sysroot when missing.
    pub async fn ensure_custom_rust_std_sysroot(self) -> anyhow::Result<PathBuf> {
        let sysroot_path = self.custom_rust_std_sysroot_path()?;
        if self.has_custom_rust_std(&sysroot_path) {
            return Ok(sysroot_path);
        }

        let install_root = self.rust_std_install_root()?;
        std::fs::create_dir_all(&install_root).with_context(|| {
            format!(
                "Failed to create rust-std install directory: {}",
                install_root.display()
            )
        })?;

        let tmp_extract_dir = install_root.join(format!(".tmp-{}", std::process::id()));
        if tmp_extract_dir.exists() {
            std::fs::remove_dir_all(&tmp_extract_dir).with_context(|| {
                format!(
                    "Failed to cleanup stale temporary rust-std directory: {}",
                    tmp_extract_dir.display()
                )
            })?;
        }
        std::fs::create_dir_all(&tmp_extract_dir).with_context(|| {
            format!(
                "Failed to create temporary rust-std directory: {}",
                tmp_extract_dir.display()
            )
        })?;

        let archive_path = tmp_extract_dir.join(self.rust_std_archive_name);
        download_file(self.rust_std_url, &archive_path).await?;
        verify_archive_sha256(&archive_path, self.rust_std_archive_sha256)
            .context("Failed to verify SHA-256 for custom rust-std archive.")?;

        extract_tar_xz(&archive_path, &tmp_extract_dir)?;

        let extracted_root = tmp_extract_dir.join(self.rust_std_extracted_root_dir);
        let extracted_sysroot = extracted_root.join(self.rust_std_sysroot_dir);
        if !self.has_custom_rust_std(&extracted_sysroot) {
            return Err(anyhow::anyhow!(
                "Extracted rust-std archive is missing expected target libraries at {}",
                extracted_sysroot.display()
            ));
        }

        let final_root = install_root.join(self.rust_std_extracted_root_dir);
        let final_sysroot = final_root.join(self.rust_std_sysroot_dir);

        // Atomic install attempt: move extracted root into place without deleting
        // the destination first. If destination already exists, prefer the install
        // that won the race if it is valid.
        if let Err(rename_err) = std::fs::rename(&extracted_root, &final_root) {
            if final_root.exists() && self.has_custom_rust_std(&final_sysroot) {
                tracing::debug!(
                    "Detected concurrent rust-std installation at {}, reusing it after rename conflict: {}",
                    final_root.display(),
                    rename_err
                );
            } else {
                return Err(rename_err).with_context(|| {
                    format!(
                        "Failed to move rust-std installation from {} to {}",
                        extracted_root.display(),
                        final_root.display()
                    )
                });
            }
        }

        if let Err(e) = std::fs::remove_dir_all(&tmp_extract_dir) {
            tracing::warn!(
                "Failed to remove temporary rust-std install directory {}: {}",
                tmp_extract_dir.display(),
                e
            );
        }

        self.verify_custom_rust_std_sysroot(&sysroot_path)
            .context("Installed custom rust-std sysroot validation failed.")?;

        Ok(sysroot_path)
    }

    fn rust_std_install_root(self) -> anyhow::Result<PathBuf> {
        let base_dirs = BaseDirs::new().context("Failed to locate home directory for rust-std installation.")?;
        Ok(base_dirs
            .home_dir()
            .join(".lyquor")
            .join("rust-std")
            .join(self.rust_std_release))
    }

    fn custom_rust_std_sysroot_path(self) -> anyhow::Result<PathBuf> {
        Ok(self
            .rust_std_install_root()?
            .join(self.rust_std_extracted_root_dir)
            .join(self.rust_std_sysroot_dir))
    }

    fn target_lib_dir(self, sysroot_path: &Path) -> PathBuf {
        sysroot_path.join("lib").join("rustlib").join(self.target).join("lib")
    }

    fn has_custom_rust_std(self, sysroot_path: &Path) -> bool {
        let target_lib_dir = self.target_lib_dir(sysroot_path);
        let Ok(entries) = std::fs::read_dir(target_lib_dir) else {
            return false;
        };

        let mut found_prefixes = [false; REQUIRED_TARGET_LIB_PREFIXES.len()];
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else { continue };

            for (idx, prefix) in REQUIRED_TARGET_LIB_PREFIXES.iter().enumerate() {
                if !found_prefixes[idx] && file_name.starts_with(prefix) {
                    found_prefixes[idx] = true;
                }
            }

            if found_prefixes.iter().all(|found| *found) {
                return true;
            }
        }

        false
    }
}

async fn download_file(url: &str, path: &Path) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(format!("shaker/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("Failed to initialize HTTP client for rust-std download.")?;
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Failed to download rust-std archive from {url}"))?
        .error_for_status()
        .with_context(|| format!("rust-std download returned an unsuccessful status code: {url}"))?;

    let mut file =
        File::create(path).with_context(|| format!("Failed to create rust-std archive file at {}", path.display()))?;
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("Failed while downloading rust-std archive from {url}"))?
    {
        file.write_all(&chunk)
            .with_context(|| format!("Failed to write rust-std archive to {}", path.display()))?;
    }
    file.sync_all()
        .with_context(|| format!("Failed to flush rust-std archive to {}", path.display()))?;
    Ok(())
}

fn extract_tar_xz(archive_path: &Path, destination: &Path) -> anyhow::Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("Failed to open rust-std archive at {}", archive_path.display()))?;
    let decoder = xz2::read::XzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(destination)
        .with_context(|| format!("Failed to unpack rust-std archive into {}", destination.display()))?;
    Ok(())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("Failed to open file for SHA-256 verification: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];

    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("Failed to read file during SHA-256 verification: {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex::encode(hasher.finalize()))
}

fn verify_archive_sha256(path: &Path, expected_sha256: &str) -> anyhow::Result<()> {
    let expected = expected_sha256
        .strip_prefix("sha256:")
        .unwrap_or(expected_sha256)
        .to_lowercase();
    let found = sha256_file(path)?;
    if found != expected {
        return Err(anyhow::anyhow!(
            "SHA-256 mismatch for archive {}: expected sha256:{}, found sha256:{}",
            path.display(),
            expected,
            found
        ));
    }
    Ok(())
}

fn rustc_version(toolchain: &str) -> anyhow::Result<String> {
    let mut cmd = Command::new("rustc");
    cmd.arg(format!("+{toolchain}")).arg("--version");

    let output = cmd.output().context("Failed to run rustc --version")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "rustc command failed with status: {}, output: {}",
            output.status,
            stderr
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Returns whether `rustc +toolchain --version` matches `expected` exactly.
pub fn check_rustc_version(toolchain: &str, expected: &str) -> anyhow::Result<bool> {
    Ok(rustc_version(toolchain)? == expected)
}

#[cfg(test)]
mod tests {
    use super::ToolchainSpec;
    use lyquor_test::test;

    #[test]
    fn has_custom_rust_std_rejects_incomplete_layout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sysroot = tmp.path().join("sysroot");
        let spec = ToolchainSpec::lyquid_default();

        let target_lib_dir = sysroot.join("lib").join("rustlib").join(spec.target).join("lib");
        std::fs::create_dir_all(&target_lib_dir).expect("create target lib dir");
        std::fs::write(target_lib_dir.join("libcore-abc.rlib"), b"").expect("write core artifact");

        assert!(!spec.has_custom_rust_std(&sysroot));
        assert!(spec.verify_custom_rust_std_sysroot(&sysroot).is_err());
    }

    #[test]
    fn has_custom_rust_std_accepts_required_artifacts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sysroot = tmp.path().join("sysroot");
        let spec = ToolchainSpec::lyquid_default();

        let target_lib_dir = sysroot.join("lib").join("rustlib").join(spec.target).join("lib");
        std::fs::create_dir_all(&target_lib_dir).expect("create target lib dir");
        std::fs::write(target_lib_dir.join("libcore-abc.rlib"), b"").expect("write core artifact");
        std::fs::write(target_lib_dir.join("liballoc-abc.rlib"), b"").expect("write alloc artifact");
        std::fs::write(target_lib_dir.join("libcompiler_builtins-abc.rlib"), b"")
            .expect("write compiler_builtins artifact");

        assert!(spec.has_custom_rust_std(&sysroot));
        spec.verify_custom_rust_std_sysroot(&sysroot)
            .expect("sysroot should be valid");
    }
}
