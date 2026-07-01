//! WASM metadata extraction and preprocessing for Lyquid artifacts.
//!
//! This crate is the normalization step between a compiled Lyquid module and the VM/artifact
//! pipeline. It reads custom sections that describe Lyquid methods and Ethereum exports, derives
//! Ethereum JSON ABI metadata, and rewrites memory imports plus atomic wait/notify instructions
//! into the host ABI shape expected by `lyquor-vm`.

use std::collections::HashMap;

use alloy_json_abi::AbiItem;
pub use alloy_json_abi::{Constructor, Function, JsonAbi, Param, StateMutability};
use lyquor_primitives::{Deserialize, GROUP_DEFAULT, Serialize, StateCategory, alloy_primitives};
use wasm_encoder::reencode::{self, Reencode};
use wasm_encoder::{CodeSection, EntityType, ImportSection, Instruction, Module, TypeSection, ValType};
use wasmparser::{Import, Imports, Operator, Parser, Payload, TypeRef};

/// Guard policy applied to generated EVM wrapper methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EthExportGuard {
    /// No EVM wrapper guard policy.
    None,
    /// Generated EVM wrapper transactions require `msg.sender` to be the deployment creator.
    Creator,
}

/// Ethereum ABI metadata exported for one Lyquid method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EthExportInfo {
    /// Four-byte Ethereum selector.
    pub selector: [u8; 4],
    /// Guard applied by the generated EVM wrapper surface.
    pub guard: EthExportGuard,
    /// Human-readable parameter tuple.
    pub params: String,
    /// Human-readable return tuple.
    pub returns: String,
    /// Canonical parameter types used for JSON ABI output.
    pub params_canonical_types: Vec<String>,
    /// Canonical return types used for JSON ABI output.
    pub returns_canonical_types: Vec<String>,
}

/// HTTP ingress metadata exported for one Lyquid instance method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpExportInfo {
    /// HTTP method to match, or `*` for any method.
    pub method: String,
    /// Canonical segment-aware path prefix to match.
    pub path_prefix: String,
}

/// Discovered Lyquid method metadata extracted from a WASM module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LyquidFunc {
    /// State category that owns the method.
    pub category: StateCategory,
    /// Whether the method mutates its state category.
    pub mutable: bool,
    /// Base58 encoding of the method group name, as used in exported function names.
    pub group_hash: String,
    /// Method name.
    pub method: String,
    /// Ethereum export metadata when the method is exported through Ethereum ABI.
    pub eth: Option<EthExportInfo>,
    /// HTTP export metadata when the method is exported as node-local HTTP ingress.
    #[serde(default)]
    pub http: Option<HttpExportInfo>,
}

const METHOD_EXPORT_SECTION: &str = "lyquor.method.export.eth";
const METHOD_EXPORT_VERSION: u8 = 2;
const METHOD_HTTP_EXPORT_SECTION: &str = "lyquor.method.export.http";
const METHOD_HTTP_EXPORT_VERSION: u8 = 1;
const METHOD_INFO_SECTION: &str = "lyquor.method.info";
const METHOD_INFO_VERSION: u8 = 1;
const LDK_SECTION: &str = "lyquor.ldk.version";
const LDK_SECTION_VERSION: u8 = 1;

/// Outcome of reading the `lyquor.ldk.version` descriptor from a Lyquid image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LdkDescriptor {
    /// No descriptor section is present; the image predates the LDK-version scheme.
    Absent,
    /// A descriptor with a recognized payload encoding, carrying the LDK version string.
    Version(String),
    /// A descriptor whose payload version is newer than this build can decode.
    Unrecognized,
}

/// Raw Ethereum export entry decoded from the custom method-export section.
#[derive(Debug, Clone)]
pub(crate) struct EthExportEntry {
    category: StateCategory,
    mutable: bool,
    guard: EthExportGuard,
    group: String,
    method: String,
    params: Vec<(String, bool)>,
    returns: Vec<(String, bool)>,
}

