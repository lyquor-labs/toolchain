use std::{collections::BTreeMap, sync::Arc};

use alloy_dyn_abi::{DynSolValue, JsonAbiExt};
use alloy_json_abi::JsonAbi;
use anyhow::Context;
use lyquor_eth::{EthSubmitter, Signer};
use lyquor_oci::pack::{LazyLyquidPack, LyquidPack, LyquidPackDigest};
use lyquor_primitives::alloy_primitives::FixedBytes;
use lyquor_primitives::{Address, Bytes, LyquidID, alloy_primitives, hex};
use lyquor_proto::lyquid::v1::{GetLyquidByAddressRequest, lyquid_service_client::LyquidServiceClient};

use crate::{Client, format_abi_param_tuple};

/// User-supplied deployment knobs for Lyquid EVM deployment.
#[derive(Debug, Clone, Default)]
pub struct DeployOptions {
    pub bartender: Option<Address>,
    pub superseded: Option<Address>,
    pub input: Option<alloy_primitives::Bytes>,
}

/// Result of deploying a Lyquid contract and optionally resolving its Lyquid ID.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LyquidDeployment {
    pub contract: Address,
    pub lyquid_id: Option<LyquidID>,
    /// Lyquor platform ("OS") version recorded in the pack metadata.
    pub os_version: String,
}

struct ResolvedLyquidDeployArtifact<'a> {
    name: String,
    evm_deployment: Bytes,
    evm_auxiliary: Option<BTreeMap<String, Bytes>>,
    eth_abi: Option<JsonAbi>,
    os_version: String,
    hash: FixedBytes<32>,
    repo_hint: Option<&'a str>,
}

fn prepare_constructor_input(
    name: &str, eth_abi: Option<&JsonAbi>, input: Option<&alloy_primitives::Bytes>,
) -> anyhow::Result<Option<Vec<DynSolValue>>> {
    match (input, eth_abi.and_then(|abi| abi.constructor.as_ref())) {
        (Some(input), Some(constructor)) => {
            let decoded = constructor.abi_decode_input(input.as_ref()).with_context(|| {
                format!(
                    "Input does not match the constructor inputs: {}",
                    format_abi_param_tuple(&constructor.inputs)
                )
            })?;
            Ok(Some(decoded))
        }
        (Some(_), None) => Err(anyhow::anyhow!(
            "Constructor input was provided, but Lyquid `{name}` has no constructor entry in its Ethereum ABI."
        )),
        (None, Some(constructor)) => {
            let constructor_params = constructor.inputs.len();
            if constructor_params == 0 {
                Ok(None)
            } else {
                let noun = if constructor_params == 1 {
                    "argument"
                } else {
                    "arguments"
                };
                anyhow::bail!(
                    "Missing constructor input for Lyquid `{name}`. Constructor inputs `{}` require {constructor_params} {noun}. Set DeployOptions::input, or pass `--input <HEX>` in `shaker deploy`, with Ethereum ABI-encoded constructor arguments.",
                    format_abi_param_tuple(&constructor.inputs)
                );
            }
        }
        (None, None) => Ok(None),
    }
}

async fn deploy_bartender_shared_libraries(
    submitter: &EthSubmitter, evm_auxiliary: Option<BTreeMap<String, Bytes>>,
) -> anyhow::Result<(Address, Address)> {
    let Some(evm_auxiliary) = evm_auxiliary else {
        anyhow::bail!("Missing EVM auxiliary bytecodes for bartender");
    };
    let oracle_bytecode = evm_auxiliary.get("oracle").context("Missing Oracle bytecode")?.clone();
    let ed25519_bytecode = evm_auxiliary
        .get("SCL_EIP6565")
        .context("Missing Ed25519 bytecode")?
        .clone();

    let oracle = submitter.deploy_contract(oracle_bytecode.into()).await?;
    let ed25519 = submitter.deploy_contract(ed25519_bytecode.into()).await?;
    Ok((oracle, ed25519))
}

