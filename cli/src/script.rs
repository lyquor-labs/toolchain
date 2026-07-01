use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Emit Cargo build-script metadata for the Lyquor version and git revision.
pub fn lyquor_version_build_script() {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set"));
    emit_git_rerun_hints(&manifest_dir);

    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is not set");
    let git_sha = git_output(&manifest_dir, ["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let git_sha_short =
        git_output(&manifest_dir, ["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let git_dirty = git_is_dirty(&manifest_dir);
    let build_version = if git_sha_short == "unknown" {
        version
    } else if git_dirty {
        format!("{version}+{git_sha_short}.dirty")
    } else {
        format!("{version}+{git_sha_short}")
    };

    println!("cargo:rustc-env=LYQUOR_BUILD_VERSION={build_version}");
    println!("cargo:rustc-env=LYQUOR_GIT_COMMIT_SHA={git_sha}");
    println!("cargo:rustc-env=LYQUOR_GIT_COMMIT_SHA_SHORT={git_sha_short}");
    println!("cargo:rustc-env=LYQUOR_GIT_DIRTY={}", if git_dirty { "1" } else { "0" });
}

fn emit_git_rerun_hints(manifest_dir: &Path) {
    let Some(repo_root) = git_path(manifest_dir, ["rev-parse", "--show-toplevel"]) else {
        return;
    };
    emit_tracked_file_rerun_hints(&repo_root);

    let Some(git_dir) = git_path(manifest_dir, ["rev-parse", "--git-dir"]) else {
        return;
    };
    emit_rerun_if_exists(&git_dir.join("HEAD"));
    emit_rerun_if_exists(&git_dir.join("index"));

    let Some(common_dir) = git_path(manifest_dir, ["rev-parse", "--git-common-dir"]) else {
        return;
    };
    emit_rerun_if_exists(&common_dir.join("packed-refs"));

    if let Some(head_ref) = git_output(manifest_dir, ["symbolic-ref", "-q", "HEAD"]) {
        emit_rerun_if_exists(&common_dir.join(head_ref));
    }
}

fn emit_tracked_file_rerun_hints(repo_root: &Path) {
    let Some(paths) = git_outputs(repo_root, ["ls-files", "-z"], b'\0') else {
        return;
    };

    for path in paths {
        emit_rerun_if_exists(&repo_root.join(path));
    }
}

fn emit_rerun_if_exists(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn git_path<const N: usize>(manifest_dir: &Path, args: [&str; N]) -> Option<PathBuf> {
    let path = git_output(manifest_dir, args)?;
    let path = PathBuf::from(path);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(manifest_dir.join(path))
    }
}

fn git_output<const N: usize>(manifest_dir: &Path, args: [&str; N]) -> Option<String> {
    let value = git_stdout(manifest_dir, args)?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    Some(value.to_string())
}

fn git_outputs<const N: usize>(manifest_dir: &Path, args: [&str; N], delimiter: u8) -> Option<Vec<String>> {
    let stdout = git_stdout(manifest_dir, args)?;
    Some(
        stdout
            .split(char::from(delimiter))
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
    )
}

fn git_is_dirty(manifest_dir: &Path) -> bool {
    git_output(manifest_dir, ["status", "--porcelain", "--untracked-files=no"]).is_some()
}

fn git_stdout<const N: usize>(manifest_dir: &Path, args: [&str; N]) -> Option<String> {
    let output = Command::new("git").current_dir(manifest_dir).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout).ok()
}
