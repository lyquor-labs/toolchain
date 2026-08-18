use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use alloy_sol_types::{SolCall, sol};
use anyhow::Context;
use lyquor_eth::{EthSubmitter, Signer};
use lyquor_jsonrpc::types::{
    BlockNumber, EthCall, EthCallResp, EthCallTx, EthGetTransactionReceipt, EthGetTransactionReceiptResp,
};
use lyquor_primitives::alloy_primitives::B256;
use lyquor_primitives::oracle::OracleConfig;
use lyquor_primitives::{Address, NodeID, U256, decode_object};

use crate::Client;

const AVAILABILITY_TOPIC: &str = "availability";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(200);
const DEFAULT_POLL_TIMEOUT: Duration = Duration::from_secs(60);
const COMMITTEE_TIMEOUT_HINT: &str = "a committee node may be offline, the threshold may be unreachable, or a committee node may be missing its Ed25519 key";

sol! {
    interface BartenderAvailability {
        function __lyquor_oracle_initialize(
            string topic,
            address targetAddr,
            bool isEvm,
            bytes32[] committee,
            uint16 threshold
        ) external;
        function __lyquor_oracle_advance_epoch(
            string topic,
            address targetAddr,
            bool isEvm
        ) external returns (bool);
        function __lyquor_oracle_finalize_epoch(
            string topic,
            address targetAddr,
            bool isEvm
        ) external returns (bool);
        function __lyquor_oracle_dest_epoch_info(
            string topic,
            bool fullConfig
        ) external returns (uint64 epoch, bytes32 configHash, uint32 changeCount, bytes config);
        function get_availability_epoch() external returns (uint32);
        function get_availability_counts() external returns (uint64 admittedImages, uint64 pendingDeployments);
    }
}

/// Observable availability-gate state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AvailabilityStatus {
    pub source_epoch: u32,
    pub dest_epoch: u32,
    pub committee: Vec<NodeID>,
    pub threshold: u16,
    pub admitted_image_count: u64,
    pub pending_deployment_count: u64,
}

#[derive(Debug)]
struct EpochState {
    source_epoch: u32,
    dest_epoch: u32,
    config: OracleConfig,
}

async fn eth_call<C: SolCall>(client: &Client, bartender: Address, call: C) -> anyhow::Result<C::Return> {
    let response: EthCallResp = client
        .request(EthCall {
            tx: EthCallTx {
                from: None,
                to: Some(bartender),
                gas: None,
                gas_price: None,
                value: None,
                data: Some(call.abi_encode().into()),
            },
            block_number: BlockNumber::Latest,
        })
        .await
        .context("Availability call failed")?;
    C::abi_decode_returns(&response.0).context("Failed to decode availability call response")
}

async fn epoch_state(client: &Client, bartender: Address) -> anyhow::Result<EpochState> {
    let (source_epoch, dest) = tokio::try_join!(
        eth_call(client, bartender, BartenderAvailability::get_availability_epochCall {}),
        eth_call(
            client,
            bartender,
            BartenderAvailability::__lyquor_oracle_dest_epoch_infoCall {
                topic: AVAILABILITY_TOPIC.to_owned(),
                fullConfig: true,
            },
        )
    )?;
    let dest_epoch = u32::try_from(dest.epoch).context("Availability destination epoch does not fit in u32")?;
    let config = if dest.config.is_empty() {
        OracleConfig {
            committee: Vec::new(),
            threshold: 0,
        }
    } else {
        decode_object(&dest.config).context("Bartender returned an invalid availability committee config")?
    };
    Ok(EpochState {
        source_epoch,
        dest_epoch,
        config,
    })
}

