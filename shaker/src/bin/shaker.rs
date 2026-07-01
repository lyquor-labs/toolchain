use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;

use clap::{ArgMatches, Command};
use lyquor_proto::core::v1::LyquidId as ProtoLyquidId;
use lyquor_proto::lyquid::v1::{
    ConsoleSink as GrpcConsoleSink, GetLyquidInfoRequest, ListLyquidsRequest, LyquidInfo, StreamConsoleRequest,
    lyquid_service_client::LyquidServiceClient,
};
use lyquor_proto::node::v1::{GetNodeInfoRequest, node_service_client::NodeServiceClient};
use tokio::signal::unix::{SignalKind, signal};
use tonic::transport::Channel;

use alloy_json_abi::JsonAbi;
use anyhow::Context as _Context;
use lyquor_jsonrpc::client::ClientConfig;
use lyquor_oci::pack::{LyquidPack, LyquidPackDigest, serialize_canonical_json};
use lyquor_oci::registry::Reference as RegistryReference;
use lyquor_primitives::{Address, B256, LyquidID, StateCategory, alloy_primitives::Bytes, hex};

fn parse_push_oci_references(input: &str) -> anyhow::Result<Vec<RegistryReference>> {
    let mut references = Vec::new();

    for (index, raw_reference) in input.split(',').enumerate() {
        let reference = raw_reference.trim();
        if reference.is_empty() {
            anyhow::bail!("Invalid OCI reference list: entry {} is empty.", index + 1);
        }

        let reference = RegistryReference::from_str(reference)
            .map_err(|e| anyhow::anyhow!("Invalid OCI reference at entry {} ({reference}): {e}", index + 1))?;
        references.push(reference);
    }

    if references.is_empty() {
        anyhow::bail!("At least one OCI reference must be provided.");
    }

    Ok(references)
}

fn parse_deploy_reference(raw: &str) -> anyhow::Result<RegistryReference> {
    RegistryReference::from_str(raw).map_err(|e| anyhow::anyhow!(e.to_string()))
}

fn normalize_image_digest_filter(input: &str) -> anyhow::Result<String> {
    let input = input.trim();
    if input.is_empty() {
        anyhow::bail!("Invalid image digest: value is empty.");
    }

    let digest = LyquidPackDigest::from_oci_digest(input)
        .map_err(|err| anyhow::anyhow!("Invalid image digest `{input}`: {err}"))?;
    Ok(digest.to_oci_digest())
}

fn node_image_digest_to_oci_digest(input: &str, lyquid_id: &str) -> anyhow::Result<String> {
    if input.is_empty() {
        anyhow::bail!("GetLyquidInfo for Lyquid `{lyquid_id}` did not include image_digest");
    }
    let digest =
        B256::from_str(input).map_err(|err| anyhow::anyhow!("Invalid image_digest for Lyquid `{lyquid_id}`: {err}"))?;
    Ok(LyquidPackDigest::new(digest).to_oci_digest())
}

fn write_canonical_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    let mut stdout = io::stdout();
    stdout
        .write_all(&serialize_canonical_json(value).map_err(|e| anyhow::anyhow!(e.to_string()))?)
        .context("Failed to write JSON output")?;
    stdout.write_all(b"\n").context("Failed to write JSON output")?;
    Ok(())
}

#[derive(serde::Serialize)]
struct ListJson {
    lyquids: Vec<ListLyquidJson>,
}

#[derive(serde::Serialize)]
struct ListLyquidJson {
    lyquid_id: String,
    contract: String,
    sequence_backend: String,
    image_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    lyquid_number: Option<ListLyquidNumberJson>,
}

#[derive(serde::Serialize)]
struct ListLyquidNumberJson {
    image: u32,
    var: u32,
}

impl ListLyquidJson {
    fn from_info(info: LyquidInfo, fallback_id: String) -> anyhow::Result<Self> {
        let lyquid_id = info.lyquid_id.map_or(fallback_id, |id| id.value);
        let contract = info
            .contract
            .with_context(|| format!("GetLyquidInfo for Lyquid `{lyquid_id}` did not include contract"))?
            .value;
        if info.sequence_backend.is_empty() {
            anyhow::bail!("GetLyquidInfo for Lyquid `{lyquid_id}` did not include sequence_backend");
        }
        let image_digest = node_image_digest_to_oci_digest(&info.image_digest, &lyquid_id)?;
        let lyquid_number = info.lyquid_number.map(|number| ListLyquidNumberJson {
            image: number.image,
            var: number.var,
        });

        Ok(Self {
            lyquid_id,
            contract,
            sequence_backend: info.sequence_backend,
            image_digest,
            lyquid_number,
        })
    }
}

