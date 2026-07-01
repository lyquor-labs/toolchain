use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as Write_;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, bail};
use lyquor_oci::pack::{LyquidPack, LyquidPackMetadata};
use lyquor_primitives::{Bytes, StateCategory};
use lyquor_wasm::{EthExportGuard, LyquidFunc};

use crate::toolchain::ToolchainSpec;

const CONTRACT_NAME: &str = "SequenceBackend";

/// Options for compiling a Lyquid crate into a deployable pack.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub manifest: PathBuf,
    pub target_dir: PathBuf,
    pub debug: bool,
    pub is_bartender: bool,
}

#[allow(dead_code)]
fn add_indentation(lines: &str, nspaces: usize) -> String {
    let mut output = String::new();
    for line in lines.lines() {
        for _ in 0..nspaces {
            output.push(' ');
        }
        output.push_str(line);
        output.push('\n');
    }
    output
}

struct CallCodeGen<'a> {
    method: &'a str,
    group: &'a str,
    caller: Option<&'a str>,
    input: Option<&'a str>,
    image_hash: Option<&'a str>,
}

impl<'a> CallCodeGen<'a> {
    fn generate(&self) -> String {
        let caller = self.caller.unwrap_or("msg.sender");
        let method = self.method;
        let group = self.group;
        let input = self.input.unwrap_or("msg.data");
        let image_hash = self.image_hash.unwrap_or("hex\"\"");
        format!(
            "
       CallParams[] memory calls = new CallParams[](1);
       calls[0] = CallParams({{
           origin: tx.origin,
           caller: {caller},
           method: \"{method}\",
           group: \"{group}\",
           input: {input},
           abi_: ABI.Eth
       }});
       emit Slot (
           next_slot++,
           calls,
           {image_hash},
           address(0)
       );",
        )
    }
}

#[derive(Debug)]
struct GeneratedSolidityMethods {
    methods: String,
    constructor_params: Option<GeneratedConstructorParams>,
}

#[derive(Debug, Default)]
struct GeneratedConstructorParams {
    declaration_suffix: String,
    call_args: String,
}

fn reject_unsupported_creator_guard(guard: EthExportGuard, group: &str, method: &str) -> anyhow::Result<()> {
    if matches!(guard, EthExportGuard::Creator) {
        bail!(
            "Ethereum export `{group}::{method}` uses `eth_guard = creator`, but generated Solidity wrappers only support mutable network methods in the `main` or `node` group."
        );
    }
    Ok(())
}

