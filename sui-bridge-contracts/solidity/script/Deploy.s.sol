// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script, console} from "forge-std/Script.sol";
import {ChainId} from "../src/libraries/ChainId.sol";
import {Envelope} from "../src/libraries/Envelope.sol";
import {Registry} from "../src/Registry.sol";
import {Outbox} from "../src/Outbox.sol";
import {Inbox} from "../src/Inbox.sol";

/// @notice Deploys Layer 1 messaging to HyperEVM and wires the chain registry +
///         the (1-of-1 launch) ECDSA group key.
///
/// Usage:
///   forge script script/Deploy.s.sol:Deploy \
///     --rpc-url hyperevm_testnet --broadcast
///
/// The deployer is the temporary governance during wiring; if GOVERNANCE is set
/// to a different address (e.g. a multisig), governance is transferred at the
/// end. See .env.example for the inputs.
contract Deploy is Script {
    function run() external {
        uint256 pk = vm.envUint("PRIVATE_KEY");
        address deployer = vm.addr(pk);
        address governance = vm.envOr("GOVERNANCE", deployer);
        address guardian = vm.envOr("GUARDIAN", deployer);
        address groupAddress = vm.envAddress("GROUP_ADDRESS"); // ECDSA group address

        uint32 hyperLocal = uint32(vm.envOr("HYPER_LOCAL_CHAIN_ID", uint256(998)));
        uint32 suiLocal = uint32(vm.envOr("SUI_LOCAL_ID", uint256(0)));
        uint64 hyperConfirmations = uint64(vm.envOr("HYPER_CONFIRMATIONS", uint256(12)));

        uint32 hyperId = ChainId.encode(ChainId.FAMILY_EVM, hyperLocal);
        uint32 suiId = ChainId.encode(ChainId.FAMILY_SUI, suiLocal);

        vm.startBroadcast(pk);

        // Deployer holds governance during wiring so it can register entries.
        Registry registry = new Registry(deployer, guardian);
        Inbox inbox = new Inbox(registry, hyperId);
        Outbox outbox = new Outbox(registry, hyperId);

        // This chain (HyperEVM): finalityKind 0 = confirmation depth.
        registry.registerChain(
            hyperId,
            abi.encodePacked(hyperLocal),
            bytes32(uint256(uint160(address(outbox)))),
            bytes32(uint256(uint160(address(inbox)))),
            0,
            hyperConfirmations
        );
        // Peer chain (Sui): finalityKind 1 = finalized-checkpoint rule. The Sui
        // Outbox/Inbox object ids can be backfilled by governance later.
        registry.registerChain(
            suiId, bytes("sui"), vm.envOr("SUI_OUTBOX", bytes32(0)), vm.envOr("SUI_INBOX", bytes32(0)), 1, 0
        );

        // 1-of-1 launch: a single aggregated ECDSA key, indistinguishable
        // on-chain from a later k-of-n group key.
        registry.registerGroupKey(1, Envelope.SCHEME_ECDSA_SECP256K1, abi.encodePacked(groupAddress));

        if (governance != deployer) {
            registry.transferGovernance(governance);
        }

        vm.stopBroadcast();

        console.log("Registry   ", address(registry));
        console.log("Inbox      ", address(inbox));
        console.log("Outbox     ", address(outbox));
        console.log("hyperId    ", hyperId);
        console.log("suiId      ", suiId);
        console.log("governance ", governance);
        console.log("guardian   ", guardian);
        console.log("groupAddr  ", groupAddress);
    }
}