async fn poll<T, F, Fut>(timeout_message: impl Into<String>, mut operation: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<Option<T>>>,
{
    let timeout_message = timeout_message.into();
    tokio::time::timeout(DEFAULT_POLL_TIMEOUT, async {
        loop {
            if let Some(value) = operation().await? {
                return Ok(value);
            }
            tokio::time::sleep(DEFAULT_POLL_INTERVAL).await;
        }
    })
    .await
    .map_err(|_| anyhow::Error::msg(timeout_message))?
}

async fn wait_for_transaction(client: &Client, tx_hash: B256) -> anyhow::Result<()> {
    let receipt = poll(
        format!("Timed out waiting for availability initialization transaction {tx_hash} to be sequenced"),
        || async {
            let receipt: EthGetTransactionReceiptResp = client.request(EthGetTransactionReceipt(tx_hash)).await?;
            Ok(receipt)
        },
    )
    .await?;
    if receipt.status != U256::from(1_u8) {
        anyhow::bail!(
            "Availability initialization transaction {tx_hash} failed with status {}",
            receipt.status
        );
    }
    Ok(())
}

fn validate_committee(committee: &[NodeID], threshold: u16) -> anyhow::Result<()> {
    if committee.is_empty() {
        anyhow::bail!("Availability committee cannot be empty");
    }
    if threshold == 0 || usize::from(threshold) > committee.len() {
        anyhow::bail!(
            "Availability threshold {} must be between 1 and the committee size {}",
            threshold,
            committee.len()
        );
    }
    let mut unique = committee.to_vec();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() != committee.len() {
        anyhow::bail!("Availability committee contains duplicate node IDs");
    }
    Ok(())
}

/// Complete bartender's first availability epoch.
pub async fn activate<S: Signer + Clone + Send + Sync + 'static>(
    client: &Client, signer: &S, bartender: Address, target: Address, committee: &[NodeID], threshold: u16,
) -> anyhow::Result<AvailabilityStatus> {
    validate_committee(committee, threshold)?;

    let call = BartenderAvailability::__lyquor_oracle_initializeCall {
        topic: AVAILABILITY_TOPIC.to_owned(),
        targetAddr: target,
        isEvm: false,
        committee: committee.iter().map(|id| <[u8; 32]>::from(*id).into()).collect(),
        threshold,
    };
    let submitter = EthSubmitter::new(client.clone(), Arc::new(signer.clone()));
    let tx_hash = submitter
        .submit_contract_call(bartender, call.abi_encode().into())
        .await
        .context("Failed to submit availability committee initialization")?;
    wait_for_transaction(client, tx_hash).await?;

    poll(
        format!("Timed out submitting the availability epoch advance; {COMMITTEE_TIMEOUT_HINT}"),
        || async {
            let advanced = eth_call(
                client,
                bartender,
                BartenderAvailability::__lyquor_oracle_advance_epochCall {
                    topic: AVAILABILITY_TOPIC.to_owned(),
                    targetAddr: target,
                    isEvm: false,
                },
            )
            .await?;
            Ok(advanced.then_some(()))
        },
    )
    .await?;
    let mut state = poll(
        format!("Timed out waiting for availability destination epoch 1; {COMMITTEE_TIMEOUT_HINT}"),
        || async {
            let state = epoch_state(client, bartender).await?;
            Ok((state.dest_epoch >= 1).then_some(state))
        },
    )
    .await?;

    let finalized = eth_call(
        client,
        bartender,
        BartenderAvailability::__lyquor_oracle_finalize_epochCall {
            topic: AVAILABILITY_TOPIC.to_owned(),
            targetAddr: target,
            isEvm: false,
        },
    )
    .await?;
    if !finalized {
        anyhow::bail!("Bartender did not accept availability epoch finalization");
    }
    let expected = state.dest_epoch;
    state = poll(
        format!("Timed out waiting for availability source epoch {expected}; {COMMITTEE_TIMEOUT_HINT}"),
        || async {
            let state = epoch_state(client, bartender).await?;
            Ok((state.source_epoch >= expected).then_some(state))
        },
    )
    .await?;

    if state.source_epoch == 0 || state.source_epoch != state.dest_epoch {
        anyhow::bail!(
            "Availability ceremony did not settle: source epoch {}, destination epoch {}",
            state.source_epoch,
            state.dest_epoch
        );
    }

    let status = status_from_state(client, bartender, state).await?;
    if status.committee != committee || status.threshold != threshold {
        anyhow::bail!(
            "Availability committee is already active with a different configuration; active committee {:?}, threshold {}",
            status.committee,
            status.threshold
        );
    }
    Ok(status)
}

/// Read bartender's availability committee and deployment-admission status.
pub async fn status(client: &Client, bartender: Address) -> anyhow::Result<AvailabilityStatus> {
    let state = epoch_state(client, bartender).await?;
    status_from_state(client, bartender, state).await
}

async fn status_from_state(
    client: &Client, bartender: Address, state: EpochState,
) -> anyhow::Result<AvailabilityStatus> {
    let counts = eth_call(client, bartender, BartenderAvailability::get_availability_countsCall {}).await?;
    let committee = state
        .config
        .committee
        .into_iter()
        .map(|signer| {
            NodeID::try_from(signer.key.as_ref())
                .map_err(|err| anyhow::anyhow!("Invalid node ID in availability committee: {err:?}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(AvailabilityStatus {
        source_epoch: state.source_epoch,
        dest_epoch: state.dest_epoch,
        committee,
        threshold: state.config.threshold,
        admitted_image_count: counts.admittedImages,
        pending_deployment_count: counts.pendingDeployments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lyquor_test::test;

    #[test]
    fn activation_rejects_invalid_committee_thresholds_and_duplicates() {
        let node = NodeID::from(1);
        assert!(validate_committee(&[], 1).is_err());
        assert!(validate_committee(&[node], 0).is_err());
        assert!(validate_committee(&[node], 2).is_err());
        assert!(validate_committee(&[node, node], 1).is_err());
        assert!(validate_committee(&[node], 1).is_ok());
    }
}
