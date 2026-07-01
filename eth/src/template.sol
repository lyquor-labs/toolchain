// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

//// sequencer-preamble-begin

import "./lib/primitives.sol";
import "./lib/crypto.sol";
import "./lib/oracle.sol"; // Certified call implementation.

//// sequencer-preamble-end

// NOTE: shaker will extract the code surrounded by
// sequencer-*-begin/end. This is preserved to make the template a legit
// solidity file for checking during development.
contract _template is ISequenceBackend {
    //// sequencer-body-begin

    bytes32 private constant ORACLE_ON_EPOCH_ADVANCE_METHOD_HASH = keccak256("__lyquor_oracle_on_epoch_advance");
    bytes32 private constant ORACLE_INTERNAL_GROUP_HASH = keccak256("oracle::internal");
    string private constant ORACLE_CERTIFIED_GROUP_PREFIX = "oracle::certified::";
    address creator;
    IBartender bartender = IBartender(address(0)); // Only used by non-bartender Lyquids.
    address oracleLibrary;
    address ed25519Library;

    uint256 next_slot = 0;
    mapping(bytes32 => address) ed25519ToAddress; // Only used by the bartender. Signed mapping from ed25519 node ID to address.
    mapping(bytes32 => Oracle.OracleDest) private _oracle;

    function getEpoch(bytes32 topic) external view returns (uint32) {
        return _oracle[topic].epoch;
    }

    function getConfigHash(bytes32 topic) external view returns (bytes32) {
        return _oracle[topic].configHash;
    }

    function getChangeCount(bytes32 topic) external view returns (uint32) {
        return _oracle[topic].changeCount;
    }

    function getConfig(bytes32 topic) external view returns (OracleConfig memory) {
        return Oracle.getConfig(_oracle[topic]);
    }

    function getOracleLibrary() external view returns (address) {
        return oracleLibrary;
    }

    function getEd25519Library() external view returns (address) {
        return ed25519Library;
    }

    function __lyquor_decode_eth_certified_envelope(
        bytes calldata raw
    ) external pure returns (bytes memory inputRaw, OracleCert memory oc) {
        return abi.decode(raw, (bytes, OracleCert));
    }

    function _isEpochAdvanceParams(
        address origin,
        address,
        ABI abi_,
        bytes32 groupHash,
        bytes32 methodHash
    ) private pure returns (bool) {
        return
            origin == address(0) &&
            abi_ == ABI.Lyquor &&
            groupHash == ORACLE_INTERNAL_GROUP_HASH &&
            methodHash == ORACLE_ON_EPOCH_ADVANCE_METHOD_HASH;
    }

    function _bartenderAddress() private view returns (address) {
        address bar = address(bartender);
        if (bar == address(0)) {
            // bartender contract itself
            return address(this);
        }
        return bar;
    }

    function _sequenceBackendId() private view returns (bytes32) {
        return keccak256(abi.encodePacked("lyquor_sequence_backend", uint64(block.chainid), _bartenderAddress()));
    }

    function _forwardCertifiedCall(
        OracleHeader calldata header,
        CallParams calldata params
    ) private {
        if (header.target == address(0)) {
            return;
        }
        bytes4 selector = bytes4(keccak256(bytes(params.method)));
        (bool ok, ) = header.target.call(
            abi.encodeWithSelector(selector, header.topic, header.group, params.origin, params.caller, params.input)
        );
        require(ok, "Certified call failed.");
    }

    function __lyquor_switch_contract(address next) external returns (uint256 slot_base) {
        if (tx.origin == creator) {
            slot_base = next_slot;
            // skip slot doesn't advance the slot number
            emit Slot(0, new CallParams[](0), "", next);
        }
    }

    function ethCertifiedCall(
        OracleCert calldata oc, // Certificate bundle to establish the validity of the call.
        CallParams calldata params
    ) external {
        bytes32 groupHash = keccak256(bytes(params.group));
        require(groupHash == oc.header.group, "Group mismatch.");
        bool isEpochAdvance = _isEpochAdvanceParams(
            params.origin,
            params.caller,
            params.abi_,
            groupHash,
            keccak256(bytes(params.method))
        );
        uint256 oracleStorageSlot;
        bytes32 topic = oc.header.topic;
        assembly {
            mstore(0x00, topic)
            mstore(0x20, _oracle.slot)
            oracleStorageSlot := keccak256(0x00, 0x40)
        }
        SharedLibraryCall.delegate(
            oracleLibrary,
            abi.encodeWithSelector(
                Oracle.verifyAtStorageSlot.selector,
                oracleStorageSlot,
                oc,
                params,
                isEpochAdvance,
                _sequenceBackendId(),
                IBartender(_bartenderAddress())
            )
        );
        if (!isEpochAdvance) {
            _forwardCertifiedCall(oc.header, params);
        }
    }

    function __lyquor_submit_certified_calls(CallParams[] memory calls) external {
        CallParams[] memory slotCalls = new CallParams[](calls.length);
        uint256 slotCount = 0;
        for (uint256 i = 0; i < calls.length; i++) {
            CallParams memory call = calls[i];
            bytes32 groupHash = keccak256(bytes(call.group));
            bytes32 methodHash = keccak256(bytes(call.method));
            bool isEpochAdvance = _isEpochAdvanceParams(
                call.origin,
                call.caller,
                call.abi_,
                groupHash,
                methodHash
            );
            // `abi_` is only a decode-path hint; destination authority comes from the cert target.
            if (call.abi_ == ABI.Eth || isEpochAdvance) {
                try this.__lyquor_decode_eth_certified_envelope(call.input) returns (
                    bytes memory inputRaw,
                    OracleCert memory oc
                ) {
                    call.input = inputRaw;
                    this.ethCertifiedCall(oc, call);
                    continue;
                } catch {
                    if (call.abi_ == ABI.Eth) {
                        revert("Invalid certified envelope.");
                    }
                }
            }
            if (!isEpochAdvance) {
                call.group = string(abi.encodePacked(ORACLE_CERTIFIED_GROUP_PREFIX, call.group));
            }
            slotCalls[slotCount++] = call;
        }
        if (slotCount == 0) {
            return;
        }
        CallParams[] memory out = new CallParams[](slotCount);
        for (uint256 i = 0; i < slotCount; i++) {
            out[i] = slotCalls[i];
        }
        emit Slot(next_slot++, out, "", address(0));
    }

    //// sequencer-body-end
}