fn generate_solidity_methods(funcs: impl IntoIterator<Item = LyquidFunc>) -> anyhow::Result<GeneratedSolidityMethods> {
    let mut methods = String::new();
    let mut constructor_params = None;

    for func in funcs {
        if let Some(eth) = func.eth {
            let group_decoded = match lyquor_primitives::cb58::bs58::decode(func.group_hash.as_str()).into_vec() {
                Ok(bytes) => String::from_utf8(bytes).unwrap_or_else(|_| "main".to_string()),
                Err(_) => "main".to_string(),
            };

            let method = func.method;
            let params = eth.params;
            let returns = eth.returns;
            let guard = eth.guard;
            match func.category {
                StateCategory::Network => {
                    if !func.mutable {
                        reject_unsupported_creator_guard(guard, &group_decoded, &method)?;
                        continue;
                    }
                    if method == "__lyquid_constructor" {
                        if matches!(guard, EthExportGuard::Creator) {
                            bail!(
                                "Ethereum export `{group_decoded}::{method}` uses `eth_guard = creator`, but constructors do not expose public transaction wrappers."
                            );
                        }
                        constructor_params = Some(if params.is_empty() {
                            GeneratedConstructorParams::default()
                        } else {
                            let params = params
                                .split(',')
                                .enumerate()
                                .map(|(i, decl)| format!("{} _{i}", decl.trim()))
                                .collect::<Vec<_>>();
                            GeneratedConstructorParams {
                                declaration_suffix: format!(", {}", params.join(", ")),
                                call_args: (0..params.len())
                                    .map(|i| format!("_{i}"))
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            }
                        });
                        let selector = lyquor_primitives::hex::encode(eth.selector);
                        let body = CallCodeGen {
                            method: &method,
                            group: "main",
                            caller: None,
                            input: Some(&format!("abi.encodePacked(bytes4(0x{selector}), params)")),
                            image_hash: Some("image_hash"),
                        }
                        .generate();
                        write!(
                            methods,
                            "    function {method}(bytes32 image_hash, bytes memory params) private {{{body}\n    }}\n\n"
                        )?;
                    } else if group_decoded == "main" || group_decoded == "node" {
                        let guard_code = match guard {
                            EthExportGuard::None => "",
                            EthExportGuard::Creator => {
                                "        require(msg.sender == creator, \"Only creator can call this method.\");\n"
                            }
                        };
                        let body = CallCodeGen {
                            method: &method,
                            group: &group_decoded,
                            caller: None,
                            input: None,
                            image_hash: None,
                        }
                        .generate();
                        write!(
                            methods,
                            "    function {method}({params}) public {} {{{guard_code}{body}\n    }}\n\n",
                            if !returns.is_empty() {
                                format!("returns ({returns})")
                            } else {
                                String::new()
                            }
                        )?;
                    } else {
                        reject_unsupported_creator_guard(guard, &group_decoded, &method)?;
                    }
                }
                StateCategory::Instance => {
                    reject_unsupported_creator_guard(guard, &group_decoded, &method)?;
                    if group_decoded == "main" {
                        writeln!(
                            methods,
                            "    function {method}({params}) public view {} {{}} /* instance func hosted by Lyquor */",
                            if !returns.is_empty() {
                                format!("returns ({returns})")
                            } else {
                                String::new()
                            }
                        )?;
                    }
                }
            }
        }
    }

    Ok(GeneratedSolidityMethods {
        methods,
        constructor_params,
    })
}

