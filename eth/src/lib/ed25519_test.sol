// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

// Run: forge script ed25519_test.sol --root . -vvvv

import "./ed25519.sol";

contract StandaloneTest {
    event Log(string message);

    // The following are from https://datatracker.ietf.org/doc/html/rfc8032#section-7.1
    uint256 constant PUBKEY = 0xd75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a;
    uint256 constant SIG_R = 0xe5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155;
    uint256 constant SIG_S = 0x5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b;

    // The following values are to be tested (should correspond to the pubkey).
    uint256 constant QX = 0x5961a13b250782afee75256fdba2e6bec4d810f89f6ce1c033585acd96b48329;
    uint256 constant QY = 0x53373f33d468fee07fb2e53496849c8e52b3db37af7729999b2c3d372aca8de5;

    function run() public {
        emit Log("Checking Rust Coordinates...");

        // Call the library
        bool isValid = SCL_EIP6565.verifyLEWithPubkey("", SIG_R, SIG_S, bytes32(PUBKEY), QX, QY);

        if (isValid) {
            emit Log("RESULT: SUCCESS. Signature Verified.");
        } else {
            emit Log("RESULT: FAILURE. Signature Invalid.");
        }

        require(isValid, "Verification Failed!");
    }
}
