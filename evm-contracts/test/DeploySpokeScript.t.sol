// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {DeploySpoke} from "../script/DeploySpoke.s.sol";
import {SpokeVault} from "../src/SpokeVault.sol";
import {RelayerEndpoint} from "../src/endpoints/RelayerEndpoint.sol";
import {TUSDG} from "../src/TUSDG.sol";

/// @notice Proof that `script/DeploySpoke.s.sol` actually runs: drives its
///         deploy flow (TUSDG + dev relayer endpoint + SpokeVault, the
///         testnet-bootstrap shape) and asserts the artifact it writes has
///         exactly the keys `deployment-manager --record-evm-spoke` parses.
contract DeploySpokeScriptTest is Test {
    address internal curator = makeAddr("curator");
    address internal relayerBot = makeAddr("relayerBot");

    string internal constant ARTIFACT = "deployments/test-harness.json";

    function _setBaseEnv(DeploySpoke script) internal {
        vm.setEnv("SPOKE_NAME", "robinhood");
        vm.setEnv("SPOKE_ID", "3");
        vm.setEnv("PROTOCOL_CHAIN_ID", "257");
        vm.setEnv("HUB_CHAIN_ID", "1");
        vm.setEnv("HUB_APP", vm.toString(bytes32(uint256(0xbeef))));
        vm.setEnv("CURATOR", vm.toString(curator));
        // deployForTest() deploys as the script contract, so making it the
        // admin exercises the role-grant + endpoint-bind path.
        vm.setEnv("ADMIN", vm.toString(address(script)));
        vm.setEnv("DEPLOY_TUSDG", "true");
        vm.setEnv("DEPLOY_RELAYER_ENDPOINT", "true");
        vm.setEnv("RELAYER_ADDRESS", vm.toString(relayerBot));
        // LZ / CCIP external addresses absent → their stubs are skipped.
        vm.setEnv("LZ_ENDPOINT", vm.toString(address(0)));
        vm.setEnv("CCIP_ROUTER", vm.toString(address(0)));
        vm.setEnv("DEPLOY_ENV", "test-harness");
    }

    function test_deploy_script_runs_and_artifact_has_the_right_keys() public {
        DeploySpoke script = new DeploySpoke();
        _setBaseEnv(script);

        DeploySpoke.Deployed memory d = script.deployForTest();

        // ── deployed shape ──
        assertTrue(d.vault != address(0), "vault deployed");
        assertTrue(d.usdg != address(0), "tusdg deployed");
        assertTrue(d.relayerEndpoint != address(0), "relayer endpoint deployed");
        assertEq(d.layerzeroEndpoint, address(0), "no LZ stub without LZ_ENDPOINT");
        assertEq(d.ccipEndpoint, address(0), "no CCIP stub without CCIP_ROUTER");

        SpokeVault vault = SpokeVault(payable(d.vault));
        assertEq(vault.SPOKE_ID(), 3);
        assertEq(vault.LOCAL_CHAIN_ID(), 257);
        assertEq(vault.HUB_CHAIN_ID(), 1);
        assertEq(vault.HUB_APP(), bytes32(uint256(0xbeef)));
        assertEq(vault.curator(), curator);
        assertEq(vault.activeEndpoint(), d.relayerEndpoint);

        // §6.1 role grants (admin == creator path).
        assertTrue(vault.hasRole(vault.WHITELIST_ROLE(), address(script)));
        assertTrue(vault.hasRole(vault.PAUSER_ROLE(), address(script)));
        RelayerEndpoint relayer = RelayerEndpoint(d.relayerEndpoint);
        assertTrue(relayer.hasRole(relayer.RELAYER_ROLE(), relayerBot));

        // The relayer endpoint really is wired to the vault (address
        // prediction held): a RELAYER_ROLE deliver reaches the vault.
        assertEq(address(relayer.VAULT()), d.vault);
        // TUSDG is the recorded asset and mints like the faucet mock.
        TUSDG(d.usdg).mint(address(this), 1e6);
        assertEq(TUSDG(d.usdg).decimals(), 6);

        // ── artifact keys (the --record-evm-spoke contract) ──
        string memory json = vm.readFile(ARTIFACT);
        assertEq(vm.parseJsonString(json, ".name"), "robinhood");
        assertEq(vm.parseJsonUint(json, ".spokeId"), 3);
        assertEq(vm.parseJsonUint(json, ".protocolChainId"), 257);
        assertEq(vm.parseJsonUint(json, ".evmChainId"), block.chainid);
        assertEq(vm.parseJsonAddress(json, ".spokeVault"), d.vault);
        assertEq(vm.parseJsonAddress(json, ".relayerEndpoint"), d.relayerEndpoint);
        // Undeployed endpoints are OMITTED, not null.
        assertFalse(vm.keyExistsJson(json, ".layerzeroEndpoint"));
        assertFalse(vm.keyExistsJson(json, ".ccipEndpoint"));
        assertEq(vm.parseJsonAddress(json, ".usdg.address"), d.usdg);
        assertEq(vm.parseJsonUint(json, ".usdg.decimals"), 6);
        assertEq(vm.parseJsonUint(json, ".usdg.assetCode"), 1);
        assertEq(vm.parseJsonUint(json, ".deployBlock"), block.number);
        assertEq(vm.parseJsonAddress(json, ".deployer"), address(script));
        // deployedAt is RFC 3339: 2xxx-xx-xxTxx:xx:xxZ.
        bytes memory at = bytes(vm.parseJsonString(json, ".deployedAt"));
        assertEq(at.length, 20, "deployedAt length");
        assertEq(at[4], "-");
        assertEq(at[10], "T");
        assertEq(at[19], "Z");

        vm.removeFile(ARTIFACT);
    }

    function test_deploy_script_refuses_zero_endpoints() public {
        DeploySpoke script = new DeploySpoke();
        _setBaseEnv(script);
        vm.setEnv("DEPLOY_RELAYER_ENDPOINT", "false");
        vm.expectRevert(bytes("no endpoint requested (see DEPLOY_RELAYER_ENDPOINT / LZ_ENDPOINT / CCIP_ROUTER)"));
        script.deployForTest();
    }
}
