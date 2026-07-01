// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

enum ABI { Lyquor, Eth }
// Mirrors lyquor_primitives::CallParams.
struct CallParams {
    address origin;
    address caller;
    string group;
    string method;
    bytes input;
    ABI abi_;
}

// Mirrors lyquor_seq::Slot.
event Slot (
    uint256 sn,
    CallParams[] calls,
    bytes32 image_hash,
    address switch_contract
);

// Same as lyquor_primitives::oracle::eth::OracleHeader.
struct OracleHeader {
    bytes32 topic;
    bytes32 group;
    bytes32 proposer;
    address target;
    bytes32 seqId;
    address ethContract;
    bytes32 configHash;
    uint32 epoch;
    bytes32 nonce;
}

// Same as lyquor_primitives::oracle::eth::OracleSigner.
struct OracleSigner {
    uint32 id;
    bytes32 nodeID;
}

// Same as lyquor_primitives::oracle::eth::OracleConfig.
struct OracleConfig {
    OracleSigner[] committee;
    uint16 threshold;
}

// Same as lyquor_primitives::oracle::eth::OracleConfigDelta.
struct OracleConfigDelta {
    OracleSigner[] upsert;
    uint32[] remove;
    bool thresholdChanged;
    uint16 threshold;
}

// Mirrors lyquor_primitives::oracle::eth::OracleCert.
struct OracleCert {
    OracleHeader header;
    bool hasConfigDelta;
    uint32[] signers;
    bytes[] signatures;
}

interface ISequenceBackend {
    function getEpoch(bytes32 topic) external view returns (uint32);
    function getConfigHash(bytes32 topic) external view returns (bytes32);
    function getChangeCount(bytes32 topic) external view returns (uint32);
    function getConfig(bytes32 topic) external view returns (OracleConfig memory);

    function ethCertifiedCall(
        OracleCert calldata oc, // Certificate bundle to establish the validity of the call.
        CallParams calldata params
    ) external;

    function __lyquor_switch_contract(address) external returns (uint256);
    function __lyquor_submit_certified_calls(CallParams[] memory calls) external;
}

interface IBartender {
    function register(address, address[] memory, bytes32, string memory) external;
    function getEd25519Address(bytes32 nodeID) external view returns (address);
    function getOracleLibrary() external view returns (address);
    function getEd25519Library() external view returns (address);
}

library SharedLibraryCall {
    function _revert(bytes memory err) private pure {
        if (err.length == 0) {
            revert("Shared library call failed.");
        }
        assembly {
            revert(add(err, 32), mload(err))
        }
    }

    function _requireCode(address libraryAddress) private view {
        require(libraryAddress != address(0), "Missing shared library.");
        require(libraryAddress.code.length != 0, "Shared library is not deployed.");
    }

    function delegate(address libraryAddress, bytes memory data) internal returns (bytes memory out) {
        _requireCode(libraryAddress);
        (bool ok, bytes memory ret) = libraryAddress.delegatecall(data);
        if (!ok) {
            _revert(ret);
        }
        return ret;
    }

    function staticcall(address libraryAddress, bytes memory data) internal view returns (bytes memory out) {
        _requireCode(libraryAddress);
        (bool ok, bytes memory ret) = libraryAddress.staticcall(data);
        if (!ok) {
            _revert(ret);
        }
        return ret;
    }
}