/// Raw HTTP export entry decoded from the custom HTTP method-export section.
#[derive(Debug, Clone)]
pub(crate) struct HttpExportEntry {
    category: StateCategory,
    group: String,
    method: String,
    http_method: String,
    path_prefix: String,
}

/// Raw method info entry decoded from the custom method-info section.
#[derive(Debug, Clone)]
pub(crate) struct MethodInfoEntry {
    pub(crate) category: StateCategory,
    pub(crate) mutable: bool,
    pub(crate) group: String,
    pub(crate) method: String,
}

/// Parse all Ethereum export metadata entries keyed by category, group, and method.
pub(crate) fn parse_method_exports(wasm: &[u8]) -> HashMap<(u8, String, String), EthExportEntry> {
    let mut exports = HashMap::new();
    let parser = Parser::new(0);
    for payload in parser.parse_all(wasm) {
        let Ok(Payload::CustomSection(section)) = payload else {
            continue;
        };
        if section.name() != METHOD_EXPORT_SECTION {
            continue;
        }
        let mut idx = 0usize;
        let data = section.data();
        while idx < data.len() {
            if let Some(entry) = decode_method_export_at(data, &mut idx) {
                exports.insert((entry.category as u8, entry.group.clone(), entry.method.clone()), entry);
            } else {
                break;
            }
        }
    }
    exports
}

/// Parse all HTTP export metadata entries keyed by category, group, and method.
pub(crate) fn parse_http_method_exports(wasm: &[u8]) -> HashMap<(u8, String, String), HttpExportEntry> {
    let mut exports = HashMap::new();
    let parser = Parser::new(0);
    for payload in parser.parse_all(wasm) {
        let Ok(Payload::CustomSection(section)) = payload else {
            continue;
        };
        if section.name() != METHOD_HTTP_EXPORT_SECTION {
            continue;
        }
        let mut idx = 0usize;
        let data = section.data();
        while idx < data.len() {
            if let Some(entry) = decode_http_method_export_at(data, &mut idx) {
                exports.insert((entry.category as u8, entry.group.clone(), entry.method.clone()), entry);
            } else {
                break;
            }
        }
    }
    exports
}

/// Parse all Lyquid method-info entries keyed by category, group, and method.
pub(crate) fn parse_method_info(wasm: &[u8]) -> HashMap<(u8, String, String), MethodInfoEntry> {
    let mut infos = HashMap::new();
    let parser = Parser::new(0);
    for payload in parser.parse_all(wasm) {
        let Ok(Payload::CustomSection(section)) = payload else {
            continue;
        };
        if section.name() != METHOD_INFO_SECTION {
            continue;
        }
        let mut idx = 0usize;
        let data = section.data();
        while idx < data.len() {
            if let Some(entry) = decode_method_info_at(data, &mut idx) {
                infos.insert((entry.category as u8, entry.group.clone(), entry.method.clone()), entry);
            } else {
                break;
            }
        }
    }
    infos
}

/// Return sorted `(category, group_hash, method)` tuples from parsed method info.
pub(crate) fn list_method_info_funcs(
    infos: &HashMap<(u8, String, String), MethodInfoEntry>,
) -> Vec<(String, String, String)> {
    let mut funcs = Vec::with_capacity(infos.len());
    for info in infos.values() {
        let cat = match info.category {
            StateCategory::Network => "network",
            StateCategory::Instance => "instance",
        }
        .to_string();
        let mut group_hash = String::new();
        lyquor_primitives::cb58::bs58::encode(info.group.as_bytes())
            .onto(&mut group_hash)
            .unwrap();
        funcs.push((cat, group_hash, info.method.clone()));
    }
    funcs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    funcs
}