/// Deploys a fully materialized Lyquid pack through the supplied signer and node client.
pub async fn deploy_lyquid<S: Signer + Clone + Send + Sync + 'static>(
    pack: LyquidPack, repo_hint: Option<&str>, options: &DeployOptions, signer: &S, client: &Client,
) -> anyhow::Result<LyquidDeployment> {
    deploy_lyquid_with_resolved_artifact(
        ResolvedLyquidDeployArtifact {
            name: pack.metadata().name.clone(),
            evm_deployment: pack.evm_deployment_bytecode().clone(),
            evm_auxiliary: pack.evm_auxiliary_bytecodes().cloned(),
            eth_abi: pack.eth_abi().cloned(),
            os_version: pack.metadata().os_version.clone(),
            hash: *pack.digest().digest(),
            repo_hint,
        },
        options,
        signer,
        client,
    )
    .await
}

/// Loads deployment bytecodes from a lazy pack and deploys the Lyquid.
pub async fn deploy_lazy_lyquid<S: Signer + Clone + Send + Sync + 'static>(
    pack: &LazyLyquidPack, repo_hint: &str, options: &DeployOptions, signer: &S, client: &Client,
) -> anyhow::Result<LyquidDeployment> {
    let (evm_deployment, evm_auxiliary) = pack
        .load_evm_bytecodes()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to load EVM deployment bytecode layer from registry: {e}"))?;
    let eth_abi = pack
        .load_eth_abi()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to load Ethereum ABI layer from registry: {e}"))?;
    deploy_lyquid_with_resolved_artifact(
        ResolvedLyquidDeployArtifact {
            name: pack.metadata().name.clone(),
            evm_deployment,
            evm_auxiliary,
            eth_abi,
            os_version: pack.metadata().os_version.clone(),
            hash: *pack.digest().digest(),
            repo_hint: Some(repo_hint),
        },
        options,
        signer,
        client,
    )
    .await
}

async fn deploy_lyquid_with_resolved_artifact<S: Signer + Clone + Send + Sync + 'static>(
    artifact: ResolvedLyquidDeployArtifact<'_>, options: &DeployOptions, signer: &S, jsonrpc_client: &Client,
) -> anyhow::Result<LyquidDeployment> {
    let ResolvedLyquidDeployArtifact {
        name,
        evm_deployment,
        evm_auxiliary,
        eth_abi,
        os_version,
        hash,
        repo_hint,
    } = artifact;
    let submitter = EthSubmitter::new(jsonrpc_client.clone(), Arc::new(signer.clone()));
    tracing::info!("{name}.image_hash = {}", LyquidPackDigest::new(hash).to_oci_digest());
    tracing::info!("{name}.os_version = {os_version}");
    let constructor_input = prepare_constructor_input(&name, eth_abi.as_ref(), options.input.as_ref())?;
    let superseded = options.superseded.unwrap_or(Address::ZERO);
    let mut eth_input = vec![
        DynSolValue::Address(options.bartender.unwrap_or(Address::ZERO)),
        DynSolValue::Address(superseded),
        DynSolValue::FixedBytes(<[u8; 32]>::from(hash).into(), 32),
        DynSolValue::String(repo_hint.unwrap_or("").to_owned()),
    ];
    if options.bartender.is_none() {
        let (oracle_library, ed25519_library) = deploy_bartender_shared_libraries(&submitter, evm_auxiliary).await?;
        tracing::info!("{name}.oracle_library = {oracle_library}");
        tracing::info!("{name}.ed25519_library = {ed25519_library}");
        eth_input.push(DynSolValue::Address(oracle_library));
        eth_input.push(DynSolValue::Address(ed25519_library));
    }
    let mut deps: Vec<Address> = Vec::new();

    if let Some(constructor_input) = constructor_input {
        for val in &constructor_input {
            if let DynSolValue::Address(addr) = val {
                tracing::info!("input address is {:?}", addr);
                deps.push(*addr);
            }
        }
        eth_input.push(DynSolValue::Array(deps.into_iter().map(DynSolValue::Address).collect()));
        eth_input.extend(constructor_input);
    } else {
        eth_input.push(DynSolValue::Array(vec![]));
    }
    let mut data: Vec<u8> = evm_deployment.into();
    let eth_input = DynSolValue::Tuple(eth_input).abi_encode_params();
    data.extend(&eth_input);
    tracing::info!("{name}.input = {}", hex::encode(eth_input));
    let addr = submitter.deploy_contract(data.into()).await?;
    tracing::info!("{name}.address = {addr}");
    let lyquid_id = wait_for_lyquid_id(addr, jsonrpc_client).await?;
    if let Some(lyquid_id) = lyquid_id {
        tracing::info!("{name}.lyquid_id = {lyquid_id}");
    } else {
        tracing::warn!("{name}.lyquid_id was not resolved within the timeout");
    }
    Ok(LyquidDeployment {
        contract: addr,
        lyquid_id,
        os_version,
    })
}

