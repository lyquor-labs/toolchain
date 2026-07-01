// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import "./primitives.sol";
import "./ed25519.sol";

library Crypto {
    string private constant SET_ED25519_SECP256K1_BINDING_PREFIX = "lyquor_set_ed25519_secp256k1_binding\x00";

    function verifyECSig(address signer, bytes32 digest, bytes memory sig) internal pure returns (bool) {
        if (signer == address(0) || sig.length != 65) {
            return false;
        }
        bytes32 r;
        bytes32 s;
        uint8 v = uint8(sig[64]);
        assembly {
            r := mload(add(sig, 32))
            s := mload(add(sig, 64))
        }
        return signer == ecrecover(digest, v, r, s);
    }

    function setEd25519Address(
        address ed25519Library,
        mapping(bytes32 => address) storage ed25519ToAddress,
        uint256 next_slot,
        address addr,
        bytes32 pubkey,
        uint256[2] memory q,
        uint256[2] memory edSig,
        bytes calldata ecSig
    ) internal returns (bool, uint256) {
        bytes memory message = abi.encodePacked(SET_ED25519_SECP256K1_BINDING_PREFIX, addr, pubkey);
        if (!verifyEd25519Signature(ed25519Library, string(message), edSig[0], edSig[1], pubkey, q[0], q[1])) {
            return (false, next_slot);
        }
        if (!verifyECSig(addr, keccak256(message), ecSig)) {
            return (false, next_slot);
        }
        ed25519ToAddress[pubkey] = addr;
        bytes memory input = abi.encodeWithSelector(
            bytes4(keccak256("set_ed25519_address(bytes32,uint256,uint256,address)")),
            pubkey,
            q[0],
            q[1],
            addr
        );

        CallParams[] memory calls = new CallParams[](1);
        calls[0] = CallParams({
            origin: tx.origin,
            caller: msg.sender,
            method: "set_ed25519_address",
            group: "main",
            input: input,
            abi_: ABI.Eth
        });

        emit Slot(next_slot, calls, bytes32(0), address(0));
        return (true, next_slot + 1);
    }

    function verifyEd25519Signature(
        address ed25519Library,
        string memory m,
        uint256 r,
        uint256 s,
        bytes32 pubkey,
        uint256 qx,
        uint256 qy
    ) internal view returns (bool) {
        bytes memory ret = SharedLibraryCall.staticcall(
            ed25519Library,
            abi.encodeWithSelector(SCL_EIP6565.verifyLEWithPubkey.selector, m, r, s, pubkey, qx, qy)
        );
        return abi.decode(ret, (bool));
    }
}