/// Read the LDK descriptor from a WASM module.
///
/// The `state!` macro emits exactly one descriptor per Lyquid: a payload-version tag byte followed
/// by the UTF-8 LDK version string. A present-but-undecodable section (newer tag, or invalid
/// UTF-8) is reported as [LdkDescriptor::Unrecognized] rather than [LdkDescriptor::Absent], so the
/// load-time gate can reject images it cannot reason about instead of silently running them.
pub fn read_ldk_descriptor(wasm: &[u8]) -> LdkDescriptor {
    let parser = Parser::new(0);
    for payload in parser.parse_all(wasm) {
        let Ok(Payload::CustomSection(section)) = payload else {
            continue;
        };
        if section.name() != LDK_SECTION {
            continue;
        }
        let data = section.data();
        if data.first() != Some(&LDK_SECTION_VERSION) {
            return LdkDescriptor::Unrecognized;
        }
        return match String::from_utf8(data[1..].to_vec()) {
            Ok(version) => LdkDescriptor::Version(version),
            Err(_) => LdkDescriptor::Unrecognized,
        };
    }
    LdkDescriptor::Absent
}

/// Whether an image built with LDK `image` can be safely consumed by a host (node or toolchain)
/// built with LDK `host`.
///
/// The single source of truth for LDK compatibility, shared by the node's load-time gate and the
/// build-time toolchain warning so they cannot drift. The rule mirrors Cargo's SemVer semantics:
/// the major must match, and pre-1.0 (where a minor bump is breaking) the minor must match too;
/// from 1.0 on the host only has to be at least as new (`host.minor >= image.minor`).
pub fn ldk_versions_compatible(image: &semver::Version, host: &semver::Version) -> bool {
    image.major == host.major &&
        if host.major == 0 {
            image.minor == host.minor
        } else {
            host.minor >= image.minor
        }
}

/// Extract all Lyquid method metadata from custom sections in a WASM module.
pub fn extract_lyquid_functions_from_wasm(wasm: &[u8]) -> anyhow::Result<Vec<LyquidFunc>> {
    let exports = parse_method_exports(wasm);
    let http_exports = parse_http_method_exports(wasm);
    let infos = parse_method_info(wasm);
    let funcs = list_method_info_funcs(&infos);
    extract_lyquid_functions(funcs, &exports, &http_exports, &infos)
}

/// Extract the standard Ethereum JSON ABI for the Lyquid Ethereum call surface.
pub fn ethereum_json_abi_from_wasm(wasm: &[u8]) -> anyhow::Result<JsonAbi> {
    let mut entries = Vec::<AbiItem<'static>>::new();
    for func in extract_lyquid_functions_from_wasm(wasm)? {
        let LyquidFunc {
            category,
            mutable,
            group_hash,
            eth,
            method,
            ..
        } = func;
        let Some(eth) = eth else {
            continue;
        };
        let group = decode_group_hash(&group_hash);

        let inputs = nameless_abi_params(eth.params_canonical_types)?;
        if matches!(category, StateCategory::Network) && method == "__lyquid_constructor" {
            entries.push(
                Constructor {
                    inputs,
                    state_mutability: StateMutability::NonPayable,
                }
                .into(),
            );
            continue;
        }

        // This ABI describes the Lyquor node's Ethereum RPC surface rather than the generated
        // Solidity wrapper surface. Mutable network exports are transaction-callable through the
        // generated sequencer contract, which currently exposes only the default and `node`
        // network groups. Node-native `eth_call` routes only to instance methods in the default
        // group. Everything else stays out of the ABI until the corresponding RPC or contract
        // route can actually dispatch it.
        let state_mutability = match category {
            StateCategory::Network if mutable && (group == GROUP_DEFAULT || group == "node") => {
                StateMutability::NonPayable
            }
            StateCategory::Instance if group == GROUP_DEFAULT => StateMutability::View,
            StateCategory::Network | StateCategory::Instance => continue,
        };

        entries.push(
            Function {
                name: method,
                inputs,
                outputs: nameless_abi_params(eth.returns_canonical_types)?,
                state_mutability,
            }
            .into(),
        );
    }
    entries.sort_by_key(eth_json_abi_sort_key);
    Ok(entries.into_iter().collect())
}

