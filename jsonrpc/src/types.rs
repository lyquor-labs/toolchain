use std::fmt;

use alloy_primitives::{BlockHash, Bytes};
use jsonrpsee::core::traits::ToRpcParams;
use lyquor_primitives::{Address, U256, alloy_primitives};
use serde::{Deserialize, Serialize};

pub use jsonrpsee::rpc_params;

#[cfg(test)] use lyquor_test::test;

/// JSON-RPC message trait for jsonrpsee.
pub trait JsonRPCMsgT: Send + 'static {
    /// Return the JSON-RPC method name.
    fn method() -> &'static str;
    /// Convert the request into jsonrpsee parameters.
    fn into_params(self) -> impl ToRpcParams + Send + 'static;
}

/// JSON-RPC subscription request trait with a matching unsubscribe method.
pub trait JsonRPCSubscriptionMsgT: Send + Clone {
    /// Return the subscribe method name.
    fn method() -> &'static str;
    /// Return the unsubscribe method name.
    fn unsubscribe_method() -> &'static str;
    /// Convert the subscription request into jsonrpsee parameters.
    fn into_params(self) -> impl ToRpcParams + Send + 'static;
}

/// Ethereum block tag or explicit block number.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum BlockNumber {
    Quantity(U256),
    Latest,
    Earliest,
    Pending,
    Safe,
    Finalized,
}

impl Serialize for BlockNumber {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Quantity(q) => q.serialize(serializer),
            Self::Latest => serializer.serialize_str("latest"),
            Self::Earliest => serializer.serialize_str("earliest"),
            Self::Pending => serializer.serialize_str("pending"),
            Self::Safe => serializer.serialize_str("safe"),
            Self::Finalized => serializer.serialize_str("finalized"),
        }
    }
}

impl<'de> Deserialize<'de> for BlockNumber {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct BNVisitor;
        impl serde::de::Visitor<'_> for BNVisitor {
            type Value = BlockNumber;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "block number or tag")
            }
            fn visit_str<E>(self, data: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                use BlockNumber::*;
                match data {
                    "latest" => Ok(Latest),
                    "earliest" => Ok(Earliest),
                    "pending" => Ok(Pending),
                    "safe" => Ok(Safe),
                    "finalized" => Ok(Finalized),
                    _ => match data.parse() {
                        Ok(q) => Ok(BlockNumber::Quantity(q)),
                        Err(e) => Err(E::custom(e)),
                    },
                }
            }
        }
        deserializer.deserialize_str(BNVisitor)
    }
}

impl fmt::Display for BlockNumber {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Quantity(q) => q.fmt(f),
            Self::Latest => write!(f, "latest"),
            Self::Earliest => write!(f, "earliest"),
            Self::Pending => write!(f, "pending"),
            Self::Safe => write!(f, "safe"),
            Self::Finalized => write!(f, "finalized"),
        }
    }
}

#[test]
fn test_block_number() {
    use lyquor_primitives::alloy_primitives::uint;
    let bn = BlockNumber::Finalized;
    let bn2: BlockNumber = serde_json::from_str(&serde_json::to_string(&bn).unwrap()).unwrap();
    assert_eq!(bn, bn2);
    let bn = BlockNumber::Quantity(uint!(123_U256));
    let bn2: BlockNumber = serde_json::from_str(&serde_json::to_string(&bn).unwrap()).unwrap();
    assert_eq!(bn, bn2);
}

/// Ethereum 256-bit hash value.
pub type Hash = alloy_primitives::B256;

/// `eth_subscribe` request.
#[derive(Serialize, Debug, Clone)]
pub struct EthSubscribe(pub String);

/// `eth_subscribe` response carrying the subscription ID.
#[derive(Deserialize, Debug)]
pub struct EthSubscribeResp(pub Bytes);

impl JsonRPCSubscriptionMsgT for EthSubscribe {
    fn method() -> &'static str {
        "eth_subscribe"
    }

    fn unsubscribe_method() -> &'static str {
        "eth_unsubscribe"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params![self.0]
    }
}

/// `eth_unsubscribe` request.
#[derive(Serialize, Debug)]
pub struct EthUnsubscribe(pub Bytes);

/// `eth_unsubscribe` response.
#[derive(Deserialize, Debug)]
pub struct EthUnsubscribeResp(pub bool);

