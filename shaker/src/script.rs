use anyhow::Context;

/// Emits shared Cargo build-script invalidation directives for Lyquid-related crates.
pub fn emit_common_rerun_if_changed() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../eth/Cargo.toml");
    println!("cargo:rerun-if-changed=../eth/src");
    println!("cargo:rerun-if-changed=../lyquid/Cargo.toml");
    println!("cargo:rerun-if-changed=../lyquid/src");
    println!("cargo:rerun-if-changed=../lyquid/proc/Cargo.toml");
    println!("cargo:rerun-if-changed=../lyquid/proc/src");
}

/// Returns true when the build script should skip heavy work for rust-analyzer.
#[allow(unexpected_cfgs)]
pub fn skip_if_rust_analyzer() -> bool {
    // Skip heavy build operations when run by rust-analyzer for faster IDE experience
    if cfg!(rust_analyzer) {
        println!("cargo:warning=Build script skipped: running under rust-analyzer");
        return true;
    }

    false
}

/// Emits Cargo build-script invalidation directives for one nested Lyquid crate.
pub fn emit_lyquid_rerun_if_changed(name: &str) {
    println!("cargo:rerun-if-changed={name}");
    println!("cargo:rerun-if-changed={name}/Cargo.toml");
    println!("cargo:rerun-if-changed={name}/src");
    println!("cargo:rerun-if-changed={name}/assets");
}

/// Builds a nested Lyquid crate from a build script and writes the pack into `OUT_DIR`.
pub async fn build_lyquid_from_build_script(
    manifest_dir: &str, name: &str, debug: bool, is_bartender: bool,
) -> anyhow::Result<()> {
    emit_lyquid_rerun_if_changed(name);

    let manifest = std::path::Path::new(manifest_dir).join(name).join("Cargo.toml");
    let workspace_target = resolve_workspace_target_dir()?;
    let nested_target = workspace_target.join("lyquid_tools_target");

    let options = crate::BuildOptions {
        manifest,
        target_dir: nested_target,
        debug,
        is_bartender,
    };
    let lyquid = crate::build_lyquid(&options).await?;

    let pack_dst = std::path::Path::new(&std::env::var("OUT_DIR").unwrap())
        .join(if debug { "debug" } else { "release" })
        .join(&lyquid.metadata().name)
        .join("lyquid.pack");
    std::fs::create_dir_all(pack_dst.parent().unwrap()).context("Failed to create output directory for Lyquid pack")?;
    std::fs::write(
        &pack_dst,
        lyquid.to_repo_bytes().context("Failed to encode Lyquid pack")?,
    )
    .context(format!("Failed to write Lyquid pack to {}", pack_dst.display()))?;

    Ok(())
}

fn resolve_workspace_target_dir() -> anyhow::Result<std::path::PathBuf> {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").context("OUT_DIR is not set for build script")?);
    // Cargo does not expose workspace target dir directly to build scripts.
    // This depends on Cargo's current internal OUT_DIR layout, but remains the
    // most reliable approach we have for now.
    out_dir
        .ancestors()
        .nth(4)
        .map(std::path::Path::to_path_buf)
        .context("failed to derive workspace target directory from OUT_DIR")
}