fn decode_method_export_at(data: &[u8], idx: &mut usize) -> Option<EthExportEntry> {
    let mut cursor = *idx;
    if data.len().saturating_sub(cursor) < 1 {
        return None;
    }
    let version = data[cursor];
    cursor += 1;
    if version != METHOD_EXPORT_VERSION {
        return None;
    }
    if data.len().saturating_sub(cursor) < 9 {
        return None;
    }

    let category = match data[cursor] {
        0 => StateCategory::Network,
        1 => StateCategory::Instance,
        _ => return None,
    };
    cursor += 1;
    let mutable = data[cursor] != 0;
    cursor += 1;
    let guard = match data[cursor] {
        0 => EthExportGuard::None,
        1 => EthExportGuard::Creator,
        _ => return None,
    };
    cursor += 1;
    let param_count = data[cursor] as usize;
    cursor += 1;
    let return_count = data[cursor] as usize;
    cursor += 1;

    let group_len = read_u16(data, &mut cursor)? as usize;
    let method_len = read_u16(data, &mut cursor)? as usize;
    if cursor + group_len + method_len > data.len() {
        return None;
    }

    let group = String::from_utf8(data[cursor..cursor + group_len].to_vec()).ok()?;
    cursor += group_len;
    let method = String::from_utf8(data[cursor..cursor + method_len].to_vec()).ok()?;
    cursor += method_len;

    let mut params = Vec::with_capacity(param_count);
    for _ in 0..param_count {
        let len = read_u16(data, &mut cursor)? as usize;
        if cursor + len + 1 > data.len() {
            return None;
        }
        let name = String::from_utf8(data[cursor..cursor + len].to_vec()).ok()?;
        cursor += len;
        let needs_loc = data[cursor] != 0;
        cursor += 1;
        params.push((name, needs_loc));
    }

    let mut returns = Vec::with_capacity(return_count);
    for _ in 0..return_count {
        let len = read_u16(data, &mut cursor)? as usize;
        if cursor + len + 1 > data.len() {
            return None;
        }
        let name = String::from_utf8(data[cursor..cursor + len].to_vec()).ok()?;
        cursor += len;
        let needs_loc = data[cursor] != 0;
        cursor += 1;
        returns.push((name, needs_loc));
    }

    *idx = cursor;
    Some(EthExportEntry {
        category,
        mutable,
        guard,
        group,
        method,
        params,
        returns,
    })
}

fn decode_http_method_export_at(data: &[u8], idx: &mut usize) -> Option<HttpExportEntry> {
    let mut cursor = *idx;
    if data.len().saturating_sub(cursor) < 10 {
        return None;
    }
    let version = data[cursor];
    cursor += 1;
    if version != METHOD_HTTP_EXPORT_VERSION {
        return None;
    }

    let category = match data[cursor] {
        0 => StateCategory::Network,
        1 => StateCategory::Instance,
        _ => return None,
    };
    cursor += 1;

    let group_len = read_u16(data, &mut cursor)? as usize;
    let method_len = read_u16(data, &mut cursor)? as usize;
    let http_method_len = read_u16(data, &mut cursor)? as usize;
    let path_prefix_len = read_u16(data, &mut cursor)? as usize;
    if cursor + group_len + method_len + http_method_len + path_prefix_len > data.len() {
        return None;
    }

    let group = String::from_utf8(data[cursor..cursor + group_len].to_vec()).ok()?;
    cursor += group_len;
    let method = String::from_utf8(data[cursor..cursor + method_len].to_vec()).ok()?;
    cursor += method_len;
    let http_method = String::from_utf8(data[cursor..cursor + http_method_len].to_vec()).ok()?;
    cursor += http_method_len;
    let path_prefix = String::from_utf8(data[cursor..cursor + path_prefix_len].to_vec()).ok()?;
    cursor += path_prefix_len;

    *idx = cursor;
    Some(HttpExportEntry {
        category,
        group,
        method,
        http_method,
        path_prefix,
    })
}

