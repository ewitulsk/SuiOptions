// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {ISpokeIntegration} from "../../src/interfaces/ISpokeIntegration.sol";

/// @notice Test double integration: records extendTo callbacks and serves
///         a settable rawState blob.
contract MockIntegration is ISpokeIntegration {
    bytes public raw;
    address public lastAsset;
    uint256 public lastAmount;
    uint256 public receivedCount;

    function setRaw(bytes calldata raw_) external {
        raw = raw_;
    }

    function onFundsReceived(address asset, uint256 amount) external {
        lastAsset = asset;
        lastAmount = amount;
        receivedCount += 1;
    }

    function rawState() external view returns (bytes memory) {
        return raw;
    }
}