async fn collect_list_lyquid_json(
    client: &mut LyquidServiceClient<Channel>, lyquid_ids: Vec<ProtoLyquidId>, image_digest_filter: Option<&str>,
) -> anyhow::Result<Vec<ListLyquidJson>> {
    let mut lyquids = Vec::new();
    for lyquid_id in lyquid_ids {
        let fallback_id = lyquid_id.value.clone();
        let info = client
            .get_lyquid_info(GetLyquidInfoRequest {
                lyquid_id: Some(lyquid_id),
            })
            .await
            .with_context(|| format!("Failed to get Lyquid info for `{fallback_id}`"))?
            .into_inner()
            .lyquid_info
            .with_context(|| format!("GetLyquidInfo returned no info for listed Lyquid `{fallback_id}`"))?;
        let lyquid = ListLyquidJson::from_info(info, fallback_id)?;
        if image_digest_filter.is_none_or(|digest| lyquid.image_digest == digest) {
            lyquids.push(lyquid);
        }
    }
    Ok(lyquids)
}

fn format_constructor_inputs(eth_abi: &JsonAbi) -> String {
    eth_abi.constructor.as_ref().map_or_else(
        || "(none)".to_owned(),
        |constructor| shaker::format_abi_param_tuple(&constructor.inputs),
    )
}

#[derive(serde::Serialize)]
struct InspectJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ldk_descriptor: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ldk_version: Option<String>,
    constructor_inputs: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    methods: Vec<InspectMethod>,
}

#[derive(serde::Serialize)]
struct InspectMethod {
    category: &'static str,
    mutable: bool,
    group_hash: String,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    eth: Option<InspectEthExport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http: Option<lyquor_wasm::HttpExportInfo>,
}

#[derive(serde::Serialize)]
struct InspectEthExport {
    selector: String,
    params: String,
    returns: String,
    params_canonical_types: Vec<String>,
    returns_canonical_types: Vec<String>,
}

fn inspect_methods_json(funcs: Vec<lyquor_wasm::LyquidFunc>) -> Vec<InspectMethod> {
    funcs
        .into_iter()
        .map(|func| InspectMethod {
            category: match func.category {
                StateCategory::Network => "network",
                StateCategory::Instance => "instance",
            },
            mutable: func.mutable,
            group_hash: func.group_hash,
            method: func.method,
            eth: func.eth.map(|eth| InspectEthExport {
                selector: hex::encode(eth.selector),
                params: eth.params,
                returns: eth.returns,
                params_canonical_types: eth.params_canonical_types,
                returns_canonical_types: eth.returns_canonical_types,
            }),
            http: func.http,
        })
        .collect()
}

fn with_registry_auth_args(command: Command) -> Command {
    command
        .arg(clap::arg!(-t --token <TOKEN> "oauth2 token").required(false))
        .arg(clap::arg!(-u --username <USERNAME> "username for auth").required(false))
        .arg(clap::arg!(-p --password <PASSWORD> "password for auth").required(false))
}

fn registry_auth_options(sub: &ArgMatches) -> (Option<&str>, Option<&str>, Option<&str>) {
    (
        sub.get_one::<String>("token").map(String::as_str),
        sub.get_one::<String>("username").map(String::as_str),
        sub.get_one::<String>("password").map(String::as_str),
    )
}

async fn warn_if_node_version_differs(endpoint: &str) {
    if let Err(err) = warn_if_node_version_differs_inner(endpoint).await {
        tracing::debug!("Failed to check node version at {endpoint}: {err:#}");
    }
}

async fn warn_if_node_version_differs_inner(endpoint: &str) -> anyhow::Result<()> {
    let (_, channel) = shaker::connect_grpc_api_channel(endpoint, "NodeService").await?;
    let mut client = NodeServiceClient::new(channel);
    warn_if_node_version_differs_with_client(&mut client, endpoint).await
}