fn decode_method_info_at(data: &[u8], idx: &mut usize) -> Option<MethodInfoEntry> {
    let mut cursor = *idx;
    if data.len().saturating_sub(cursor) < 7 {
        return None;
    }
    let version = data[cursor];
    cursor += 1;
    if version != METHOD_INFO_VERSION {
        return None;
    }

    let category = match data[cursor] {
        0 => StateCategory::Network,
        1 => StateCategory::Instance,
        _ => return None,
    };
    cursor += 1;
    let mutable = data[cursor] != 0;
    cursor += 1;

    let group_len = read_u16(data, &mut cursor)? as usize;
    let method_len = read_u16(data, &mut cursor)? as usize;
    if cursor + group_len + method_len > data.len() {
        return None;
    }

    let group = String::from_utf8(data[cursor..cursor + group_len].to_vec()).ok()?;
    cursor += group_len;
    let method = String::from_utf8(data[cursor..cursor + method_len].to_vec()).ok()?;
    cursor += method_len;

    *idx = cursor;
    Some(MethodInfoEntry {
        category,
        mutable,
        group,
        method,
    })
}

fn read_u16(data: &[u8], idx: &mut usize) -> Option<u16> {
    if *idx + 2 > data.len() {
        return None;
    }
    let val = ((data[*idx] as u16) << 8) | (data[*idx + 1] as u16);
    *idx += 2;
    Some(val)
}

fn format_params(params: &[(String, bool)], is_constructor: bool) -> (String, String) {
    let loc = if is_constructor { "memory" } else { "calldata" };
    let mut params_str = String::new();
    let mut canonical = String::new();
    canonical.push('(');

    for (idx, (ty, needs_loc)) in params.iter().enumerate() {
        if idx > 0 {
            params_str.push_str(", ");
            canonical.push(',');
        }
        params_str.push_str(ty);
        if *needs_loc {
            params_str.push(' ');
            params_str.push_str(loc);
        }
        canonical.push_str(ty);
    }

    canonical.push(')');
    (params_str, canonical)
}

fn format_returns(returns: &[(String, bool)]) -> String {
    let loc = "memory";
    let mut out = String::new();

    for (idx, (ty, needs_loc)) in returns.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push_str(ty);
        if *needs_loc {
            out.push(' ');
            out.push_str(loc);
        }
    }

    out
}

fn nameless_abi_params(types: Vec<String>) -> anyhow::Result<Vec<Param>> {
    types
        .into_iter()
        .map(|ty| Param::new("", &ty, Vec::new(), None).map_err(|err| anyhow::anyhow!(err.to_string())))
        .collect()
}

fn eth_json_abi_sort_key(entry: &AbiItem<'_>) -> (u8, String, String) {
    match entry {
        AbiItem::Constructor(constructor) => (0, String::new(), abi_signature_types(&constructor.inputs)),
        AbiItem::Function(function) => (1, function.name.clone(), abi_signature_types(&function.inputs)),
        AbiItem::Fallback(_) => (2, String::new(), String::new()),
        AbiItem::Receive(_) => (3, String::new(), String::new()),
        AbiItem::Event(event) => (4, event.name.clone(), String::new()),
        AbiItem::Error(error) => (5, error.name.clone(), abi_signature_types(&error.inputs)),
    }
}

fn abi_signature_types(params: &[Param]) -> String {
    params
        .iter()
        .map(|param| param.selector_type().into_owned())
        .collect::<Vec<_>>()
        .join(",")
}