/// Header update returned by the `newHeads` subscription.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EthNewHeadUpdate {
    pub number: BlockNumber,
    pub hash: Option<BlockHash>,
    pub parent_hash: BlockHash,
    pub transactions_root: Hash,
    pub state_root: Hash,
    pub receipts_root: Hash,
    pub gas_limit: U256,
    pub gas_used: U256,
    pub timestamp: U256,
}
// Reference: https://geth.ethereum.org/docs/interacting-with-geth/rpc/pubsub#supported-subscriptions

/// Raw Ethereum subscription notification.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EthSubscriptionUpdate {
    pub result: Box<serde_json::value::RawValue>,
    pub subscription: Bytes,
}

/// `eth_newFilter` response carrying the filter ID.
#[derive(Deserialize, Debug)]
pub struct EthNewFilterResp(pub U256);

/// `eth_uninstallFilter` response.
#[derive(Deserialize, Debug)]
pub struct EthUninstallFilterResp(pub bool);

/// Ethereum log object returned by filter and receipt APIs.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Log {
    pub log_index: U256,
    pub transaction_index: U256,
    pub transaction_hash: Hash,
    pub block_hash: Hash,
    pub block_number: BlockNumber,
    pub address: Address,
    pub data: Bytes,
    pub topics: Vec<Hash>,
}

/// `eth_getLogs` response.
#[derive(Deserialize, Debug)]
pub struct EthGetLogsResp(pub Vec<Log>);

/// `eth_getFilterChanges` response.
#[derive(Deserialize, Debug)]
pub struct EthGetFilterChangesResp(pub Vec<Log>);

/// Ethereum block object returned by block lookup APIs.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    pub number: BlockNumber,
    pub hash: BlockHash,
    pub parent_hash: BlockHash,
    pub nonce: alloy_primitives::FixedBytes<8>,
    pub sha3_uncles: Hash,
    pub logs_bloom: alloy_primitives::FixedBytes<256>,
    pub transactions_root: Hash,
    pub state_root: Hash,
    pub receipts_root: Hash,
    pub miner: Address,
    pub difficulty: U256,
    pub total_difficulty: U256,
    pub extra_data: Bytes,
    pub size: U256,
    pub gas_limit: U256,
    pub gas_used: U256,
    pub timestamp: U256,
    pub transactions: Vec<Hash>,
    pub uncles: Vec<Hash>,
}
// Reference:
// [1] https://ethereum.org/en/developers/docs/apis/json-rpc/#eth_getblockbyhash
// [2] https://github.com/ethereum/go-ethereum/blob/master/core/types/block.go

/// `eth_getBlockByNumber` response.
#[derive(Deserialize, Debug)]
pub struct EthGetBlockByNumberResp(pub Option<Block>);

/// `eth_accounts` request.
pub struct EthAccounts;

/// `eth_accounts` response.
#[derive(Deserialize, Debug)]
pub struct EthAccountsResp(pub Vec<Address>);

impl JsonRPCMsgT for EthAccounts {
    fn method() -> &'static str {
        "eth_accounts"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params!()
    }
}

/// `eth_sendTransaction` request.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EthSendTransaction {
    pub from: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas: Option<U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_price: Option<U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<U256>,
    pub input: Bytes,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<U256>,
}

/// `eth_sendTransaction` response.
#[derive(Deserialize, Debug)]
pub struct EthSendTransactionResp(pub Hash);

impl JsonRPCMsgT for EthSendTransaction {
    fn method() -> &'static str {
        "eth_sendTransaction"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params!(self)
    }
}

/// `eth_sendRawTransaction` request.
#[derive(Serialize, Debug)]
pub struct EthSendRawTransaction(pub Bytes);

impl JsonRPCMsgT for EthSendRawTransaction {
    fn method() -> &'static str {
        "eth_sendRawTransaction"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params!(self.0)
    }
}

/// `eth_chainId` request.
#[derive(Debug)]
pub struct EthGetChainId;

/// `eth_chainId` response.
#[derive(Deserialize, Debug)]
pub struct EthGetChainIdResp(pub U256);

impl JsonRPCMsgT for EthGetChainId {
    fn method() -> &'static str {
        "eth_chainId"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params!()
    }
}

/// `eth_getTransactionCount` request.
#[derive(Debug)]
pub struct EthGetTransactionCount {
    pub address: Address,
    pub block_number: BlockNumber,
}