/// Generates Solidity sequencer contract source for a Lyquid WASM module.
pub async fn generate_solidity_sequencer(bin: Bytes, is_bartender: bool) -> anyhow::Result<String> {
    let funcs = lyquor_wasm::extract_lyquid_functions_from_wasm(bin.as_ref())
        .context("Failed to extract the lyquid functions from the binary.")?;
    let GeneratedSolidityMethods {
        mut methods,
        constructor_params,
    } = generate_solidity_methods(funcs)?;

    if constructor_params.is_none() {
        write!(
            methods,
            "    function __lyquid_constructor(bytes32 image_hash, bytes memory) private {{
        emit Slot(next_slot++, new CallParams[](0), image_hash, address(0));\n    }}\n\n",
        )?;
    }
    let constructor_params = constructor_params.unwrap_or_default();
    let constructor_params_declaration_suffix = constructor_params.declaration_suffix;
    let constructor_params_call_args = constructor_params.call_args;

    let name = CONTRACT_NAME;
    let body = lyquor_eth::extract_code_sections("sequencer-body");
    let preamble = lyquor_eth::extract_code_sections("sequencer-preamble");
    let (bartender_preamble, bartender_body) = if is_bartender {
        (
            lyquor_eth::extract_code_sections("sequencer-bartender-preamble"),
            lyquor_eth::extract_code_sections("sequencer-bartender-body"),
        )
    } else {
        (String::new(), String::new())
    };

    let set_node_address = match is_bartender {
        false => String::new(),
        true => "function setEd25519Address(address addr, bytes32 pubkey, uint256 qx, uint256 qy, uint256 edR, uint256 edS, bytes calldata ecSig) external returns (bool) {
        bool ok;
        uint256[2] memory q = [qx, qy];
        uint256[2] memory edSig = [edR, edS];
        (ok, next_slot) = Crypto.setEd25519Address(ed25519Library, ed25519ToAddress, next_slot, addr, pubkey, q, edSig, ecSig);
        return ok;
    }
    
    function getEd25519Address(bytes32 nodeID) external view returns (address) {
        return ed25519ToAddress[nodeID];
    }

    function verifyEd25519Signature(string calldata m, uint256 r, uint256 s, bytes32 pubkey, uint256 qx, uint256 qy) external view returns (bool) {
        return Crypto.verifyEd25519Signature(ed25519Library, m, r, s, pubkey, qx, qy);
    }"
        .to_string(),
    };

    let special_constructor_params = match is_bartender {
        false => "",
        true => ", address _oracleLibrary, address _ed25519Library",
    };

    let constructor_library_setup = match is_bartender {
        false => "\
        bartender = IBartender(_bartender);
        oracleLibrary = bartender.getOracleLibrary();
        require(oracleLibrary != address(0), \"Missing oracle library.\");
        require(oracleLibrary.code.length != 0, \"Oracle library is not deployed.\");"
            .to_string(),
        true => "\
        oracleLibrary = _oracleLibrary;
        ed25519Library = _ed25519Library;
        require(oracleLibrary != address(0), \"Missing oracle library.\");
        require(ed25519Library != address(0), \"Missing ed25519 library.\");
        require(oracleLibrary.code.length != 0, \"Oracle library is not deployed.\");
        require(ed25519Library.code.length != 0, \"Ed25519 library is not deployed.\");"
            .to_string(),
    };

    let constructor_register = match is_bartender {
        false => "        bartender.register(superseded, deps, image_hash, repo_hint);".to_string(),
        true => {
            let body = CallCodeGen {
                method: "register",
                group: "main",
                caller: Some("address(this)"),
                input: Some(
                    "abi.encodeWithSelector(bytes4(keccak256(\"register(address,address[],bytes32,string)\")), address(0), deps, image_hash, repo_hint)",
                ),
                image_hash: None,
            }
            .generate();
            format!(
                "\
        // NOTE: the special treatment for a bartender contract is to use the address of the
        // contract itself as the caller, so bartender can register itself correctly
{body}"
            )
        }
    };

    let constructor_sequence = format!(
        "\
{constructor_library_setup}
        // Emit Lyquid constructor event.
        __lyquid_constructor(image_hash, params); // skip `superseded`
{constructor_register}"
    );

    let sol = format!(
        "\
// DO NOT EDIT THIS FILE !!!
// This file is automatically generated from a Lyquid implementation for its Ethereum-compatible ABI and sequencing.
// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

{preamble}
{bartender_preamble}

contract {name} is ISequenceBackend {{
{body}
{bartender_body}

    constructor(address _bartender, address superseded, bytes32 image_hash, string memory repo_hint{special_constructor_params}, address[] memory deps{constructor_params_declaration_suffix}) {{
        bytes memory params = abi.encode({constructor_params_call_args});
        // Stop the superseded contract first.
        if (superseded != address(0)) {{
            ISequenceBackend s = ISequenceBackend(superseded);
            next_slot = s.__lyquor_switch_contract(address(this));
        }}
        // Record the creator's address once and for all.
        creator = tx.origin;
        {constructor_sequence}
    }}

    {set_node_address}

{methods}}}"
    );
    Ok(sol)
}

/// Reads a WASM module from disk and generates its Solidity sequencer contract source.
pub async fn generate_solidity_sequencer_from_file<P: AsRef<Path>>(
    path: P, is_bartender: bool,
) -> anyhow::Result<String> {
    let mut bin = Vec::new();
    File::open(path)
        .and_then(|mut f| f.read_to_end(&mut bin))
        .context("Failed to read the file.")?;
    generate_solidity_sequencer(bin.into(), is_bartender)
        .await
        .context("Failed to generate solidity sequencer contract code.")
}

/// Returns the current working directory with build-context error reporting.
pub(crate) fn get_cwd() -> anyhow::Result<PathBuf> {
    std::env::current_dir().context("Failed to retrieve the currrent directory.")
}

/// Compiles generated Solidity and returns one artifact's deployment bytecode.
pub fn build_solidity_bytecode<P: AsRef<Path>>(
    sol_out: P, source_name: &str, artifact_name: &str,
) -> anyhow::Result<Bytes> {
    use foundry_compilers::artifacts::EvmVersion;
    use foundry_compilers::multi::MultiCompilerSettings;
    let mut settings = MultiCompilerSettings::default();
    settings.vyper.evm_version = Some(EvmVersion::Paris);
    settings.solc.settings.evm_version = Some(EvmVersion::Paris);
    settings.solc.settings.optimizer.enabled = Some(true);
    settings.solc.settings.optimizer.runs = Some(200);

    use foundry_compilers::Project;
    let proj = Project::builder()
        .paths(
            foundry_compilers::ProjectPathsConfig::builder()
                .root(sol_out.as_ref())
                .sources(sol_out.as_ref())
                .build()?,
        )
        .settings(settings)
        .build(Default::default())?;
    let sol_path = sol_out.as_ref().join(format!("{source_name}.sol"));
    let mut output = std::thread::spawn(move || proj.compile_file(&sol_path))
        .join()
        .map_err(|_| anyhow::anyhow!("foundry_compilers thread panicked."))
        .and_then(|e| e.map_err(anyhow::Error::from))
        .context("Ethereum compilation error.")?;

    let sol = output
        .remove_first(artifact_name)
        .and_then(|f| f.bytecode)
        .and_then(foundry_compilers::artifacts::CompactBytecode::into_bytes)
        .ok_or_else(|| {
            let errors = output
                .output()
                .errors
                .iter()
                .filter(|e| {
                    if let foundry_compilers::multi::MultiCompilerError::Solc(e) = e {
                        e.is_error()
                    } else {
                        true
                    }
                })
                .collect::<Vec<_>>();

            anyhow::anyhow!(
                "Failed to compile {artifact}: {:?}",
                errors,
                artifact = format_args!("{artifact_name}.sol")
            )
        })?;
    Ok(sol.into())
}

#[doc(hidden)]
async fn build_lyquid_files(
    options: &BuildOptions,
) -> anyhow::Result<(PathBuf, PathBuf, Option<BTreeMap<String, PathBuf>>, String, PathBuf)> {
    let profile = if options.debug { "debug" } else { "release" };
    let cwd = get_cwd()?;
    let target_path = cwd.join(&options.target_dir);
    let manifest =
        std::fs::canonicalize(cwd.join(&options.manifest)).context("Failed to access the lyquid project path.")?;

    let toolchain_spec =
        ToolchainSpec::from_env().context("Failed to resolve the Lyquid toolchain spec from environment.")?;
    let lyquid_toolchain = toolchain_spec
        .resolve_toolchain()
        .context("Failed to find the Lyquid toolchain.")?;
    let custom_sysroot = toolchain_spec
        .ensure_custom_rust_std_sysroot()
        .await
        .context("Failed to prepare the custom rust-std sysroot.")?;
    toolchain_spec
        .verify_custom_rust_std_sysroot(&custom_sysroot)
        .context("Custom rust-std preflight check failed before invoking cargo.")?;

    let metadata = cargo_metadata::MetadataCommand::new()
        .manifest_path(&manifest)
        .exec()
        .context("Failed to read cargo metadata.")?;

    let mut cmd = Command::new("cargo");
    cmd.env_remove("RUSTDOC");
    for (key, _value) in std::env::vars() {
        if (key.starts_with("RUSTC") || key.starts_with("CARGO") || key.starts_with("RUSTUP")) &&
            !key.ends_with("_HOME")
        {
            cmd.env_remove(key);
        }
    }

    for (key, value) in std::env::vars() {
        if key.starts_with("LYQUID_") {
            let key = key.strip_prefix("LYQUID_").unwrap();
            if !toolchain_spec.should_forward_lyquid_env(key) {
                continue;
            }
            cmd.env(key, value);
        }
    }

    if !options.debug &&
        toolchain_spec.requires_release_opt_level_workaround() &&
        std::env::var_os("LYQUID_CARGO_PROFILE_RELEASE_OPT_LEVEL").is_none()
    {
        cmd.env("CARGO_PROFILE_RELEASE_OPT_LEVEL", "1");
    }

    let encoded_rustflags = toolchain_spec.encoded_rustflags(&custom_sysroot);

    cmd.current_dir(&metadata.workspace_root)
        .arg(format!("+{lyquid_toolchain}"))
        .arg("build")
        .arg(format!("--target={}", toolchain_spec.target))
        .env_remove("RUSTFLAGS")
        .env("CARGO_ENCODED_RUSTFLAGS", encoded_rustflags)
        .env("CARGO_TARGET_DIR", &target_path);

    if !options.debug {
        cmd.arg("--release");
    }

    // Keep stdout clean for machine-readable CLI outputs by routing cargo stdout to stderr.
    cmd.stdout(Stdio::from(std::io::stderr())).stderr(Stdio::inherit());

    tracing::debug!("Running cargo: {:?}", cmd);
    let status = cmd.status().context("Failed to execute cargo build.")?;
    if !status.success() {
        return Err(anyhow::anyhow!("Cargo failed to build."));
    }

    let name = metadata
        .root_package()
        .ok_or_else(|| anyhow::anyhow!("No root package found."))?
        .name
        .to_string();

    let crate_name = name.replace('-', "_");
    let wasm_out = target_path.join(format!("{}/{}/{crate_name}.wasm", toolchain_spec.target, profile));
    let sol_out = target_path.join("solidity").join(&name);
    if let Err(e) = std::fs::create_dir_all(&sol_out) {
        match e.kind() {
            std::io::ErrorKind::AlreadyExists => (),
            _ => return Err(anyhow::anyhow!("Failed to create solidity output directory: {e}")),
        }
    }

    tracing::debug!("Generating sequencer...");
    let contract = generate_solidity_sequencer_from_file(&wasm_out, options.is_bartender).await?;
    File::create(sol_out.join(format!("{name}.sol")))
        .and_then(|mut f| f.write_all(contract.as_bytes()))
        .with_context(|| format!("Failed to create file for {name}.sol"))?;

    if let Err(e) = lyquor_eth::write_library_files(&sol_out) {
        tracing::warn!("Failed to write sequencer lib: {}", e);
    }

    tracing::debug!("Building solidity bytecode...");
    let evm_deployment_bytecode = build_solidity_bytecode(&sol_out, &name, CONTRACT_NAME)?;
    File::create(sol_out.join(format!("{name}.bin")))
        .and_then(|mut f| f.write_all(&evm_deployment_bytecode))
        .with_context(|| format!("Failed to create file for {name}.bin"))?;

    // Bartender fix (TODO: use manifest after we added)
    let sol_aux_out = if options.is_bartender {
        let oracle_bytecode = crate::build::build_solidity_bytecode(sol_out.as_path(), "lib/oracle", "Oracle")?;
        File::create(sol_out.join(format!("{}.bin", "dep_oracle")))
            .and_then(|mut f| f.write_all(&oracle_bytecode))
            .context("Failed to create file for dep_oracle.bin")?;

        let ed25519_bytecode = crate::build::build_solidity_bytecode(sol_out.as_path(), "lib/ed25519", "SCL_EIP6565")?;
        File::create(sol_out.join(format!("{}.bin", "dep_SCL_EIP6565")))
            .and_then(|mut f| f.write_all(&ed25519_bytecode))
            .context("Failed to create file for dep_SCL_EIP6565.bin")?;

        let mut aux = BTreeMap::new();
        aux.insert("oracle".to_owned(), PathBuf::from("dep_oracle.bin"));
        aux.insert("SCL_EIP6565".to_owned(), PathBuf::from("dep_SCL_EIP6565.bin"));

        Some(aux)
    } else {
        None
    };

    Ok((
        wasm_out,
        sol_out,
        sol_aux_out,
        name,
        manifest.parent().unwrap_or_else(|| Path::new(".")).to_path_buf(),
    ))
}

fn collect_assets(project_dir: &Path) -> anyhow::Result<Option<BTreeMap<String, Bytes>>> {
    let root = project_dir.join("assets");
    match std::fs::symlink_metadata(&root) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Failed to stat Lyquid assets path {}.", root.display())),
    }
    if !root.is_dir() {
        anyhow::bail!("Lyquid assets path {} is not a directory.", root.display());
    }

    let mut assets = BTreeMap::<String, Bytes>::new();
    #[allow(
        clippy::redundant_clone,
        reason = "owned seed for the stack; `root` is borrowed below by `strip_prefix`"
    )]
    let mut stack = vec![(root.to_path_buf(), BTreeSet::new())];

    while let Some((dir, mut ancestors)) = stack.pop() {
        let canonical_dir = std::fs::canonicalize(&dir)
            .with_context(|| format!("Failed to canonicalize Lyquid assets directory {}.", dir.display()))?;
        if !ancestors.insert(canonical_dir) {
            anyhow::bail!("Lyquid assets directory cycle detected at {}.", dir.display());
        }

        let mut entries = std::fs::read_dir(&dir)
            .with_context(|| format!("Failed to read Lyquid assets directory {}.", dir.display()))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("Failed to scan Lyquid assets directory {}.", dir.display()))?;
        entries.sort_by_key(std::fs::DirEntry::path);

        for entry in entries {
            let path = entry.path();
            let metadata =
                std::fs::metadata(&path).with_context(|| format!("Failed to stat Lyquid asset {}.", path.display()))?;
            if metadata.is_dir() {
                stack.push((path, ancestors.clone()));
                continue;
            }
            if !metadata.is_file() {
                anyhow::bail!(
                    "Unsupported Lyquid asset {}. Only regular files and directories are supported.",
                    path.display()
                );
            }

            let relative = path
                .strip_prefix(&root)
                .with_context(|| format!("Failed to relativize Lyquid asset {}.", path.display()))?;
            let asset_name = relative
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Lyquid asset path {} is not valid UTF-8.", path.display()))?
                .replace(std::path::MAIN_SEPARATOR, "/");
            let data =
                std::fs::read(&path).with_context(|| format!("Failed to read Lyquid asset {}.", path.display()))?;
            assets.insert(asset_name, data.into());
        }
    }

    Ok((!assets.is_empty()).then_some(assets))
}

/// Builds a Lyquid crate, generated Solidity artifacts, assets, and metadata into a pack.
pub async fn build_lyquid(options: &BuildOptions) -> anyhow::Result<LyquidPack> {
    let (wasm_out, sol_out, sol_aux, name, project_dir) = build_lyquid_files(options).await?;
    let wasm = std::fs::read(wasm_out).context("Failed to read the WASM binary.")?;
    let evm_deployment_bytecode =
        std::fs::read(sol_out.join(format!("{name}.bin"))).context("Failed to read the EVM deployment bytecode.")?;

    let evm_auxiliary_bytecodes = if let Some(aux) = sol_aux {
        let mut aux_set = BTreeMap::<String, Bytes>::new();
        for (name, path) in &aux {
            let bytecode = std::fs::read(sol_out.join(path)).context("Failed to read the EVM deployment bytecode.")?;
            aux_set.insert(name.to_owned(), bytecode.into());
        }
        Some(aux_set)
    } else {
        None
    };
    let assets = collect_assets(&project_dir)?;
    // The LDK this toolchain (shaker) was built against. The lyquid we just compiled may pull a
    // different `lyquid` crate; if the two are SemVer-incompatible, shaker's WASM processing and
    // the node may misinterpret the image, so warn the user to align them.
    let toolchain_ldk = lyquid::consts::LDK_VERSION;
    let ldk_version = match lyquor_wasm::read_ldk_descriptor(&wasm) {
        lyquor_wasm::LdkDescriptor::Version(version) => {
            tracing::info!("Built {name} with LDK {version}");
            match (semver::Version::parse(&version), semver::Version::parse(toolchain_ldk)) {
                (Ok(built), Ok(toolchain)) if !lyquor_wasm::ldk_versions_compatible(&built, &toolchain) => {
                    tracing::warn!(
                        "{name} was built with LDK {version}, but shaker was built with LDK {toolchain_ldk}; \
                         update shaker or the lyquid's `lyquid` dependency to compatible versions to avoid \
                         undefined behavior."
                    );
                }
                _ => {}
            }
            Some(version)
        }
        lyquor_wasm::LdkDescriptor::Unrecognized => {
            tracing::warn!(
                "{name} carries an LDK descriptor that shaker (LDK {toolchain_ldk}) cannot decode; \
                 update shaker to a version matching the lyquid's `lyquid` dependency to avoid undefined behavior."
            );
            None
        }
        lyquor_wasm::LdkDescriptor::Absent => {
            tracing::warn!("Built {name} without a recognized LDK version descriptor");
            None
        }
    };
    // The Lyquor platform is the image's "OS"; record the compatibility line (major.minor) the
    // image targets. Patch releases are compatible by definition, so they don't belong here —
    // the full build version stays in the WASM LDK descriptor.
    let os_version = ldk_version
        .and_then(|version| semver::Version::parse(&version).ok())
        .map(|version| format!("{}.{}", version.major, version.minor));
    let eth_abi = lyquor_wasm::ethereum_json_abi_from_wasm(&wasm)
        .context("Failed to extract the Ethereum JSON ABI from the binary.")?;
    let eth_abi = if eth_abi.is_empty() { None } else { Some(eth_abi) };
    let metadata = LyquidPackMetadata::new(&name, None, None, None, os_version.as_deref());
    Ok(LyquidPack::try_build_with_binary(
        wasm.into(),
        evm_deployment_bytecode.into(),
        evm_auxiliary_bytecodes,
        assets,
        eth_abi,
        metadata,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lyquor_test::test;

    #[cfg(unix)] use std::os::unix::fs::symlink;

    #[test]
    fn generate_solidity_methods_applies_creator_guard() {
        let main_group_hash = lyquor_primitives::cb58::bs58::encode(b"main").into_string();
        let generated = generate_solidity_methods([
            LyquidFunc {
                category: StateCategory::Network,
                mutable: true,
                group_hash: main_group_hash.clone(),
                method: "setup".to_owned(),
                eth: Some(lyquor_wasm::EthExportInfo {
                    selector: [0, 0, 0, 0],
                    guard: EthExportGuard::Creator,
                    params: String::new(),
                    returns: String::new(),
                    params_canonical_types: Vec::new(),
                    returns_canonical_types: Vec::new(),
                }),
                http: None,
            },
            LyquidFunc {
                category: StateCategory::Network,
                mutable: true,
                group_hash: main_group_hash,
                method: "open".to_owned(),
                eth: Some(lyquor_wasm::EthExportInfo {
                    selector: [0, 0, 0, 0],
                    guard: EthExportGuard::None,
                    params: String::new(),
                    returns: String::new(),
                    params_canonical_types: Vec::new(),
                    returns_canonical_types: Vec::new(),
                }),
                http: None,
            },
        ])
        .expect("method generation");

        let guard = "require(msg.sender == creator, \"Only creator can call this method.\");";
        let setup_pos = generated.methods.find("function setup").expect("setup wrapper");
        let guard_pos = generated.methods.find(guard).expect("creator guard");
        let setup_slot_pos = generated.methods[setup_pos..]
            .find("emit Slot")
            .map(|pos| setup_pos + pos)
            .expect("setup slot emission");
        let open_pos = generated.methods.find("function open").expect("open wrapper");
        assert!(setup_pos < guard_pos && guard_pos < setup_slot_pos);
        assert!(setup_slot_pos < open_pos);
        assert!(generated.methods.contains("method: \"setup\""));
        assert!(generated.methods.contains("method: \"open\""));
        assert!(!generated.methods[open_pos..].contains("Only creator can call this method"));
    }

    #[test]
    fn generate_solidity_methods_rejects_creator_guard_without_transaction_wrapper() {
        let err = generate_solidity_methods([LyquidFunc {
            category: StateCategory::Instance,
            mutable: false,
            group_hash: lyquor_primitives::cb58::bs58::encode(b"main").into_string(),
            method: "lookup".to_owned(),
            eth: Some(lyquor_wasm::EthExportInfo {
                selector: [0, 0, 0, 0],
                guard: EthExportGuard::Creator,
                params: String::new(),
                returns: String::new(),
                params_canonical_types: Vec::new(),
                returns_canonical_types: Vec::new(),
            }),
            http: None,
        }])
        .expect_err("creator guard on an instance export should fail");

        assert!(err.to_string().contains("only support mutable network methods"));
    }

    #[test]
    fn collect_assets_reads_assets_subdirectory_as_is() {
        let temp = tempfile::tempdir().expect("temp dir");
        let assets_dir = temp.path().join("assets");
        std::fs::create_dir_all(assets_dir.join("src")).expect("src dir");
        std::fs::create_dir_all(assets_dir.join("public")).expect("public dir");
        std::fs::create_dir_all(assets_dir.join(".well-known")).expect("well-known dir");
        std::fs::write(assets_dir.join("index.html"), "<html></html>").expect("index");
        std::fs::write(assets_dir.join("src/app.js"), "console.log('ok');").expect("app");
        std::fs::write(assets_dir.join("public/config.local.json"), "{}").expect("public config");
        std::fs::write(assets_dir.join(".well-known/assetlinks.json"), "[]").expect("well-known asset");

        let assets = collect_assets(temp.path()).expect("collect").expect("assets");

        assert_eq!(assets.get("index.html").unwrap().as_ref(), b"<html></html>");
        assert_eq!(assets.get("src/app.js").unwrap().as_ref(), b"console.log('ok');");
        assert_eq!(assets.get("public/config.local.json").unwrap().as_ref(), b"{}");
        assert_eq!(assets.get(".well-known/assetlinks.json").unwrap().as_ref(), b"[]");
        assert!(!assets.contains_key("config.local.json"));
    }

    #[cfg(unix)]
    #[test]
    fn collect_assets_follows_symlinks_and_rejects_cycles() {
        let temp = tempfile::tempdir().expect("temp dir");
        let assets_dir = temp.path().join("assets");
        let shared_dir = temp.path().join("shared");
        std::fs::create_dir_all(&assets_dir).expect("assets dir");
        std::fs::create_dir_all(shared_dir.join("nested")).expect("shared dir");
        std::fs::write(temp.path().join("linked.txt"), "linked").expect("linked file");
        std::fs::write(shared_dir.join("nested/app.js"), "console.log('linked');").expect("shared asset");
        symlink(temp.path().join("linked.txt"), assets_dir.join("linked.txt")).expect("file symlink");
        symlink(&shared_dir, assets_dir.join("shared")).expect("dir symlink");

        let assets = collect_assets(temp.path()).expect("collect").expect("assets");

        assert_eq!(assets.get("linked.txt").unwrap().as_ref(), b"linked");
        assert_eq!(
            assets.get("shared/nested/app.js").unwrap().as_ref(),
            b"console.log('linked');"
        );
        symlink(".", assets_dir.join("loop")).expect("loop symlink");

        let err = collect_assets(temp.path()).expect_err("cycle must be rejected");

        assert!(err.to_string().contains("directory cycle"));
    }
}