/// Merge method-info and export metadata into public Lyquid function descriptors.
pub(crate) fn extract_lyquid_functions(
    funcs: Vec<(String, String, String)>, exports: &HashMap<(u8, String, String), EthExportEntry>,
    http_exports: &HashMap<(u8, String, String), HttpExportEntry>,
    infos: &HashMap<(u8, String, String), MethodInfoEntry>,
) -> anyhow::Result<Vec<LyquidFunc>> {
    let mut result = Vec::new();
    if infos.is_empty() {
        return Err(anyhow::anyhow!("Missing method info section."));
    }
    validate_http_export_table(http_exports, infos)?;

    for (cat, group, method) in funcs {
        let category = match cat.as_str() {
            "network" => StateCategory::Network,
            "instance" => StateCategory::Instance,
            _ => continue,
        };
        let group_decoded = decode_group_hash(&group);

        let export_key = (category as u8, group_decoded.clone(), method.clone());
        let info_entry = infos.get(&export_key);
        let eth = if let Some(export) = exports.get(&export_key) {
            let (params, params_canonical) = format_params(&export.params, export.method == "__lyquid_constructor");
            let returns = format_returns(&export.returns);
            let selector =
                alloy_primitives::utils::keccak256(format!("{}{}", export.method, params_canonical).as_bytes()).0[..4]
                    .try_into()
                    .unwrap();
            Some(EthExportInfo {
                selector,
                guard: export.guard,
                params,
                returns,
                params_canonical_types: export.params.iter().map(|(ty, _)| ty.clone()).collect(),
                returns_canonical_types: export.returns.iter().map(|(ty, _)| ty.clone()).collect(),
            })
        } else {
            None
        };
        let http = http_exports.get(&export_key).map(|export| HttpExportInfo {
            method: export.http_method.clone(),
            path_prefix: export.path_prefix.clone(),
        });

        let (category, mutable) = if let Some(info) = info_entry {
            (info.category, info.mutable)
        } else if let Some(export) = exports.get(&export_key) {
            (export.category, export.mutable)
        } else {
            tracing::warn!("Missing method info for {cat}:{group}:{method}, ignoring this function.");
            continue;
        };

        result.push(LyquidFunc {
            category,
            mutable,
            group_hash: group,
            eth,
            http,
            method,
        });
    }
    Ok(result)
}

fn decode_group_hash(group_hash: &str) -> String {
    match lyquor_primitives::cb58::bs58::decode(group_hash).into_vec() {
        Ok(bytes) => String::from_utf8(bytes).unwrap_or_else(|_| "main".to_string()),
        Err(_) => "main".to_string(),
    }
}

fn validate_http_export_table(
    http_exports: &HashMap<(u8, String, String), HttpExportEntry>,
    infos: &HashMap<(u8, String, String), MethodInfoEntry>,
) -> anyhow::Result<()> {
    // The proc macro validates each HTTP export's syntax and signature. The
    // image extractor can only validate relationships across emitted sections.
    let mut http_export_keys = HashMap::<(String, String), String>::new();
    for (metadata_key, export) in http_exports {
        let Some(info) = infos.get(metadata_key) else {
            return Err(anyhow::anyhow!(
                "HTTP export `{}`::`{}` does not correspond to a Lyquid method info entry",
                export.group,
                export.method
            ));
        };
        if info.category != StateCategory::Instance {
            return Err(anyhow::anyhow!(
                "HTTP export `{}`::`{}` must correspond to an instance method info entry",
                export.group,
                export.method
            ));
        }
        let key = (export.http_method.clone(), export.path_prefix.clone());
        let owner = format!("{}::{}", export.group, export.method);
        if let Some(previous) = http_export_keys.insert(key.clone(), owner.clone()) {
            return Err(anyhow::anyhow!(
                "duplicate HTTP export for method `{}` and path_prefix `{}`: {previous} and {owner}",
                key.0,
                key.1
            ));
        }
    }
    Ok(())
}

