// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @title Minimal Chainlink CCIP client interfaces (vendored)
/// @notice Trimmed to the surface `CCIPEndpoint.sol` needs. Canonical
///         upstream: smartcontractkit/ccip, package "chainlink/contracts-ccip"
///         (`ccip/libraries/Client.sol` and `ccip/interfaces/IRouterClient.sol`).
///         Struct layouts match upstream so a real router is call-compatible.
library Client {
    struct EVMTokenAmount {
        address token;
        uint256 amount;
    }

    /// @notice A message delivered by the CCIP router to a receiver.
    struct Any2EVMMessage {
        bytes32 messageId;
        uint64 sourceChainSelector;
        bytes sender; // ABI-decodable per source chain family
        bytes data;
        EVMTokenAmount[] destTokenAmounts;
    }

    /// @notice A message submitted to the CCIP router for sending.
    struct EVM2AnyMessage {
        bytes receiver; // encoded per destination chain family
        bytes data;
        EVMTokenAmount[] tokenAmounts;
        address feeToken; // address(0) = native
        bytes extraArgs;
    }
}

interface IRouterClient {
    function ccipSend(uint64 destinationChainSelector, Client.EVM2AnyMessage calldata message)
        external
        payable
        returns (bytes32);

    function getFee(uint64 destinationChainSelector, Client.EVM2AnyMessage calldata message)
        external
        view
        returns (uint256);
}

/// @notice Receiver surface the CCIP router calls on delivery (upstream:
///         `IAny2EVMMessageReceiver`).
interface IAny2EVMMessageReceiver {
    function ccipReceive(Client.Any2EVMMessage calldata message) external;
}
