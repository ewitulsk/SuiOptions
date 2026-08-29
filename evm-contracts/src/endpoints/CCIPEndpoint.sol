// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {IMessageEndpoint} from "../interfaces/IMessageEndpoint.sol";
import {ISpokeVault} from "../interfaces/ISpokeVault.sol";
import {Wire} from "../lib/Wire.sol";
import {Client, IRouterClient, IAny2EVMMessageReceiver} from "../vendor/CCIP.sol";

/// @title CCIPEndpoint — secondary transport endpoint (plan §2.3)
/// @notice CCIPReceiver-style adapter between the SpokeVault and the
///         Chainlink CCIP router on this chain. The hub (source chain
///         selector + sender bytes) is pinned at construction.
///         Verification rides the Chainlink DON — this contract only
///         checks that delivery comes from the real router and from the
///         pinned hub sender.
contract CCIPEndpoint is IMessageEndpoint, IAny2EVMMessageReceiver {
    ISpokeVault public immutable VAULT;
    IRouterClient public immutable ROUTER;
    uint64 public immutable HUB_CHAIN_SELECTOR;
    /// @dev Pinned hub sender, compared by hash (the sender is raw bytes,
    ///      encoded per the hub's chain family — a Sui address here).
    bytes32 public immutable HUB_SENDER_HASH;

    /// @notice Raw hub sender bytes (receiver field for outbound sends).
    bytes public hubSender;
    /// @notice CCIP extraArgs attached to every send; fixed at construction.
    bytes public sendExtraArgs;

    error NotVault(address caller);
    error NotRouter(address caller);
    error BadSource(uint64 chainSelector, bytes sender);

    constructor(
        address vault,
        address router,
        uint64 hubChainSelector,
        bytes memory hubSender_,
        bytes memory sendExtraArgs_
    ) {
        VAULT = ISpokeVault(vault);
        ROUTER = IRouterClient(router);
        HUB_CHAIN_SELECTOR = hubChainSelector;
        HUB_SENDER_HASH = keccak256(hubSender_);
        hubSender = hubSender_;
        sendExtraArgs = sendExtraArgs_;
    }

    /// @inheritdoc IMessageEndpoint
    function send(bytes calldata message) external payable {
        if (msg.sender != address(VAULT)) revert NotVault(msg.sender);
        ROUTER.ccipSend{value: msg.value}(HUB_CHAIN_SELECTOR, _ccipMessage(message));
    }

    /// @inheritdoc IMessageEndpoint
    function quoteFee(bytes calldata message) external view returns (uint256) {
        return ROUTER.getFee(HUB_CHAIN_SELECTOR, _ccipMessage(message));
    }

    /// @notice CCIP delivery: only the router may call, and only with the
    ///         pinned hub source chain selector and sender.
    function ccipReceive(Client.Any2EVMMessage calldata message) external {
        if (msg.sender != address(ROUTER)) revert NotRouter(msg.sender);
        if (
            message.sourceChainSelector != HUB_CHAIN_SELECTOR
                || keccak256(message.sender) != HUB_SENDER_HASH
        ) revert BadSource(message.sourceChainSelector, message.sender);
        bytes calldata data = message.data;
        VAULT.handleMessage(data[:Wire.ENVELOPE_LEN], data[Wire.ENVELOPE_LEN:]);
    }

    function _ccipMessage(bytes calldata message)
        private
        view
        returns (Client.EVM2AnyMessage memory)
    {
        return Client.EVM2AnyMessage({
            receiver: hubSender,
            data: message,
            tokenAmounts: new Client.EVMTokenAmount[](0),
            feeToken: address(0),
            extraArgs: sendExtraArgs
        });
    }
}