// Rewrites wasm imports/ops while reencoding to avoid walrus.
struct WasmBinaryRewriter {
    wait_type_index: Option<u32>,
    notify_type_index: Option<u32>,
    wait_import_index: Option<u32>,
    notify_import_index: Option<u32>,
    original_import_func_count: u32,
    added_imports: u32,
    is_64bits: bool,
}

impl WasmBinaryRewriter {
    fn new(is_64bits: bool) -> Self {
        Self {
            wait_type_index: None,
            notify_type_index: None,
            wait_import_index: None,
            notify_import_index: None,
            original_import_func_count: 0,
            added_imports: 0,
            is_64bits,
        }
    }

    fn require_index(&self, label: &'static str, index: Option<u32>) -> Result<u32, reencode::Error<anyhow::Error>> {
        index.ok_or_else(|| reencode::Error::UserError(anyhow::anyhow!("Missing {label} index")))
    }

    fn record_import(
        &mut self, imports: &mut ImportSection, func_index: &mut u32, import: Import<'_>,
    ) -> Result<(), reencode::Error<anyhow::Error>> {
        match import.ty {
            TypeRef::Func(_) | TypeRef::FuncExact(_) => {
                if import.module == "lyquor_api" && import.name == "__wait" {
                    self.wait_import_index = Some(*func_index);
                }
                if import.module == "lyquor_api" && import.name == "__notify" {
                    self.notify_import_index = Some(*func_index);
                }
                *func_index += 1;
            }
            _ => {}
        }

        let entity = match import.ty {
            TypeRef::Memory(mut mem) if import.module == "env" && import.name == "memory" => {
                mem.initial = 65536;
                mem.maximum = Some(65536);
                mem.shared = true;
                EntityType::Memory(self.memory_type(mem)?)
            }
            _ => self.entity_type(import.ty)?,
        };
        imports.import(import.module, import.name, entity);
        Ok(())
    }
}

impl Reencode for WasmBinaryRewriter {
    type Error = anyhow::Error;

    fn parse_type_section(
        &mut self, types: &mut TypeSection, section: wasmparser::TypeSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        reencode::utils::parse_type_section(self, types, section)?;
        let base = types.len();
        let bits_dep_type = if self.is_64bits { ValType::I64 } else { ValType::I32 };
        self.wait_type_index = Some(base);
        types.ty().function(
            [bits_dep_type, ValType::I32, ValType::I64, bits_dep_type],
            [ValType::I32],
        );
        self.notify_type_index = Some(base + 1);
        types
            .ty()
            .function([bits_dep_type, ValType::I32, bits_dep_type], [ValType::I32]);
        Ok(())
    }

    // A valid lyquid wasm should have import section, if it doesn't we need hard fail anyway.
    fn parse_import_section(
        &mut self, imports: &mut ImportSection, section: wasmparser::ImportSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        let mut func_index = 0;
        for group in section {
            match group? {
                Imports::Single(_, import) => {
                    self.record_import(imports, &mut func_index, import)?;
                }
                Imports::Compact1 { module, items } => {
                    for item in items {
                        let item = item?;
                        let import = Import {
                            module,
                            name: item.name,
                            ty: item.ty,
                        };
                        self.record_import(imports, &mut func_index, import)?;
                    }
                }
                Imports::Compact2 { module, ty, names } => {
                    for name in names {
                        let name = name?;
                        let import = Import { module, name, ty };
                        self.record_import(imports, &mut func_index, import)?;
                    }
                }
            }
        }

        self.original_import_func_count = func_index;

        let wait_type = self.require_index("__wait type", self.wait_type_index)?;
        let notify_type = self.require_index("__notify type", self.notify_type_index)?;
        let mut added = 0;
        if self.wait_import_index.is_none() {
            let index = func_index + added;
            imports.import("lyquor_api", "__wait", EntityType::Function(wait_type));
            self.wait_import_index = Some(index);
            added += 1;
        }
        if self.notify_import_index.is_none() {
            let index = func_index + added;
            imports.import("lyquor_api", "__notify", EntityType::Function(notify_type));
            self.notify_import_index = Some(index);
            added += 1;
        }
        self.added_imports = added;
        Ok(())
    }