async fn warn_if_node_version_differs_with_client(
    client: &mut NodeServiceClient<Channel>, endpoint: &str,
) -> anyhow::Result<()> {
    let node_info = client
        .get_node_info(GetNodeInfoRequest {})
        .await
        .with_context(|| format!("Failed to get node info from `{endpoint}`"))?
        .into_inner();
    shaker::warn_if_node_version_differs(endpoint, &node_info.version);

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    lyquor_cli::setup_tracing()?;

    let matches = clap::command!()
        .version(lyquor_cli::build_version!())
        .propagate_version(true)
        .subcommand_required(true)
        .subcommand(
            Command::new("solidity")
                .about("Generate solidity sequencer contract.")
                .arg_required_else_help(true)
                .arg(clap::arg!([WASM_FILE]))
                .arg(clap::arg!(--"is-bartender").action(clap::ArgAction::SetTrue)),
        )
        .subcommand(
            Command::new("build")
                .about("Build and generate files useful for deployment.")
                .arg_required_else_help(true)
                .arg(clap::arg!([LYQUID_MANIFEST]))
                .arg(clap::arg!(--"is-bartender").action(clap::ArgAction::SetTrue))
                .arg(
                    clap::arg!(--debug "Build the target with dev profile.")
                    .required(false)
                ),

        )
        .subcommand(with_registry_auth_args(
            Command::new("push")
                .about("Build the lyquid and push the image without deploying.")
                .arg_required_else_help(true)
                .arg(clap::arg!([LYQUID_MANIFEST]))
                .arg(clap::arg!(--"is-bartender").action(clap::ArgAction::SetTrue))
                .arg(
                    clap::arg!(--debug "Build the target with dev profile.")
                    .required(false)
                )
                .arg(
                    clap::arg!(-r --reference <REFERENCES> "Comma-separated OCI-compliant references. If omitted, push to the node OCI distribution API derived from --endpoint.")
                        .required(false)
                )
                .arg(
                    clap::arg!(-e --endpoint <URL> "Lyquor node's API endpoint.")
                        .required(false)
                        .default_value(std::env::var("LYQUOR_ENDPOINT").unwrap_or_else(|_| "ws://localhost:10087/ws".into())),
                )

        ))
        .subcommand(with_registry_auth_args(
            Command::new("deploy")
                .about("Build and deploy.")
                .arg_required_else_help(true)
                .arg(
                    clap::arg!([LYQUID_MANIFEST])
                        .required(false)
                )
                .arg(
                    clap::arg!(--update <LYQUID_ID> "Specify an existing Lyquid to update the code. A new Lyquid will be created and deployed without this option.")
                        .required(false)
                        .conflicts_with("is-bartender")
                        .value_parser(shaker::parse_lyquid_id)
                )
                .arg(
                    clap::arg!(--bartender <ADDR> "Use the specified bartender contract address instead of resolving it from the node.")
                        .required(false)
                        .conflicts_with("is-bartender")
                        .value_parser(shaker::parse_address)
                )
                .arg(
                    clap::arg!(--"is-bartender" "Deploy as bartender (self-register and include bartender-specific constructor libraries).")
                        .required(false)
                        .action(clap::ArgAction::SetTrue)
                        .conflicts_with("bartender")
                        .conflicts_with("update")
                )
                .arg(
                    clap::arg!(--debug "Build the target with dev profile.")
                    .required(false)
                )
                .arg(
                    clap::arg!(-r --reference <REFERENCE> "OCI-compliant reference. With <LYQUID_MANIFEST>, this tag reference is used as the push target and deployment source (pinned to the pushed digest). Without <LYQUID_MANIFEST>, deploy from the provided reference directly.")
                        .required(false)
                )
                .arg(
                    clap::arg!(-e --endpoint <URL> "Lyquor node's API endpoint.")
                        .required(false)
                        .default_value(std::env::var("LYQUOR_ENDPOINT").unwrap_or_else(|_| "ws://localhost:10087/ws".into())),
                    )
                .arg(
                    clap::arg!(-i --input <HEX> "Ethereum ABI-encoded input data to Lyquid's constructor.")
                        .required(false)
                        .value_parser(shaker::parse_hex_bytes)
                )
                .arg(
                    clap::arg!(--"private-key" <HEX> "Private key to sequence the transaction. (Anvil/Hardhat devnet key will be used if not present.)")
                        .required(false)
                        .value_parser(shaker::parse_hex_bytes)
                )
                .arg(
                    clap::arg!(-o --output <FORMAT> "Output format for deploy result (`text` or `json`).")
                        .required(false)
                        .default_value("text")
                        .value_parser(["text", "json"])
                )
        ))
        .subcommand(
            Command::new("list")
                .about("List Lyquids visible to a node.")
                .arg(
                    clap::arg!(--"hosted-only" "List only Lyquids currently hosted by this node.")
                        .required(false)
                        .action(clap::ArgAction::SetTrue)
                )
                .arg(
                    clap::arg!(--"image-digest" <DIGEST> "Only include Lyquids with this deployed image digest (`sha256:...`).")
                        .required(false)
                )
                .arg(
                    clap::arg!(-e --endpoint <URL> "Lyquor node's API endpoint.")
                        .required(false)
                        .default_value(std::env::var("LYQUOR_ENDPOINT").unwrap_or_else(|_| "ws://localhost:10087/ws".into())),
                )
                .arg(
                    clap::arg!(-o --output <FORMAT> "Output format for list result (`text` or `json`).")
                        .required(false)
                        .default_value("text")
                        .value_parser(["text", "json"])
                )
        )
        .subcommand(
            Command::new("console")
                .about("Get the console output from a running Lyquid.")
                .arg_required_else_help(true)
                .arg(clap::arg!([LYQUID_ID]).value_parser(shaker::parse_lyquid_id))
                .arg(
                    clap::arg!(-e --endpoint <URL> "Lyquor node's API endpoint.")
                        .required(false)
                        .default_value(std::env::var("LYQUOR_ENDPOINT").unwrap_or_else(|_| "ws://localhost:10087/ws".into())),
                    ))
        .subcommand(
            Command::new("serve")
                .about("Proxy localhost HTTP traffic to a Lyquid's virtual host on a local node.")
                .arg_required_else_help(true)
                .arg(clap::arg!(<LYQUID_ID>).value_parser(shaker::parse_lyquid_id))
                .arg(
                    clap::arg!(-e --endpoint <URL> "Lyquor node's API endpoint.")
                        .required(false)
                        .default_value(std::env::var("LYQUOR_ENDPOINT").unwrap_or_else(|_| shaker::DEFAULT_SERVE_ENDPOINT.into())),
                    )
                .arg(
                    clap::arg!(--listen <ADDR> "Local address for the proxy to listen on.")
                        .required(false)
                        .default_value(shaker::DEFAULT_SERVE_LISTEN)
                        .value_parser(clap::value_parser!(std::net::SocketAddr))
                ))
        .subcommand(
            Command::new("to-hex")
                .about("Convert LyquidID to hex address format.")
                .arg_required_else_help(true)
                .arg(clap::arg!([LYQUID_ID]).value_parser(shaker::parse_lyquid_id)))
        .subcommand(with_registry_auth_args(
            Command::new("inspect")
                .about("Show metadata embedded in a built Lyquid pack, raw WASM binary, or OCI reference.")
                .arg_required_else_help(true)
                .arg(clap::arg!([PACK_WASM_OR_OCI_REFERENCE]))
                .arg(clap::arg!(--"abi-json" "Print the Ethereum JSON ABI only.").action(clap::ArgAction::SetTrue))
                .arg(
                    clap::arg!(-o --output <FORMAT> "Output format for inspect metadata (`text` or `json`). Ignored by --abi-json.")
                        .required(false)
                        .default_value("text")
                        .value_parser(["text", "json"])
                ),
        ))
        .get_matches();
    match matches.subcommand() {
        Some(("solidity", sub)) => {
            let is_bartender = sub.get_flag("is-bartender");
            std::io::stdout()
                .write_all(
                    shaker::generate_solidity_sequencer_from_file(
                        &sub.get_one::<String>("WASM_FILE").unwrap(),
                        is_bartender,
                    )
                    .await?
                    .as_bytes(),
                )
                .ok();
        }
        Some(("build", sub)) => {
            let is_bartender = sub.get_flag("is-bartender");
            let debug = *sub.get_one::<bool>("debug").unwrap_or(&false);
            let manifest = PathBuf::from(sub.get_one::<String>("LYQUID_MANIFEST").unwrap());
            let target_dir = PathBuf::from("./lyquid_tools_target");
            let options = shaker::BuildOptions {
                manifest,
                target_dir: target_dir.clone(),
                debug,
                is_bartender,
            };
            let pack = shaker::build_lyquid(&options)
                .await
                .context("WASM compilation error.")?;
            let lyquid_dst = target_dir
                .join(if debug { "debug" } else { "release" })
                .join(&pack.metadata().name)
                .join("lyquid.pack");
            std::fs::create_dir_all(lyquid_dst.parent().unwrap())
                .context("Failed to create output directory for Lyquid pack")?;
            std::fs::write(
                &lyquid_dst,
                pack.to_repo_bytes().context("Failed to encode Lyquid pack")?,
            )
            .context("Failed to write Lyquid pack to output directory")?;
            tracing::info!("Build success: lyquid=\"{}\".", lyquid_dst.display());
        }
        Some(("push", sub)) => {
            let is_bartender = sub.get_flag("is-bartender");

            let debug = *sub.get_one::<bool>("debug").unwrap_or(&false);
            let endpoint = sub.get_one::<String>("endpoint").unwrap();
            let references = sub
                .get_one::<String>("reference")
                .map(|input| parse_push_oci_references(input))
                .transpose()?;
            let manifest = sub.get_one::<String>("LYQUID_MANIFEST").unwrap();

            let (auth_token, auth_username, auth_password) = registry_auth_options(sub);
            let options = shaker::BuildOptions {
                manifest: PathBuf::from(manifest),
                target_dir: PathBuf::from("./lyquid_tools_target"),
                debug,
                is_bartender,
            };
            if references.is_none() {
                warn_if_node_version_differs(endpoint).await;
            }
            let pack = shaker::build_lyquid(&options).await?;

            match references {
                None => {
                    let hash = shaker::push_lyquid_to_endpoint(pack, endpoint).await?;
                    let image_digest = LyquidPackDigest::new(hash).to_oci_digest();
                    tracing::info!("Pushed image hash {image_digest} to {endpoint}");
                }
                Some(references) => {
                    let mut failures = Vec::new();

                    for reference in &references {
                        let reference_str = reference.to_string();
                        let oci =
                            shaker::oci_registry_from_reference(reference, auth_token, auth_username, auth_password);

                        match shaker::push_lyquid(pack.clone(), &oci, reference.reference()).await {
                            Ok(hash) => {
                                let image_digest = LyquidPackDigest::new(hash).to_oci_digest();
                                tracing::info!("Pushed image hash {image_digest} to {reference_str}");
                            }
                            Err(err) => {
                                tracing::error!("Failed to push image to {reference_str}: {err}");
                                failures.push(format!("{reference_str}: {err}"));
                            }
                        }
                    }

                    if !failures.is_empty() {
                        anyhow::bail!(
                            "Failed to push image to {} OCI reference(s):\n{}",
                            failures.len(),
                            failures.join("\n")
                        );
                    }
                }
            }
        }
        Some(("deploy", sub)) => {
            let output_format = sub.get_one::<String>("output").map_or("text", String::as_str);
            let bartender = sub.get_one::<Address>("bartender").copied();
            let is_bartender = sub.get_flag("is-bartender");
            let debug = *sub.get_one::<bool>("debug").unwrap_or(&false);
            let id = sub.get_one::<LyquidID>("update").copied();
            let endpoint = sub.get_one::<String>("endpoint").unwrap();
            let manifest = sub
                .get_one::<String>("LYQUID_MANIFEST")
                .map(String::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let mut reference = sub
                .get_one::<String>("reference")
                .map(String::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(parse_deploy_reference)
                .transpose()?;

            let (auth_token, auth_username, auth_password) = registry_auth_options(sub);

            if manifest.is_none() && reference.is_none() {
                anyhow::bail!("Specify at least one deploy source: a manifest path and/or --reference <REFERENCE>.");
            }
            if manifest.is_none() && debug {
                anyhow::bail!("--debug is only valid when deploying from a manifest.");
            }

            let client = ClientConfig::builder()
                .url(endpoint.parse()?)
                .build()
                .into_client(tokio_util::sync::CancellationToken::new());
            let (_, grpc_channel) = shaker::connect_grpc_api_channel(endpoint, "LyquidService").await?;
            let mut node_client = NodeServiceClient::new(grpc_channel.clone());
            if let Err(err) = warn_if_node_version_differs_with_client(&mut node_client, endpoint).await {
                tracing::debug!("Failed to check node version at {endpoint}: {err:#}");
            }
            let mut grpc_client = LyquidServiceClient::new(grpc_channel);

            let bartender = if is_bartender {
                None
            } else {
                Some(match bartender {
                    Some(b) => b,
                    None => {
                        let resp = grpc_client
                            .get_lyquid_info(GetLyquidInfoRequest { lyquid_id: None })
                            .await
                            .context("Cannot obtain the bartender info.")?
                            .into_inner();
                        let info = resp.lyquid_info.context("Bartender info should not be null.")?;
                        info.contract
                            .context("Bartender contract address should not be null.")?
                            .try_into()
                            .map_err(|err| {
                                anyhow::anyhow!(
                                    "Invalid bartender contract address returned by GetLyquidInfo: ({err:?})"
                                )
                            })?
                    }
                })
            };

            let action;
            let superseded = match id {
                Some(id) => {
                    let resp = grpc_client
                        .get_lyquid_info(GetLyquidInfoRequest {
                            lyquid_id: Some(id.into()),
                        })
                        .await
                        .context("Cannot obtain the deployment info.")?
                        .into_inner();
                    action = "updated";
                    resp.lyquid_info
                        .map(|info| {
                            info.contract
                                .context("Contract address should not be null.")?
                                .try_into()
                                .map_err(|err| {
                                    anyhow::anyhow!(
                                        "Invalid contract address returned by GetLyquidInfo for Lyquid `{id}`: ({err:?})"
                                    )
                                })
                        })
                        .transpose()?
                }
                None => {
                    action = "created";
                    None
                }
            };
            let signer = match sub.get_one::<Bytes>("private-key") {
                Some(pkey) => lyquor_eth::signer_from_bytes(pkey.clone().into()),
                None => shaker::devnet_signer(),
            }?;
            let deploy_options = shaker::DeployOptions {
                bartender,
                superseded,
                input: sub.get_one::<Bytes>("input").cloned(),
            };
            // 1) Build if a manifest is provided.
            let mut built_pack: Option<LyquidPack> = if let Some(manifest) = manifest {
                let build_options = shaker::BuildOptions {
                    manifest: PathBuf::from(manifest),
                    target_dir: PathBuf::from("./lyquid_tools_target"),
                    debug,
                    is_bartender,
                };
                Some(shaker::build_lyquid(&build_options).await?)
            } else {
                None
            };

            // 2) Push built artifact to selected destination.
            if let Some(pack) = built_pack.take() {
                match &reference {
                    Some(push_reference) => {
                        if push_reference.reference().digest().is_some() {
                            anyhow::bail!(
                                "When deploying from a manifest with --reference, the reference must be tag-based because digest-pinned references cannot be used as push targets."
                            );
                        }

                        let reference_str = push_reference.to_string();
                        let oci = shaker::oci_registry_from_reference(
                            push_reference,
                            auth_token,
                            auth_username,
                            auth_password,
                        );
                        let hash = shaker::push_lyquid(pack, &oci, push_reference.reference()).await?;
                        let image_digest = LyquidPackDigest::new(hash).to_oci_digest();
                        tracing::info!("Pushed image hash {image_digest} to {reference_str}");

                        let pinned_reference = RegistryReference::new(
                            push_reference.reference().clone_with_digest(image_digest),
                            push_reference.protocol(),
                        );
                        tracing::info!("Deploying from pinned OCI reference {pinned_reference}");
                        reference = Some(pinned_reference);
                    }
                    None => {
                        let _ = shaker::push_lyquid_to_endpoint(pack.clone(), endpoint).await?;
                        tracing::info!("Pushed image to node endpoint {endpoint}");
                        built_pack = Some(pack);
                    }
                }
            }

            // 3) Deploy from registry (with repo_hint) or endpoint path (without repo_hint).
            let deployment = if let Some(reference) = reference {
                let oci = shaker::oci_registry_from_reference(&reference, auth_token, auth_username, auth_password);
                let (pinned, lazy) = oci
                    .pull_lazy_reference(&reference)
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                let repo_hint = pinned.repo_hint();
                shaker::deploy_lazy_lyquid(&lazy, &repo_hint, &deploy_options, &signer, &client).await?
            } else {
                let pack = built_pack.ok_or_else(|| {
                    anyhow::anyhow!("Internal error: missing built artifact for endpoint deployment.")
                })?;
                shaker::deploy_lyquid(pack, None, &deploy_options, &signer, &client).await?
            };
            let lyquid_id = deployment.lyquid_id;
            match output_format {
                "json" => {
                    println!("{}", serde_json::to_string(&deployment)?);
                }
                _ => {
                    println!("Status: {action}");
                    match lyquid_id {
                        Some(lyquid_id) => println!("Lyquid ID: {lyquid_id}"),
                        None => println!("Lyquid ID: pending"),
                    }
                    println!("Contract: {}", deployment.contract);
                    println!("Platform Version: {}", deployment.os_version);
                }
            }
            match lyquid_id {
                Some(lyquid_id) => tracing::info!("{action} {lyquid_id} => {}", deployment.contract),
                None => tracing::warn!(
                    "{action} deployment {} has no resolved Lyquid ID yet",
                    deployment.contract
                ),
            }
        }
        Some(("list", sub)) => {
            let hosted_only = sub.get_flag("hosted-only");
            let endpoint = sub.get_one::<String>("endpoint").unwrap();
            let output_format = sub.get_one::<String>("output").map_or("text", String::as_str);
            let image_digest_filter = sub
                .get_one::<String>("image-digest")
                .map(|digest| normalize_image_digest_filter(digest))
                .transpose()?;

            let (_, grpc_channel) = shaker::connect_grpc_api_channel(endpoint, "LyquidService").await?;
            let mut node_client = NodeServiceClient::new(grpc_channel.clone());
            if let Err(err) = warn_if_node_version_differs_with_client(&mut node_client, endpoint).await {
                tracing::debug!("Failed to check node version at {endpoint}: {err:#}");
            }
            let mut client = LyquidServiceClient::new(grpc_channel);
            let lyquid_ids = client
                .list_lyquids(ListLyquidsRequest { hosted_only })
                .await
                .with_context(|| format!("Failed to list Lyquids from `{endpoint}`"))?
                .into_inner()
                .lyquid_ids;

            if output_format == "text" && image_digest_filter.is_none() {
                for lyquid_id in lyquid_ids {
                    println!("{}", lyquid_id.value);
                }
                return Ok(());
            }

            let lyquids = collect_list_lyquid_json(&mut client, lyquid_ids, image_digest_filter.as_deref()).await?;
            if output_format == "json" {
                write_canonical_json(&ListJson { lyquids })?;
            } else {
                for lyquid in lyquids {
                    println!("{}", lyquid.lyquid_id);
                }
            }
        }
        Some(("console", sub)) => {
            let id = *sub.get_one::<LyquidID>("LYQUID_ID").unwrap();
            let endpoint = sub.get_one::<String>("endpoint").unwrap();
            let (_, grpc_channel) = shaker::connect_grpc_api_channel(endpoint, "LyquidService").await?;
            let mut node_client = NodeServiceClient::new(grpc_channel.clone());
            if let Err(err) = warn_if_node_version_differs_with_client(&mut node_client, endpoint).await {
                tracing::debug!("Failed to check node version at {endpoint}: {err:#}");
            }
            let mut client = LyquidServiceClient::new(grpc_channel);
            let mut stream = client
                .stream_console(StreamConsoleRequest {
                    lyquid_id: Some(id.into()),
                    sink: GrpcConsoleSink::Stdout as i32,
                    from_line_id: 0,
                })
                .await
                .with_context(|| format!("Failed to stream console for Lyquid `{id}`"))?
                .into_inner();

            let mut sigint = signal(SignalKind::interrupt()).context("Failed to register SIGINT handler")?;
            let mut sigterm = signal(SignalKind::terminate()).context("Failed to register SIGTERM handler")?;
            let mut next_line_id = 0_u64;
            let mut stdout = io::stdout();

            loop {
                tokio::select! {
                    _ = sigint.recv() => {
                        tracing::info!("received SIGINT");
                        break;
                    }
                    _ = sigterm.recv() => {
                        tracing::info!("received SIGTERM");
                        break;
                    }
                    chunk = stream.message() => {
                        let Some(chunk) = chunk.with_context(|| format!("Console stream failed for Lyquid `{id}`"))? else {
                            tracing::info!("console stream closed by server");
                            break;
                        };

                        if chunk.line_id > next_line_id {
                            tracing::warn!("{} lines of output not available", chunk.line_id - next_line_id);
                        }

                        for line in &chunk.lines {
                            stdout.write_all(line.as_bytes()).with_context(|| format!("Failed to write console output for Lyquid `{id}`"))?;
                        }
                        stdout.flush().with_context(|| format!("Failed to flush console output for Lyquid `{id}`"))?;

                        let lines_in_chunk = u64::try_from(chunk.lines.len())
                            .context("Console chunk line count exceeds u64 range")?;
                        next_line_id = chunk
                            .line_id
                            .checked_add(lines_in_chunk)
                            .context("Console line cursor overflow")?;
                    }
                }
            }
        }
        Some(("serve", sub)) => {
            let id = *sub.get_one::<LyquidID>("LYQUID_ID").unwrap();
            let endpoint = sub.get_one::<String>("endpoint").unwrap().clone();
            let listen = *sub.get_one::<std::net::SocketAddr>("listen").unwrap();
            let server = shaker::ServeServer::bind(shaker::ServeOptions {
                lyquid_id: id,
                endpoint,
                listen,
            })
            .await?;
            eprintln!(
                "proxying {} => {} @ {}",
                server.local_url(),
                server.virtual_host(),
                server.upstream_base()
            );

            let shutdown = tokio_util::sync::CancellationToken::new();
            let mut sigint = signal(SignalKind::interrupt()).context("Failed to register SIGINT handler")?;
            let mut sigterm = signal(SignalKind::terminate()).context("Failed to register SIGTERM handler")?;
            tokio::select! {
                result = server.run(shutdown.clone()) => result?,
                _ = sigint.recv() => {
                    tracing::info!("received SIGINT");
                    shutdown.cancel();
                }
                _ = sigterm.recv() => {
                    tracing::info!("received SIGTERM");
                    shutdown.cancel();
                }
            }
        }
        Some(("to-hex", sub)) => {
            let id = *sub.get_one::<LyquidID>("LYQUID_ID").unwrap();
            let address: Address = id.into();
            println!("{address}");
        }
        Some(("inspect", sub)) => {
            let path = sub.get_one::<String>("PACK_WASM_OR_OCI_REFERENCE").unwrap();
            let abi_json = sub.get_flag("abi-json");
            let output_format = sub.get_one::<String>("output").map_or("text", String::as_str);
            let (auth_token, auth_username, auth_password) = registry_auth_options(sub);
            const WASM_MAGIC: &[u8] = b"\0asm";
            match std::fs::read(path) {
                Ok(data) => {
                    let (pack, wasm) = if data.starts_with(WASM_MAGIC) {
                        (None, data)
                    } else {
                        let pack = LyquidPack::from_repo_bytes(&data)
                            .map_err(|e| anyhow::anyhow!("`{path}` is neither a WASM binary nor a Lyquid pack: {e}"))?;
                        let wasm = pack.wasm().to_vec();
                        (Some(pack), wasm)
                    };
                    let eth_abi = match &pack {
                        Some(pack) => pack.eth_abi().cloned().unwrap_or_else(JsonAbi::new),
                        None => lyquor_wasm::ethereum_json_abi_from_wasm(&wasm)
                            .context("Cannot extract Ethereum JSON ABI from the image")?,
                    };
                    if abi_json {
                        write_canonical_json(&eth_abi)?;
                        return Ok(());
                    }

                    let ldk_descriptor = lyquor_wasm::read_ldk_descriptor(&wasm);
                    let funcs = lyquor_wasm::extract_lyquid_functions_from_wasm(&wasm)
                        .context("Cannot extract functions from the image")?;

                    if output_format == "json" {
                        let mut inspect = InspectJson {
                            reference: None,
                            image_digest: None,
                            name: None,
                            platform: None,
                            platform_version: None,
                            ldk_descriptor: None,
                            ldk_version: None,
                            constructor_inputs: format_constructor_inputs(&eth_abi),
                            methods: inspect_methods_json(funcs),
                        };
                        if let Some(pack) = &pack {
                            inspect.image_digest = Some(pack.digest().to_oci_digest());
                            inspect.name = Some(pack.metadata().name.clone());
                            inspect.platform = Some(pack.metadata().os.clone());
                            inspect.platform_version = Some(pack.metadata().os_version.clone());
                        }
                        match &ldk_descriptor {
                            lyquor_wasm::LdkDescriptor::Version(version) => {
                                inspect.ldk_descriptor = Some("version");
                                inspect.ldk_version = Some(version.clone());
                            }
                            lyquor_wasm::LdkDescriptor::Unrecognized => {
                                inspect.ldk_descriptor = Some("unrecognized");
                            }
                            lyquor_wasm::LdkDescriptor::Absent => {
                                inspect.ldk_descriptor = Some("absent");
                            }
                        }
                        write_canonical_json(&inspect)?;
                        return Ok(());
                    }

                    if let Some(pack) = &pack {
                        println!("Name: {}", pack.metadata().name);
                        println!("Image Hash: {}", pack.digest().to_oci_digest());
                        println!("Platform: {} {}", pack.metadata().os, pack.metadata().os_version);
                    }
                    match ldk_descriptor {
                        lyquor_wasm::LdkDescriptor::Version(version) => println!("LDK Version: {version}"),
                        lyquor_wasm::LdkDescriptor::Unrecognized => println!("LDK Version: (unrecognized descriptor)"),
                        lyquor_wasm::LdkDescriptor::Absent => println!("LDK Version: (not recorded)"),
                    }
                    println!("Constructor Inputs: {}", format_constructor_inputs(&eth_abi));
                    println!("Methods:");
                    for func in funcs {
                        let mutability = if func.mutable { "mutable" } else { "immutable" };
                        let eth = match &func.eth {
                            Some(eth) => {
                                format!(" [eth {}{} -> {}]", hex::encode(eth.selector), eth.params, eth.returns)
                            }
                            None => String::new(),
                        };
                        let http = match &func.http {
                            Some(http) => format!(" [http {} {}]", http.method, http.path_prefix),
                            None => String::new(),
                        };
                        println!(
                            "  {:?}/{} {}::{}{eth}{http}",
                            func.category, mutability, func.group_hash, func.method
                        );
                    }
                }
                Err(read_err) => {
                    let reference = parse_deploy_reference(path).with_context(|| {
                        format!("Failed to read `{path}` as a local file ({read_err}) or parse it as an OCI reference")
                    })?;
                    let oci = shaker::oci_registry_from_reference(&reference, auth_token, auth_username, auth_password);
                    let (pinned, lazy) = oci
                        .pull_lazy_reference(&reference)
                        .await
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                    let eth_abi = lazy
                        .load_eth_abi()
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to load Ethereum ABI layer from registry: {e}"))?
                        .unwrap_or_else(JsonAbi::new);
                    if abi_json {
                        write_canonical_json(&eth_abi)?;
                        return Ok(());
                    }

                    if output_format == "json" {
                        write_canonical_json(&InspectJson {
                            reference: Some(reference.to_string()),
                            image_digest: Some(pinned.digest.to_oci_digest()),
                            name: Some(lazy.metadata().name.clone()),
                            platform: Some(lazy.metadata().os.clone()),
                            platform_version: Some(lazy.metadata().os_version.clone()),
                            ldk_descriptor: None,
                            ldk_version: None,
                            constructor_inputs: format_constructor_inputs(&eth_abi),
                            methods: Vec::new(),
                        })?;
                        return Ok(());
                    }

                    println!("Name: {}", lazy.metadata().name);
                    println!("Image Hash: {}", lazy.digest().to_oci_digest());
                    println!("Platform: {} {}", lazy.metadata().os, lazy.metadata().os_version);
                    println!("Constructor Inputs: {}", format_constructor_inputs(&eth_abi));
                }
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{node_image_digest_to_oci_digest, normalize_image_digest_filter};
    use lyquor_test::test;
    use tokio::net::TcpListener;

    #[test]
    fn image_digest_filter_accepts_oci_format() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let expected = format!("sha256:{hex}");

        assert_eq!(normalize_image_digest_filter(&expected).unwrap(), expected);
        assert!(normalize_image_digest_filter(&format!("0x{hex}")).is_err());
        assert!(normalize_image_digest_filter(hex).is_err());
        assert_eq!(
            node_image_digest_to_oci_digest(&format!("0x{hex}"), "Lyquid-test").unwrap(),
            expected
        );
    }

    #[test]
    fn grpc_console_endpoint_converts_supported_endpoint_schemes() {
        let cases = [
            ("ws://127.0.0.1:10087/ws", "http://127.0.0.1:10087/"),
            ("wss://lyquor.example/ws", "https://lyquor.example/"),
            ("http://localhost:10087/api", "http://localhost:10087/"),
            ("https://localhost:10087/api", "https://localhost:10087/"),
        ];
        for (input, expected) in cases {
            assert_eq!(shaker::grpc_api_endpoint(input).unwrap(), expected);
        }
    }

    #[test(tokio::test)]
    async fn grpc_channel_endpoint_enables_tonic_https_transport() -> anyhow::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let accept = tokio::spawn(async move {
            let _ = listener.accept().await?;
            anyhow::Ok(())
        });

        let (_, endpoint) = shaker::grpc_api_channel_endpoint(&format!("https://{addr}/api"))?;
        let err = endpoint
            .connect()
            .await
            .expect_err("test listener closes before completing a TLS handshake");
        accept.await??;

        let msg = format!("{err:#}");
        assert!(
            !msg.contains("Connecting to HTTPS without TLS enabled"),
            "HTTPS gRPC endpoint did not have tonic TLS enabled: {msg}"
        );
        Ok(())
    }

    #[test]
    fn node_version_comparison_matches_release_and_dev_policy() {
        assert!(
            !shaker::node_versions_differ("0.1.1+shaker", "0.1.1+node"),
            "release builds should ignore build metadata"
        );
        assert!(
            !shaker::node_versions_differ("0.1.1-dev+abc1234", "0.1.1-dev+abc1234"),
            "matching dev build strings should not warn"
        );
        assert!(
            shaker::node_versions_differ("0.1.1-dev+abc1234", "0.1.1-dev+def5678"),
            "dev builds should require exact build identity"
        );
        assert!(
            shaker::node_versions_differ("0.1.1", "0.1.2"),
            "release patch mismatches should warn"
        );
        assert!(
            shaker::node_versions_differ("0.1.1", "0.2.0"),
            "release minor mismatches should warn"
        );
        assert!(
            !shaker::node_versions_differ("not-semver", "not-semver"),
            "matching unparsable versions should not warn"
        );
        assert!(
            shaker::node_versions_differ("not-semver", "other-not-semver"),
            "unparsable versions should use exact comparison"
        );
    }
}