async fn wait_for_lyquid_id(addr: Address, jsonrpc_client: &Client) -> anyhow::Result<Option<LyquidID>> {
    let (_, channel) = crate::connect_grpc_api_channel(jsonrpc_client.endpoint_url().as_str(), "LyquidService").await?;
    let mut grpc_client = LyquidServiceClient::new(channel);

    tokio::time::timeout(tokio::time::Duration::from_secs(15), async {
        loop {
            let resp = grpc_client
                .get_lyquid_by_address(GetLyquidByAddressRequest {
                    address: Some(addr.into()),
                })
                .await
                .map_err(anyhow::Error::from)?
                .into_inner();
            if let Some(proto_id) = resp.lyquid_id {
                let id = proto_id
                    .try_into()
                    .map_err(|err| anyhow::anyhow!("Invalid Lyquid ID returned by GetLyquidByAddress: ({err})"))?;
                return Ok::<Option<LyquidID>, anyhow::Error>(Some(id))
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        tracing::warn!("Timed out waiting for Lyquid ID for deployed contract {addr}");
        Ok(None)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_json_abi::{Constructor, Param, StateMutability};
    use lyquor_test::test;

    fn eth_abi_with_constructor(inputs: &[&str]) -> JsonAbi {
        JsonAbi {
            constructor: Some(Constructor {
                inputs: inputs.iter().map(|ty| param(ty)).collect(),
                state_mutability: StateMutability::NonPayable,
            }),
            ..Default::default()
        }
    }

    fn param(ty: &str) -> Param {
        Param::new("", ty, Vec::new(), None).expect("test constructor input type should be valid")
    }

    #[test]
    fn missing_input_is_allowed_for_empty_constructor_inputs() {
        assert!(
            prepare_constructor_input("empty", Some(&eth_abi_with_constructor(&[])), None)
                .expect("empty constructor should not require input")
                .is_none()
        );
    }

    #[test]
    fn missing_input_reports_constructor_shape_before_deploying() {
        let err = prepare_constructor_input("hello", Some(&eth_abi_with_constructor(&["string", "address[]"])), None)
            .expect_err("non-empty constructor inputs should require input");
        let msg = format!("{err:#}");

        assert!(
            msg.contains("Missing constructor input for Lyquid `hello`"),
            "got: {msg}"
        );
        assert!(
            msg.contains("Constructor inputs `(string,address[])` require 2 arguments"),
            "got: {msg}"
        );
        assert!(msg.contains("--input <HEX>"), "got: {msg}");
    }

    #[test]
    fn supplied_input_is_decoded_against_constructor_inputs() {
        let input = alloy_primitives::Bytes::from(
            DynSolValue::Tuple(vec![DynSolValue::String("hi".to_owned())]).abi_encode_params(),
        );
        let decoded = prepare_constructor_input("hello", Some(&eth_abi_with_constructor(&["string"])), Some(&input))
            .expect("valid constructor input should decode")
            .expect("constructor input should be present");

        assert_eq!(decoded, vec![DynSolValue::String("hi".to_owned())]);
    }

    #[test]
    fn supplied_input_requires_constructor_entry() {
        let input = alloy_primitives::Bytes::from(Vec::new());
        let err = prepare_constructor_input("hello", None, Some(&input))
            .expect_err("constructor input without constructor entry should fail");
        let msg = format!("{err:#}");

        assert!(
            msg.contains("has no constructor entry in its Ethereum ABI"),
            "got: {msg}"
        );
    }
}