/// `eth_getTransactionCount` response.
#[derive(Deserialize, Debug)]
pub struct EthGetTransactionCountResp(pub U256);

impl JsonRPCMsgT for EthGetTransactionCount {
    fn method() -> &'static str {
        "eth_getTransactionCount"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params!(self.address, self.block_number)
    }
}

/// `eth_gasPrice` request.
#[derive(Debug)]
pub struct EthGasPrice;

/// `eth_gasPrice` response.
#[derive(Deserialize, Debug)]
pub struct EthGasPriceResp(pub U256);

impl JsonRPCMsgT for EthGasPrice {
    fn method() -> &'static str {
        "eth_gasPrice"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params!()
    }
}

/// `eth_estimateGas` request.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EthEstimateGas {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas: Option<U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_price: Option<U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Bytes>,
}

/// `eth_estimateGas` response.
#[derive(Deserialize, Debug)]
pub struct EthEstimateGasResp(pub U256);

impl JsonRPCMsgT for EthEstimateGas {
    fn method() -> &'static str {
        "eth_estimateGas"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params!(self)
    }
}

/// Transaction object used by `eth_call`.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EthCallTx {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas: Option<U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_price: Option<U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Bytes>,
}

/// `eth_call` request.
#[derive(Serialize, Debug)]
pub struct EthCall {
    pub tx: EthCallTx,
    pub block_number: BlockNumber,
}

/// `eth_call` response bytes.
#[derive(Deserialize, Debug)]
pub struct EthCallResp(pub Bytes);

impl JsonRPCMsgT for EthCall {
    fn method() -> &'static str {
        "eth_call"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params!(self.tx, self.block_number)
    }
}

/// `eth_getBlockByNumber` request.
#[derive(Debug)]
pub struct EthGetBlockByNumber {
    pub block: BlockNumber,
    pub full_tx: bool,
}

impl JsonRPCMsgT for EthGetBlockByNumber {
    fn method() -> &'static str {
        "eth_getBlockByNumber"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params!(self.block, self.full_tx)
    }
}

/// `eth_getLogs` request.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EthGetLogs {
    pub from_block: BlockNumber,
    pub to_block: BlockNumber,
    pub address: Vec<Address>,
    pub topics: Vec<Vec<Hash>>,
}

impl JsonRPCMsgT for EthGetLogs {
    fn method() -> &'static str {
        "eth_getLogs"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params!(self)
    }
}

/// Ethereum transaction receipt object.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EthTransactionReceipt {
    pub from: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Address>,
    pub gas_used: U256,
    pub cumulative_gas_used: U256,
    pub effective_gas_price: U256,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_address: Option<Address>,
    pub block_hash: Hash,
    pub block_number: BlockNumber,
    pub transaction_index: U256,
    pub transaction_hash: Hash,
    pub logs: Vec<Log>,
    pub logs_bloom: alloy_primitives::FixedBytes<256>,
    pub type_: U256,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<U256>,
    pub status: U256,
}

/// `eth_getTransactionReceipt` response.
pub type EthGetTransactionReceiptResp = Option<EthTransactionReceipt>;

/// `eth_getTransactionReceipt` request.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EthGetTransactionReceipt(pub Hash);

impl JsonRPCMsgT for EthGetTransactionReceipt {
    fn method() -> &'static str {
        "eth_getTransactionReceipt"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params!(self.0)
    }
}

/// Anvil `anvil_dumpState` request.
#[derive(Serialize, Debug)]
pub struct AnvilDumpState;
impl JsonRPCMsgT for AnvilDumpState {
    fn method() -> &'static str {
        "anvil_dumpState"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params!()
    }
}

/// Anvil `anvil_dumpState` response.
#[derive(Deserialize, Debug)]
pub struct AnvilDumpStateResp(pub Bytes);

/// Anvil `anvil_loadState` request.
#[derive(Serialize, Debug)]
pub struct AnvilLoadState(pub Bytes);
impl JsonRPCMsgT for AnvilLoadState {
    fn method() -> &'static str {
        "anvil_loadState"
    }

    fn into_params(self) -> impl ToRpcParams + Send + 'static {
        rpc_params!(self.0)
    }
}

/// Anvil `anvil_loadState` response.
#[derive(Deserialize, Debug)]
pub struct AnvilLoadStateResp(pub bool);