    fn parse_function_body(
        &mut self, code: &mut CodeSection, func: wasmparser::FunctionBody<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        let mut function = self.new_function_with_parsed_locals(&func)?;
        let mut reader = func.get_operators_reader()?;
        while !reader.eof() {
            let op = reader.read()?;
            // TODO: add AtomicWait64 support after we land wasm64 patch
            match op {
                Operator::MemoryAtomicWait32 { memarg } => {
                    let wait = self.require_index("__wait import", self.wait_import_index)?;
                    function.instruction(&Instruction::I32Const(memarg.offset as i32));
                    function.instruction(&Instruction::Call(wait));
                }
                Operator::MemoryAtomicNotify { memarg } => {
                    let notify = self.require_index("__notify import", self.notify_import_index)?;
                    let instr = if self.is_64bits {
                        Instruction::I64Const(memarg.offset as i64)
                    } else {
                        Instruction::I32Const(memarg.offset as i32)
                    };
                    function.instruction(&instr);
                    function.instruction(&Instruction::Call(notify));
                }
                _ => {
                    if is_unsupported_atomic_operator(&op) {
                        return Err(reencode::Error::UserError(anyhow::anyhow!(
                            "Unsupported atomic instruction: {op:?}"
                        )));
                    }
                    let instruction = self.instruction(op)?;
                    function.instruction(&instruction);
                }
            }
        }
        code.function(&function);
        Ok(())
    }

    fn function_index(&mut self, func: u32) -> Result<u32, reencode::Error<Self::Error>> {
        if func < self.original_import_func_count {
            Ok(func)
        } else {
            Ok(func + self.added_imports)
        }
    }
}

/// Rewrite a Lyquid WASM binary into the normalized form expected by the VM.
///
/// The rewriter patches memory imports and atomic wait/notify calls. `is_64bits` selects the
/// guest pointer width used while rewriting host ABI imports.
pub fn process_binary(input: &[u8], is_64bits: bool) -> anyhow::Result<Vec<u8>> {
    tracing::debug!("Processing WASM binary..");
    let mut module = Module::new();
    let mut rewriter = WasmBinaryRewriter::new(is_64bits);
    reencode::utils::parse_core_module(&mut rewriter, &mut module, Parser::new(0), input)
        .map_err(|err| anyhow::anyhow!("Failed to reencode the wasm binary: {err}"))?;

    Ok(module.finish())
}

#[inline]
fn is_unsupported_atomic_operator(op: &Operator<'_>) -> bool {
    matches!(op, Operator::MemoryAtomicWait64 { .. })
}

#[cfg(test)]
mod tests {
    use super::ldk_versions_compatible;
    use semver::Version;

    fn compat(image: &str, host: &str) -> bool {
        ldk_versions_compatible(&Version::parse(image).unwrap(), &Version::parse(host).unwrap())
    }

    #[test]
    fn ldk_compatibility_follows_semver_lines() {
        // Pre-1.0: a minor bump is breaking, so minors must match (patch is irrelevant).
        assert!(compat("0.1.0", "0.1.3"));
        assert!(compat("0.1.5", "0.1.0"));
        assert!(!compat("0.1.0", "0.2.0"));
        assert!(!compat("0.2.0", "0.1.0"));
        assert!(!compat("0.1.0", "1.0.0"));

        // 1.0+: same major, and the host must be at least as new as the image.
        assert!(compat("1.2.0", "1.4.0"));
        assert!(compat("1.2.0", "1.2.9"));
        assert!(!compat("1.5.0", "1.2.0"));
        assert!(!compat("1.0.0", "2.0.0"));
    }
}
