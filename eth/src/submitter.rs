use alloy_sol_types::{SolCall, SolValue};
use anyhow::Context as _Context;
use lyquor_primitives::alloy_primitives::{self as alloy_primitives, B256, Bytes as AlloyBytes};
use lyquor_primitives::oracle::{OracleCert, OracleServiceTarget};
use lyquor_primitives::{Address, CallParams, InputABI, U256};
use std::future::Future;
use std::sync::Arc;
use thiserror::Error;
use tokio::time::Duration;

use super::Signer;
use lyquor_jsonrpc::client::{ClientHandle, Error as JsonRpcError};
use lyquor_jsonrpc::types::{
    BlockNumber, EthEstimateGas, EthEstimateGasResp, EthGasPrice, EthGasPriceResp, EthGetChainId, EthGetChainIdResp,
    EthGetTransactionCount, EthGetTransactionCountResp, EthGetTransactionReceipt, EthGetTransactionReceiptResp,
    EthNewHeadUpdate, EthSendRawTransaction, EthSendTransactionResp, EthSubscribe, Hash,
};

/// Errors returned by Ethereum transaction submission and deployment helpers.
#[derive(Debug, Error)]
pub enum EthSubmitterError {
    #[error("Ethereum client error: {0}")]
    Client(#[from] JsonRpcError),
    #[error("Deployment error for {tx_hash}: {reason}")]
    Deploy { tx_hash: Hash, reason: String },
    #[error("Other: {0}")]
    Other(#[from] anyhow::Error),
}

impl From<CallParams> for crate::eth::CallParams {
    fn from(call: CallParams) -> Self {
        Self {
            origin: call.origin,
            caller: call.caller,
            group: call.group,
            method: call.method,
            input: call.input.into(),
            abi_: match call.abi {
                InputABI::Eth => crate::eth::ABI::Eth,
                InputABI::Lyquor => crate::eth::ABI::Lyquor,
            },
        }
    }
}

/// Ethereum transaction submitter backed by a JSON-RPC client.
///
/// Handles raw signed transactions, Lyquor calls routed through the sequence backend contract,
/// and contract deployments.
pub struct EthSubmitter {
    client: ClientHandle,
    signer: Arc<dyn Signer + Send + Sync>,
}

impl EthSubmitter {
    /// Build a submitter for locally signed transactions.
    pub fn new(client: ClientHandle, signer: Arc<dyn Signer + Send + Sync>) -> Self {
        Self { client, signer }
    }

    fn encode_submit_certified_calls(call: CallParams) -> AlloyBytes {
        crate::eth::ISequenceBackend::__lyquor_submit_certified_callsCall {
            calls: vec![call.into()],
        }
        .abi_encode()
        .into()
    }

    fn deploy_error(tx_hash: Hash, reason: impl Into<String>) -> EthSubmitterError {
        EthSubmitterError::Deploy {
            tx_hash,
            reason: reason.into(),
        }
    }

    fn is_retryable_nonce_send_error(err: &JsonRpcError) -> bool {
        let matches_retryable_nonce_error = |message: &str| {
            let message = message.to_ascii_lowercase();
            ["nonce too low", "replacement transaction underpriced"]
                .iter()
                .any(|needle| message.contains(needle))
        };

        match err {
            JsonRpcError::Call(err) => {
                matches_retryable_nonce_error(err.message()) ||
                    err.data().is_some_and(|data| matches_retryable_nonce_error(data.get()))
            }
            JsonRpcError::RestartNeeded(err) => Self::is_retryable_nonce_send_error(err),
            _ => false,
        }
    }

    async fn sign_and_send_pending_nonce_transaction<F, Fut>(
        &self, address: Address, mut sign: F,
    ) -> Result<Hash, EthSubmitterError>
    where
        F: FnMut(u64) -> Fut,
        Fut: Future<Output = Result<AlloyBytes, EthSubmitterError>>,
    {
        let mut retried_nonce_send_error = false;
        loop {
            let pending: EthGetTransactionCountResp = self
                .client
                .request(EthGetTransactionCount {
                    address,
                    block_number: BlockNumber::Pending,
                })
                .await?;
            let nonce = pending.0.to();
            let raw_tx = sign(nonce).await?;

            match self
                .client
                .request::<_, EthSendTransactionResp>(EthSendRawTransaction(raw_tx))
                .await
            {
                Ok(sent) => return Ok(sent.0),
                Err(err) if !retried_nonce_send_error && Self::is_retryable_nonce_send_error(&err) => {
                    retried_nonce_send_error = true;
                    tracing::debug!(
                        %address,
                        nonce,
                        "Ethereum raw transaction nonce send error; refreshing pending nonce and retrying"
                    );
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    async fn get_deploy_address(&self, tx_hash: Hash) -> Result<Option<Address>, EthSubmitterError> {
        let receipt: EthGetTransactionReceiptResp = self.client.request(EthGetTransactionReceipt(tx_hash)).await?;
        let Some(receipt) = receipt else {
            return Ok(None);
        };

        if receipt.status != U256::from(1_u8) {
            return Err(Self::deploy_error(
                tx_hash,
                format!("receipt has failure status {}", receipt.status),
            ));
        }

        receipt
            .contract_address
            .map(Some)
            .ok_or_else(|| Self::deploy_error(tx_hash, "receipt is missing contract_address"))
    }

    async fn wait_for_deploy_address(&self, tx_hash: Hash) -> Result<Address, EthSubmitterError> {
        let deploy_receipt_timeout = Duration::from_secs(30);
        let mut subscription = self
            .client
            .subscribe::<EthSubscribe, EthNewHeadUpdate>(EthSubscribe("newHeads".to_owned()))
            .await?;

        let receipt_wait = async {
            // Subscribe before the first receipt query so a mined-in-between block is
            // observed either by the immediate query below or by the next head notification.
            if let Some(address) = self.get_deploy_address(tx_hash).await? {
                return Ok(address);
            }

            loop {
                match subscription.next().await {
                    Some(_) => {
                        if let Some(address) = self.get_deploy_address(tx_hash).await? {
                            return Ok(address);
                        }
                    }
                    None => {
                        return Err(Self::deploy_error(
                            tx_hash,
                            "newHeads subscription closed while waiting for receipt",
                        ));
                    }
                }
            }
        };

        tokio::time::timeout(deploy_receipt_timeout, receipt_wait)
            .await
            .map_err(|_| Self::deploy_error(tx_hash, "timed out waiting for receipt"))?
    }

    async fn send_call(&self, to: Address, input: AlloyBytes) -> Result<Hash, EthSubmitterError> {
        let signer = self.signer.as_ref();
        let submitter_from = signer.address();
        let chain_id: EthGetChainIdResp = self.client.request(EthGetChainId).await?;
        let gas_price: EthGasPriceResp = self.client.request(EthGasPrice).await?;
        let gas_limit: EthEstimateGasResp = self
            .client
            .request(EthEstimateGas {
                from: Some(submitter_from),
                to: Some(to),
                gas: None,
                gas_price: Some(gas_price.0),
                value: Some(U256::ZERO),
                data: Some(input.clone()),
            })
            .await?;

        let chain_id = chain_id.0.to();
        let gas_price = gas_price.0.to();
        let gas_limit = gas_limit.0.to();
        self.sign_and_send_pending_nonce_transaction(submitter_from, |nonce| {
            let input = input.clone();
            async move {
                sign_legacy_transaction(
                    signer,
                    alloy_consensus::TxLegacy {
                        nonce,
                        gas_price,
                        gas_limit,
                        to: alloy_primitives::TxKind::Call(to),
                        value: U256::ZERO,
                        input,
                        chain_id: Some(chain_id),
                    },
                )
                .await
                .context("Failed to sign the transaction.")
                .map_err(EthSubmitterError::Other)
            }
        })
        .await
    }

    /// Submit an already-signed raw Ethereum transaction.
    pub async fn submit_sender_call(&self, raw_tx: AlloyBytes) -> Result<Hash, EthSubmitterError> {
        let sent: EthSendTransactionResp = self.client.request(EthSendRawTransaction(raw_tx)).await?;
        Ok(sent.0)
    }

    /// Submit a Lyquor call through the sequence backend contract, translating certified EVM calls.
    pub async fn submit_certified_call(&self, to: Address, call: CallParams) -> Result<Hash, EthSubmitterError> {
        let mut call = call;
        let Some(envelope) = lyquor_primitives::decode_by_fields!(
            call.input.as_ref(),
            cert: OracleCert,
            input_raw: lyquor_primitives::Bytes
        ) else {
            return self.send_call(to, Self::encode_submit_certified_calls(call)).await;
        };
        let OracleServiceTarget::EVM { target, eth_contract } = envelope.cert.header.target.target else {
            return self.send_call(to, Self::encode_submit_certified_calls(call)).await;
        };

        let group = call.group.clone();
        let is_epoch_advance = call.origin == Address::ZERO &&
            call.abi == InputABI::Lyquor &&
            group == "oracle::internal" &&
            call.method == "__lyquor_oracle_on_epoch_advance";

        let (input_raw, topic_str, has_config_delta) = if is_epoch_advance {
            let payload = lyquor_primitives::decode_by_fields!(
                envelope.input_raw.as_ref(),
                topic: String,
                config_delta: lyquor_primitives::oracle::OracleConfigDelta,
                change_count: u32
            )
            .ok_or_else(|| anyhow::anyhow!("Missing or invalid epoch advance payload."))?;
            let config_delta = lyquor_primitives::oracle::eth::OracleConfigDelta::try_from(payload.config_delta)
                .map_err(|_| {
                    anyhow::anyhow!("Invalid oracle signer key length in config delta (expected 32 bytes).")
                })?;
            let has_config_delta =
                config_delta.thresholdChanged || !config_delta.upsert.is_empty() || !config_delta.remove.is_empty();
            let config_delta = crate::eth::OracleConfigDelta {
                upsert: config_delta
                    .upsert
                    .into_iter()
                    .map(|signer| crate::eth::OracleSigner {
                        id: signer.id,
                        nodeID: signer.nodeID,
                    })
                    .collect(),
                remove: config_delta.remove,
                thresholdChanged: config_delta.thresholdChanged,
                threshold: config_delta.threshold,
            };
            (
                (config_delta, payload.change_count).abi_encode_params().into(),
                payload.topic,
                has_config_delta,
            )
        } else {
            (
                AlloyBytes::from(envelope.input_raw.to_vec()),
                group
                    .split_once("::")
                    .map_or(group.as_str(), |(topic, _)| topic)
                    .to_string(),
                false,
            )
        };

        let cfg_hash: B256 = B256::from_slice(envelope.cert.header.config_hash.as_bytes());
        let seq_id: B256 = <[u8; 32]>::from(envelope.cert.header.target.seq_id).into();
        let topic = alloy_primitives::keccak256(topic_str.as_bytes());
        let group_hash = alloy_primitives::keccak256(group.as_bytes());
        let nonce: B256 = <lyquor_primitives::HashBytes as Into<[u8; 32]>>::into(envelope.cert.header.nonce).into();
        let signatures: Vec<AlloyBytes> = envelope.cert.signatures.iter().map(|sig| sig.clone().into()).collect();

        let oc = crate::eth::OracleCert {
            header: crate::eth::OracleHeader {
                proposer: <[u8; 32]>::from(envelope.cert.header.proposer).into(),
                topic,
                group: group_hash,
                target,
                seqId: seq_id,
                ethContract: eth_contract,
                configHash: cfg_hash,
                epoch: envelope.cert.header.epoch,
                nonce,
            },
            hasConfigDelta: has_config_delta,
            signers: envelope.cert.signers,
            signatures,
        };

        call.input = (input_raw, oc).abi_encode_params().into();
        self.send_call(to, Self::encode_submit_certified_calls(call)).await
    }

    /// Deploy a contract creation transaction and return the created contract address.
    pub async fn deploy_contract(&self, data: AlloyBytes) -> Result<Address, EthSubmitterError> {
        let signer = self.signer.as_ref();
        let address = signer.address();
        let chain_id: EthGetChainIdResp = self.client.request(EthGetChainId).await?;
        let gas_price: EthGasPriceResp = self.client.request(EthGasPrice).await?;
        let gas_price_val = gas_price.0;
        let zero_value = U256::ZERO;
        let gas_limit: EthEstimateGasResp = self
            .client
            .request(EthEstimateGas {
                from: Some(address),
                to: None,
                gas: None,
                gas_price: Some(gas_price_val),
                value: Some(zero_value),
                data: Some(data.clone()),
            })
            .await?;

        let chain_id = chain_id.0.to();
        let gas_price = gas_price.0.to();
        let gas_limit = gas_limit.0.to();
        let tx_hash = self
            .sign_and_send_pending_nonce_transaction(address, |nonce| {
                let data = data.clone();
                async move {
                    sign_contract_deployment(signer, data, nonce, chain_id, gas_price, gas_limit)
                        .await
                        .context("Failed to sign the transaction.")
                        .map_err(EthSubmitterError::Other)
                }
            })
            .await?;
        self.wait_for_deploy_address(tx_hash).await
    }
}

async fn sign_legacy_transaction<S: Signer + ?Sized>(
    signer: &S, tx: alloy_consensus::TxLegacy,
) -> anyhow::Result<AlloyBytes> {
    use alloy_consensus::SignableTransaction;

    let signature = signer.sign_hash(&tx.signature_hash()).await?;
    let signed = tx.into_signed(signature);
    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    Ok(buf.into())
}

/// Signs and encodes a transaction for contract deployment, ready for eth_sendRawTransaction.
/// Uses legacy transaction format for maximum compatibility.
pub async fn sign_contract_deployment<S: Signer + ?Sized>(
    signer: &S, data: AlloyBytes, nonce: u64, chain_id: u64, gas_price: u128, gas_limit: u64,
) -> anyhow::Result<AlloyBytes> {
    sign_legacy_transaction(
        signer,
        alloy_consensus::TxLegacy {
            nonce,
            gas_price,
            gas_limit,
            to: alloy_primitives::TxKind::Create,
            value: U256::ZERO,
            input: data,
            chain_id: Some(chain_id),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::RpcModule;
    use jsonrpsee::server::{Server, ServerHandle};
    use jsonrpsee::types::ErrorObjectOwned;
    use lyquor_jsonrpc::client::ClientConfig;
    use lyquor_test::test;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    const PRIVATE_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    #[derive(Clone)]
    struct RpcState {
        pending_nonces: Arc<Mutex<VecDeque<u64>>>,
        send_results: Arc<Mutex<VecDeque<Result<Hash, &'static str>>>>,
        pending_requests: Arc<Mutex<Vec<(Address, BlockNumber)>>>,
        raw_txs: Arc<Mutex<Vec<AlloyBytes>>>,
    }

    impl RpcState {
        fn new(
            pending_nonces: impl Into<VecDeque<u64>>, send_results: impl Into<VecDeque<Result<Hash, &'static str>>>,
        ) -> Self {
            Self {
                pending_nonces: Arc::new(Mutex::new(pending_nonces.into())),
                send_results: Arc::new(Mutex::new(send_results.into())),
                pending_requests: Arc::new(Mutex::new(Vec::new())),
                raw_txs: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn pending_requests(&self) -> Vec<(Address, BlockNumber)> {
            self.pending_requests
                .lock()
                .expect("pending request log should not be poisoned")
                .clone()
        }

        fn raw_txs(&self) -> Vec<AlloyBytes> {
            self.raw_txs.lock().expect("raw tx log should not be poisoned").clone()
        }
    }

    struct TestRpc {
        client: ClientHandle,
        shutdown: CancellationToken,
        server_handle: ServerHandle,
    }

    impl Drop for TestRpc {
        fn drop(&mut self) {
            self.shutdown.cancel();
            let _ = self.server_handle.stop();
        }
    }

    async fn start_rpc(state: RpcState) -> anyhow::Result<TestRpc> {
        let server = Server::builder().build("127.0.0.1:0").await?;
        let addr = server.local_addr()?;
        let mut module = RpcModule::new(());

        let nonce_state = state.clone();
        module.register_method::<Result<U256, ErrorObjectOwned>, _>(
            "eth_getTransactionCount",
            move |params, _, _| {
                let (address, block_number): (Address, BlockNumber) = params.parse()?;
                nonce_state
                    .pending_requests
                    .lock()
                    .expect("pending request log should not be poisoned")
                    .push((address, block_number));
                let nonce = nonce_state
                    .pending_nonces
                    .lock()
                    .expect("pending nonce queue should not be poisoned")
                    .pop_front()
                    .expect("test should provide enough pending nonce responses");
                Ok(U256::from(nonce))
            },
        )?;

        let send_state = state.clone();
        module.register_method::<Result<Hash, ErrorObjectOwned>, _>(
            "eth_sendRawTransaction",
            move |params, _, _| {
                let raw_tx: AlloyBytes = params.one()?;
                send_state
                    .raw_txs
                    .lock()
                    .expect("raw tx log should not be poisoned")
                    .push(raw_tx);
                match send_state
                    .send_results
                    .lock()
                    .expect("send result queue should not be poisoned")
                    .pop_front()
                    .expect("test should provide enough send responses")
                {
                    Ok(hash) => Ok(hash),
                    Err(message) => Err(ErrorObjectOwned::owned(-32000, message, None::<()>)),
                }
            },
        )?;

        let server_handle = server.start(module);
        let shutdown = CancellationToken::new();
        let client = ClientConfig::builder()
            .url(format!("ws://{addr}").parse()?)
            .build()
            .into_client(shutdown.clone());

        Ok(TestRpc {
            client,
            shutdown,
            server_handle,
        })
    }

    #[test(tokio::test)]
    async fn retries_nonce_too_low_with_fresh_pending_nonce() -> anyhow::Result<()> {
        let expected_hash = Hash::from([0x11; 32]);
        let state = RpcState::new([7, 8], [Err("nonce too low"), Ok(expected_hash)]);
        let rpc = start_rpc(state.clone()).await?;
        let signer = Arc::new(crate::signer_from_hex(PRIVATE_KEY)?);
        let submitter = EthSubmitter::new(rpc.client.clone(), signer.clone());

        let hash = submitter
            .sign_and_send_pending_nonce_transaction(
                signer.address(),
                |nonce| async move { Ok(vec![nonce as u8].into()) },
            )
            .await?;

        assert_eq!(hash, expected_hash);
        assert_eq!(
            state.pending_requests(),
            vec![
                (signer.address(), BlockNumber::Pending),
                (signer.address(), BlockNumber::Pending)
            ]
        );
        assert_eq!(
            state.raw_txs(),
            vec![AlloyBytes::from(vec![7]), AlloyBytes::from(vec![8])]
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn retries_replacement_send_error_with_fresh_pending_nonce() -> anyhow::Result<()> {
        let expected_hash = Hash::from([0x33; 32]);
        let state = RpcState::new([7, 8], [Err("replacement transaction underpriced"), Ok(expected_hash)]);
        let rpc = start_rpc(state.clone()).await?;
        let signer = Arc::new(crate::signer_from_hex(PRIVATE_KEY)?);
        let submitter = EthSubmitter::new(rpc.client.clone(), signer.clone());

        let hash = submitter
            .sign_and_send_pending_nonce_transaction(
                signer.address(),
                |nonce| async move { Ok(vec![nonce as u8].into()) },
            )
            .await?;

        assert_eq!(hash, expected_hash);
        assert_eq!(
            state.pending_requests(),
            vec![
                (signer.address(), BlockNumber::Pending),
                (signer.address(), BlockNumber::Pending)
            ]
        );
        assert_eq!(
            state.raw_txs(),
            vec![AlloyBytes::from(vec![7]), AlloyBytes::from(vec![8])]
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn already_known_send_error_does_not_retry_or_skip_nonce() -> anyhow::Result<()> {
        let expected_hash = Hash::from([0x22; 32]);
        let state = RpcState::new([7, 7], [Err("already known"), Ok(expected_hash)]);
        let rpc = start_rpc(state.clone()).await?;
        let signer = Arc::new(crate::signer_from_hex(PRIVATE_KEY)?);
        let submitter = EthSubmitter::new(rpc.client.clone(), signer.clone());

        let first = submitter
            .sign_and_send_pending_nonce_transaction(
                signer.address(),
                |nonce| async move { Ok(vec![nonce as u8].into()) },
            )
            .await;
        assert!(first.is_err(), "already-known response should not be retried");

        let hash = submitter
            .sign_and_send_pending_nonce_transaction(
                signer.address(),
                |nonce| async move { Ok(vec![nonce as u8].into()) },
            )
            .await?;

        assert_eq!(hash, expected_hash);
        assert_eq!(
            state.pending_requests(),
            vec![
                (signer.address(), BlockNumber::Pending),
                (signer.address(), BlockNumber::Pending)
            ]
        );
        assert_eq!(
            state.raw_txs(),
            vec![AlloyBytes::from(vec![7]), AlloyBytes::from(vec![7])]
        );

        Ok(())
    }
}
