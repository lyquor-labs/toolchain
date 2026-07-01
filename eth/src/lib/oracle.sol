// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.0;

import "./primitives.sol";
import "./crypto.sol";

library Oracle {
    // Certified Call implementation for the use case where the sequencing
    // chain itself is used as the target chain for call execution.
    // This part mirrors lyquid::runtime::oracle::dest::OracleDest for the EVM destination path.

    bytes28 public constant VALIDATE_PREIMAGE_PREFIX = "lyquor_validate_preimage_v1\x00";
    bytes32 public constant ORACLE_ON_EPOCH_ADVANCE_METHOD_HASH = keccak256("__lyquor_oracle_on_epoch_advance");
    bytes32 public constant ORACLE_INTERNAL_GROUP_HASH = keccak256("oracle::internal");

    // Mirrors lyquor_primitives::oracle::eth::ValidatePreimage.
    struct ValidatePreimage {
        OracleHeader header;
        CallParams params;
        bool approval; // Should always be true in checking.
    }

    event ConfigUpdate(
        bytes32 indexed topic,
        bytes32 indexed configHash,
        OracleConfig cfg
    );

    struct OracleSignerDest {
        bytes32 nodeID;
        // Resolved signer address frozen when the config becomes active.
        address signer;
    }

    // Storage-backed destination config. Rust keeps the same active config state in
    // lyquid::runtime::oracle::dest::OracleConfig.
    struct OracleConfigDest {
        mapping(uint32 => OracleSignerDest) committee;
        uint32[] committeeIds;
        uint16 threshold;
    }

    struct OracleDest {
        // The following states are used for certificate verification.
        // Active oracle config for the topic.
        OracleConfigDest config;
        // Hash of _config for the topic.
        bytes32 configHash;
        // Source-staged operation count consumed to reach the active epoch.
        uint32 changeCount;
        // The following variables are used to ensure a certified call is at most invoked once.
        // Epoch number.
        uint32 epoch;
        // Used random nonce in the current epoch; key = keccak256(epoch || nonce) => seen
        mapping(bytes32 => bool) usedNonce;
        // Size of _usedNonce
        uint32 nonceCount;
    }

    uint32 public constant MAX_NONCE_PER_EPOCH = 1_000_000;
    uint32 public constant MIN_NONCE_NEXT_EPOCH = MAX_NONCE_PER_EPOCH * 9 / 10;

    // Mirrors lyquid::runtime::oracle::dest::is_delta_canonical.
    function _isDeltaCanonical(OracleConfigDelta memory delta) private pure returns (bool) {
        // `remove` must be strictly sorted.
        for (uint256 i = 1; i < delta.remove.length; i++) {
            if (delta.remove[i - 1] >= delta.remove[i]) {
                return false;
            }
        }
        // `upsert` must be strictly sorted by signer ID.
        for (uint256 i = 1; i < delta.upsert.length; i++) {
            if (delta.upsert[i - 1].id >= delta.upsert[i].id) {
                return false;
            }
        }
        // `remove` and `upsert` must be disjoint.
        uint256 i = 0;
        uint256 j = 0;
        while (i < delta.remove.length && j < delta.upsert.length) {
            uint32 rid = delta.remove[i];
            uint32 uid = delta.upsert[j].id;
            if (rid == uid) {
                return false;
            }
            if (rid < uid) {
                i++;
            } else {
                j++;
            }
        }
        return true;
    }

    // Convert the active storage-backed config into the wire/memory form. This is still needed
    // for the public getter path and for building the post-delta config in memory.
    function _materializeConfig(OracleConfigDest storage config) private view returns (OracleConfig memory out) {
        uint256 n = config.committeeIds.length;
        out.committee = new OracleSigner[](n);
        for (uint256 i = 0; i < n; i++) {
            uint32 id = config.committeeIds[i];
            out.committee[i] = OracleSigner({
                id: id,
                nodeID: config.committee[id].nodeID
            });
        }
        out.threshold = config.threshold;
    }

    // Mirrors lyquid::runtime::oracle::dest::OracleDest::get_config for the EVM path.
    function getConfig(OracleDest storage o) internal view returns (OracleConfig memory) {
        return _materializeConfig(o.config);
    }

    // Mirrors lyquid::runtime::oracle::dest::OracleConfig::after_delta.
    function _configAfterDelta(
        OracleConfigDest storage config,
        OracleConfigDelta memory delta
    ) private view returns (OracleConfig memory nextConfig) {
        require(_isDeltaCanonical(delta), "Invalid config delta.");
        OracleConfig memory current = _materializeConfig(config);
        uint256 currentLen = current.committee.length;
        uint256 removeLen = delta.remove.length;
        uint256 upsertLen = delta.upsert.length;
        uint256 i = 0;
        uint256 r = 0;
        uint256 u = 0;
        uint256 nextLen = 0;

        while (i < currentLen || u < upsertLen) {
            if (u == upsertLen || (i < currentLen && current.committee[i].id < delta.upsert[u].id)) {
                if (r < removeLen && delta.remove[r] == current.committee[i].id) {
                    i++;
                    r++;
                } else {
                    nextLen++;
                    i++;
                }
                continue;
            }
            if (i == currentLen || delta.upsert[u].id < current.committee[i].id) {
                nextLen++;
                u++;
                continue;
            }
            nextLen++;
            i++;
            u++;
        }

        nextConfig.committee = new OracleSigner[](nextLen);
        nextConfig.threshold = delta.thresholdChanged ? delta.threshold : current.threshold;

        i = 0;
        r = 0;
        u = 0;
        uint256 k = 0;
        while (i < currentLen || u < upsertLen) {
            if (u == upsertLen || (i < currentLen && current.committee[i].id < delta.upsert[u].id)) {
                if (r < removeLen && delta.remove[r] == current.committee[i].id) {
                    i++;
                    r++;
                } else {
                    nextConfig.committee[k++] = current.committee[i++];
                }
                continue;
            }
            if (i == currentLen || delta.upsert[u].id < current.committee[i].id) {
                nextConfig.committee[k++] = delta.upsert[u++];
                continue;
            }
            nextConfig.committee[k++] = delta.upsert[u];
            i++;
            u++;
        }

        require(
            nextConfig.threshold != 0 &&
            nextLen <= uint256(type(uint16).max) &&
            nextLen >= uint256(nextConfig.threshold),
            "Invalid config."
        );
    }

    function _resolveSignerAddress(IBartender bartender, bytes32 nodeID) private view returns (address signer) {
        signer = bartender.getEd25519Address(nodeID);
        require(signer != address(0), "Missing signer binding.");
    }

    function _verifyResolvedSignerAddress(
        address signer,
        bytes32 digest,
        bytes memory signature
    ) private pure {
        bool result = Crypto.verifyECSig(signer, digest, signature);
        require(result, "Signer address mismatch.");
    }

    function _setConfig(
        OracleConfigDest storage config,
        OracleConfig memory nextConfig,
        IBartender bartender
    ) private {
        uint256 currentLen = config.committeeIds.length;
        for (uint256 i = 0; i < currentLen; i++) {
            delete config.committee[config.committeeIds[i]];
        }
        delete config.committeeIds;
        for (uint256 i = 0; i < nextConfig.committee.length; i++) {
            OracleSigner memory signer = nextConfig.committee[i];
            config.committeeIds.push(signer.id);
            config.committee[signer.id] = OracleSignerDest({
                nodeID: signer.nodeID,
                signer: _resolveSignerAddress(bartender, signer.nodeID)
            });
        }
        config.threshold = nextConfig.threshold;
    }

    function _update(
        OracleDest storage o,
        bytes32 topic,
        OracleHeader memory header,
        OracleConfig memory nextConfig,
        bool updateConfig,
        uint32 changeCount,
        IBartender bartender
    ) private {
        require(header.epoch >= o.epoch, "Stale epoch.");
        uint32 epochDelta = header.epoch - o.epoch;
        bytes32 key = keccak256(abi.encodePacked(header.epoch, header.nonce));

        if (epochDelta == 0) {
            require(!updateConfig, "Unexpected config update.");
            require(!o.usedNonce[key], "Nonce is used.");
            require(o.nonceCount < MAX_NONCE_PER_EPOCH, "Epoch full.");
        } else {
            require(epochDelta == 1, "Epoch advanced too far.");
            if (!updateConfig) {
                require(o.nonceCount >= MIN_NONCE_NEXT_EPOCH, "Epoch advanced too early.");
            }
            o.epoch = header.epoch;
            // NOTE: o.usedNonce can't be efficiently deleted due to Solidity's limitation.
            // Epoch-scoped keys plus nonceCount reset are sufficient.
            o.nonceCount = 0;
            o.changeCount = changeCount;
            if (updateConfig) {
                _setConfig(o.config, nextConfig, bartender);
                o.configHash = header.configHash;
                emit ConfigUpdate(topic, header.configHash, nextConfig);
            }
        }

        o.usedNonce[key] = true;
        o.nonceCount = o.nonceCount + 1;
    }

    function _verifyEvmBinding(OracleCert memory oc, bytes32 seqId) private view {
        // Ensure this certificate belongs to the active sequence backend.
        require(oc.header.seqId == seqId, "Invalid sequence backend.");
        // Ensure this certificate targets this sequencing contract.
        require(oc.header.ethContract == address(this), "Sequence backend mismatch.");
    }

    function _isEpochAdvanceParams(CallParams memory params) private pure returns (bool) {
        return
            params.origin == address(0) &&
            params.abi_ == ABI.Lyquor &&
            keccak256(bytes(params.group)) == ORACLE_INTERNAL_GROUP_HASH &&
            keccak256(bytes(params.method)) == ORACLE_ON_EPOCH_ADVANCE_METHOD_HASH;
    }

    function _validatePreimage(CallParams memory params, OracleHeader memory header) private pure returns (bytes memory) {
        return abi.encodePacked(VALIDATE_PREIMAGE_PREFIX, abi.encode(ValidatePreimage({
            header: header,
            params: params,
            approval: true
        })));
    }

    function _requireCertificateShape(OracleCert memory oc, uint16 threshold) private pure {
        require(oc.signers.length == oc.signatures.length, "Malformed certificate.");
        require(oc.signers.length >= threshold, "Threshold not met.");
    }

    /// Mirrors lyquid::runtime::oracle::dest::verify_oracle_cert for the bootstrap path, where
    /// the incoming config only exists in memory.
    function _verifyOracleCertMemory(
        OracleCert memory oc,
        bytes memory m,
        OracleConfig memory config,
        IBartender bartender
    ) private view {
        uint16 threshold = config.threshold;
        bytes32 digest = keccak256(m);

        _requireCertificateShape(oc, threshold);

        uint256 c = 0;
        unchecked {
            for (uint256 i = 0; i < threshold; i++) {
                uint32 sid = oc.signers[i];
                if (i > 0) {
                    require(sid > oc.signers[i - 1], "Signers not sorted.");
                }
                // `oc.signers` is a sorted subset of the full sorted committee, so advance a
                // single committee cursor until it reaches the matching signer ID.
                while (c < config.committee.length && config.committee[c].id < sid) {
                    c++;
                }
                require(c < config.committee.length && config.committee[c].id == sid, "Unknown signer.");
                _verifyResolvedSignerAddress(
                    _resolveSignerAddress(bartender, config.committee[c].nodeID),
                    digest,
                    oc.signatures[i]
                );
            }
        }
    }

    /// Mirrors lyquid::runtime::oracle::dest::verify_oracle_cert for the active-config path.
    /// This avoids materializing the full committee when the current config is already in storage.
    function _verifyOracleCertStorage(
        OracleCert memory oc,
        bytes memory m,
        OracleConfigDest storage config
    ) private view {
        uint16 threshold = config.threshold;
        bytes32 digest = keccak256(m);

        _requireCertificateShape(oc, threshold);

        uint256 c = 0;
        unchecked {
            for (uint256 i = 0; i < threshold; i++) {
                uint32 sid = oc.signers[i];
                if (i > 0) {
                    require(sid > oc.signers[i - 1], "Signers not sorted.");
                }
                // `oc.signers` is a sorted subset of the full sorted committee, so advance a
                // single committee cursor until it reaches the matching signer ID.
                while (c < config.committeeIds.length && config.committeeIds[c] < sid) {
                    c++;
                }
                require(c < config.committeeIds.length && config.committeeIds[c] == sid, "Unknown signer.");
                _verifyResolvedSignerAddress(config.committee[sid].signer, digest, oc.signatures[i]);
            }
        }
    }

    // Mirrors the destination-side verification state machine from
    // lyquid::runtime::oracle::dest::OracleDest for the EVM path. template.sol decides whether
    // this is an ordinary certified call or an epoch-advance update, and the route is checked
    // again here against the params shape.
    function verify(
        OracleDest storage o,
        OracleCert memory oc,
        CallParams memory params,
        bool isEpochAdvance,
        bytes32 seqId,
        IBartender bartender
    ) internal {
        _verifyEvmBinding(oc, seqId);
        require(isEpochAdvance == _isEpochAdvanceParams(params), "Route mismatch.");
        bytes memory m = _validatePreimage(params, oc.header);
        if (!isEpochAdvance) {
            require(o.epoch != 0, "Oracle is uninitialized.");
            require(!oc.hasConfigDelta, "Unexpected config delta.");
            // EVM destination path must carry Eth ABI call params.
            require(params.abi_ == ABI.Eth, "Invalid ABI.");
            require(oc.header.epoch == o.epoch, "Invalid epoch.");
            require(oc.header.configHash == o.configHash, "Config mismatch.");

            _verifyOracleCertStorage(oc, m, o.config);
            _update(
                o,
                oc.header.topic,
                oc.header,
                OracleConfig({committee: new OracleSigner[](0), threshold: 0}),
                false,
                o.changeCount,
                bartender
            );
            return;
        }

        require(oc.header.epoch == o.epoch + 1, "Invalid epoch.");

        OracleConfigDelta memory configDelta;
        uint32 changeCount;
        (configDelta, changeCount) = abi.decode(params.input, (OracleConfigDelta, uint32));

        OracleConfig memory nextConfig = OracleConfig({
            committee: new OracleSigner[](0),
            threshold: 0
        });
        bool updateConfig = false;

        if (!configDelta.thresholdChanged && configDelta.upsert.length == 0 && configDelta.remove.length == 0) {
            require(oc.header.configHash == o.configHash, "Config mismatch.");
        } else {
            nextConfig = _configAfterDelta(o.config, configDelta);
            require(keccak256(abi.encode(nextConfig)) == oc.header.configHash, "Config mismatch.");
            updateConfig = true;
        }

        if (o.epoch == 0) {
            require(updateConfig, "Oracle is uninitialized.");
            _verifyOracleCertMemory(oc, m, nextConfig, bartender);
        } else {
            _verifyOracleCertStorage(oc, m, o.config);
        }
        _update(o, oc.header.topic, oc.header, nextConfig, updateConfig, changeCount, bartender);
    }

    // ABI entrypoint for the manual delegatecall path. Rebuild the storage pointer from the
    // caller-provided root slot, then run the destination-side verifier for the selected route.
    function verifyAtStorageSlot(
        uint256 oracleStorageSlot,
        OracleCert memory oc,
        CallParams memory params,
        bool isEpochAdvance,
        bytes32 seqId,
        IBartender bartender
    ) external {
        OracleDest storage o;
        assembly {
            o.slot := oracleStorageSlot
        }
        verify(o, oc, params, isEpochAdvance, seqId, bartender);
    }
}
