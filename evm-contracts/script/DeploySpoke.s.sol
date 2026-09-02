// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {Script, console2} from "forge-std/Script.sol";
import {IERC20Metadata} from "@openzeppelin/contracts/token/ERC20/extensions/IERC20Metadata.sol";
import {SpokeVault} from "../src/SpokeVault.sol";
import {TUSDG} from "../src/TUSDG.sol";
import {RelayerEndpoint} from "../src/endpoints/RelayerEndpoint.sol";
import {LayerZeroEndpoint} from "../src/endpoints/LayerZeroEndpoint.sol";
import {CCIPEndpoint} from "../src/endpoints/CCIPEndpoint.sol";

/// @notice Deploys one EVM spoke (multichain-vault-plan §7, runbook step 3):
///         optional TUSDG mock + the requested endpoints + a SpokeVault wired
///         to them, grants the §6.1 operational roles, and writes the deploy
///         artifact the redeploy pipeline's `--record-evm-spoke` pass folds
///         into rust-backend/deployments.json — the ONE place addresses live.
///
/// Environment variables (required unless noted):
///   SPOKE_NAME              spoke key in deployments.json (e.g. "robinhood")
///   SPOKE_ID                hub-side spoke id (bind_spoke key)
///   PROTOCOL_CHAIN_ID       this spoke's protocol chain id (envelope ns)
///   HUB_CHAIN_ID            hub protocol chain id (default 1)
///   HUB_APP                 bytes32 hub vault object id
///   CURATOR                 curator's spoke address
///   ADMIN                   DEFAULT_ADMIN_ROLE holder (deployer EOA at
///                           first per §6.1; role grants + extra endpoint
///                           binds are skipped when ADMIN != broadcaster)
///   ADMIN_TRANSFER_DELAY    seconds (default 3 days)
///   DEPOSIT_TIMEOUT         seconds until reclaim (default 24h)
///   HEARTBEAT_TIMEOUT       seconds of hub silence → risk_off (default 6h)
///   USDG_ASSET_CODE         spoke-local asset code (default 1)
///   DEPLOY_TUSDG            deploy the TUSDG mock (default false;
///                           testnet only)
///   USDG_ADDRESS            existing USDG token (required when
///                           DEPLOY_TUSDG=false)
///   DEPLOY_RELAYER_ENDPOINT deploy the dev RelayerEndpoint (default false;
///                           NEVER in production)
///   RELAYER_ADDRESS         optional RELAYER_ROLE grantee on the dev
///                           endpoint (default: none)
///   LZ_ENDPOINT             optional LayerZero endpoint address; when set,
///                           a LayerZeroEndpoint is deployed and HUB_EID +
///                           HUB_PEER (+ optional LZ_SEND_OPTIONS bytes)
///                           become required. Absent → skipped and omitted
///                           from the artifact.
///   CCIP_ROUTER             optional CCIP router address; when set, a
///                           CCIPEndpoint is deployed and CCIP_HUB_SELECTOR
///                           + CCIP_HUB_SENDER (+ optional
///                           CCIP_SEND_EXTRA_ARGS bytes) become required.
///                           Absent → skipped and omitted.
///   DEPLOY_ENV              artifact slot: deployments/<env>.json
///                           (default "staging")
///
/// At least one endpoint must be requested (the vault constructor needs an
/// active endpoint). Initial active endpoint: the dev relayer when deployed
/// (testnet bootstrap), else LayerZero, else CCIP — hub `ConfigSync` is the
/// only thing that switches it afterwards.
contract DeploySpoke is Script {
    // Local endpoint-binding slots (`SpokeVault.endpointById` keys, carried
    // in `ConfigSync.endpoint`). Purely per-spoke identifiers; the hub's
    // spoke binding refers to these when switching transports.
    uint8 public constant RELAYER_EP_ID = 1;
    uint8 public constant LZ_EP_ID = 2;
    uint8 public constant CCIP_EP_ID = 3;

    struct Params {
        string name;
        string env;
        uint64 spokeId;
        uint64 protocolChainId;
        uint64 hubChainId;
        bytes32 hubApp;
        address curator;
        address admin;
        uint48 adminTransferDelay;
        uint64 depositTimeout;
        uint64 heartbeatTimeout;
        uint8 assetCode;
        bool deployTusdg;
        bool deployRelayer;
        address relayerGrantee;
        address lzEndpoint;
        address ccipRouter;
    }

    struct Deployed {
        Params p;
        address usdg;
        uint8 usdgDecimals;
        address vault;
        address relayerEndpoint;
        address layerzeroEndpoint;
        address ccipEndpoint;
        uint256 deployBlock;
        address deployer;
    }

    function run() external {
        vm.startBroadcast();
        (, address broadcaster,) = vm.readCallers();
        Deployed memory d = _deploy(broadcaster);
        vm.stopBroadcast();
        _writeArtifact(d);
    }

    /// @dev Test entry: deploys as this contract (no broadcast) and writes
    ///      the artifact, so `forge test` proves the whole flow.
    function deployForTest() external returns (Deployed memory d) {
        d = _deploy(address(this));
        _writeArtifact(d);
    }

    function _params() internal view returns (Params memory p) {
        p.name = vm.envString("SPOKE_NAME");
        p.env = vm.envOr("DEPLOY_ENV", string("staging"));
        p.spokeId = uint64(vm.envUint("SPOKE_ID"));
        p.protocolChainId = uint64(vm.envUint("PROTOCOL_CHAIN_ID"));
        p.hubChainId = uint64(vm.envOr("HUB_CHAIN_ID", uint256(1)));
        p.hubApp = vm.envBytes32("HUB_APP");
        p.curator = vm.envAddress("CURATOR");
        p.admin = vm.envAddress("ADMIN");
        p.adminTransferDelay = uint48(vm.envOr("ADMIN_TRANSFER_DELAY", uint256(3 days)));
        p.depositTimeout = uint64(vm.envOr("DEPOSIT_TIMEOUT", uint256(24 hours)));
        p.heartbeatTimeout = uint64(vm.envOr("HEARTBEAT_TIMEOUT", uint256(6 hours)));
        p.assetCode = uint8(vm.envOr("USDG_ASSET_CODE", uint256(1)));
        p.deployTusdg = vm.envOr("DEPLOY_TUSDG", false);
        p.deployRelayer = vm.envOr("DEPLOY_RELAYER_ENDPOINT", false);
        p.relayerGrantee = vm.envOr("RELAYER_ADDRESS", address(0));
        p.lzEndpoint = vm.envOr("LZ_ENDPOINT", address(0));
        p.ccipRouter = vm.envOr("CCIP_ROUTER", address(0));
    }

    function _deploy(address creator) internal returns (Deployed memory d) {
        Params memory p = _params();
        d.p = p;
        d.deployer = creator;
        d.deployBlock = block.number;

        // 1. Deposit asset: TUSDG mock (testnet) or the real token.
        if (p.deployTusdg) {
            d.usdg = address(new TUSDG());
        } else {
            d.usdg = vm.envAddress("USDG_ADDRESS");
        }
        d.usdgDecimals = IERC20Metadata(d.usdg).decimals();

        // 2. Endpoints need the vault address at construction and vice
        //    versa: predict the vault's address (creator nonce + one CREATE
        //    per endpoint) exactly like test/utils/SpokeTestBase.sol.
        uint256 endpointCount =
            (p.deployRelayer ? 1 : 0) + (p.lzEndpoint != address(0) ? 1 : 0)
            + (p.ccipRouter != address(0) ? 1 : 0);
        require(endpointCount > 0, "no endpoint requested (see DEPLOY_RELAYER_ENDPOINT / LZ_ENDPOINT / CCIP_ROUTER)");
        address predictedVault =
            vm.computeCreateAddress(creator, vm.getNonce(creator) + endpointCount);

        if (p.deployRelayer) {
            d.relayerEndpoint =
                address(new RelayerEndpoint(predictedVault, p.adminTransferDelay, p.admin));
        }
        if (p.lzEndpoint != address(0)) {
            d.layerzeroEndpoint = address(
                new LayerZeroEndpoint(
                    predictedVault,
                    p.lzEndpoint,
                    uint32(vm.envUint("HUB_EID")),
                    vm.envBytes32("HUB_PEER"),
                    vm.envOr("LZ_SEND_OPTIONS", bytes(""))
                )
            );
        }
        if (p.ccipRouter != address(0)) {
            d.ccipEndpoint = address(
                new CCIPEndpoint(
                    predictedVault,
                    p.ccipRouter,
                    uint64(vm.envUint("CCIP_HUB_SELECTOR")),
                    vm.envBytes("CCIP_HUB_SENDER"),
                    vm.envOr("CCIP_SEND_EXTRA_ARGS", bytes(""))
                )
            );
        }

        // 3. The vault, wired to the initially-active endpoint.
        (uint8 activeId, address activeEndpoint) = _activeEndpoint(d);
        uint8[] memory codes = new uint8[](1);
        codes[0] = p.assetCode;
        address[] memory tokens = new address[](1);
        tokens[0] = d.usdg;
        SpokeVault vault = new SpokeVault(
            SpokeVault.Config({
                admin: p.admin,
                adminTransferDelay: p.adminTransferDelay,
                curator: p.curator,
                endpointId: activeId,
                endpoint: activeEndpoint,
                spokeId: p.spokeId,
                localChainId: p.protocolChainId,
                hubChainId: p.hubChainId,
                hubApp: p.hubApp,
                assetCodes: codes,
                assetTokens: tokens,
                payoutAssetCode: p.assetCode,
                depositTimeout: p.depositTimeout,
                heartbeatTimeout: p.heartbeatTimeout
            })
        );
        require(address(vault) == predictedVault, "vault address prediction failed");
        d.vault = address(vault);

        // 4. Roles per §6.1 + standby endpoint binds. All are
        //    DEFAULT_ADMIN_ROLE calls, so they only work when the
        //    broadcaster IS the admin (the §6.1 bootstrap: deployer EOA
        //    first, multisig later). Otherwise the admin runs them by hand.
        if (p.admin == creator) {
            vault.grantRole(vault.WHITELIST_ROLE(), p.admin);
            vault.grantRole(vault.PAUSER_ROLE(), p.admin);
            if (d.relayerEndpoint != address(0) && activeEndpoint != d.relayerEndpoint) {
                vault.bindEndpoint(RELAYER_EP_ID, d.relayerEndpoint);
            }
            if (d.layerzeroEndpoint != address(0) && activeEndpoint != d.layerzeroEndpoint) {
                vault.bindEndpoint(LZ_EP_ID, d.layerzeroEndpoint);
            }
            if (d.ccipEndpoint != address(0) && activeEndpoint != d.ccipEndpoint) {
                vault.bindEndpoint(CCIP_EP_ID, d.ccipEndpoint);
            }
            if (d.relayerEndpoint != address(0) && p.relayerGrantee != address(0)) {
                RelayerEndpoint relayer = RelayerEndpoint(d.relayerEndpoint);
                relayer.grantRole(relayer.RELAYER_ROLE(), p.relayerGrantee);
            }
        } else {
            console2.log("ADMIN != broadcaster: skipping role grants + standby endpoint binds");
        }
    }

    /// @dev Initial active endpoint: dev relayer when deployed, else LZ,
    ///      else CCIP. Hub ConfigSync owns every later switch.
    function _activeEndpoint(Deployed memory d) internal pure returns (uint8, address) {
        if (d.relayerEndpoint != address(0)) return (RELAYER_EP_ID, d.relayerEndpoint);
        if (d.layerzeroEndpoint != address(0)) return (LZ_EP_ID, d.layerzeroEndpoint);
        return (CCIP_EP_ID, d.ccipEndpoint);
    }

    /// @dev Writes deployments/<env>.json in the exact shape
    ///      `deployment-manager --record-evm-spoke` parses (its
    ///      SpokeArtifact struct). Undeployed endpoints are OMITTED, not
    ///      null.
    function _writeArtifact(Deployed memory d) internal {
        string memory usdgObj = vm.serializeAddress("usdg", "address", d.usdg);
        vm.serializeUint("usdg", "decimals", d.usdgDecimals);
        usdgObj = vm.serializeUint("usdg", "assetCode", d.p.assetCode);

        vm.serializeString("artifact", "name", d.p.name);
        vm.serializeUint("artifact", "spokeId", d.p.spokeId);
        vm.serializeUint("artifact", "protocolChainId", d.p.protocolChainId);
        vm.serializeUint("artifact", "evmChainId", block.chainid);
        vm.serializeAddress("artifact", "spokeVault", d.vault);
        if (d.relayerEndpoint != address(0)) {
            vm.serializeAddress("artifact", "relayerEndpoint", d.relayerEndpoint);
        }
        if (d.layerzeroEndpoint != address(0)) {
            vm.serializeAddress("artifact", "layerzeroEndpoint", d.layerzeroEndpoint);
        }
        if (d.ccipEndpoint != address(0)) {
            vm.serializeAddress("artifact", "ccipEndpoint", d.ccipEndpoint);
        }
        vm.serializeString("artifact", "usdg", usdgObj);
        vm.serializeUint("artifact", "deployBlock", d.deployBlock);
        vm.serializeAddress("artifact", "deployer", d.deployer);
        string memory artifact =
            vm.serializeString("artifact", "deployedAt", _rfc3339(block.timestamp));

        vm.createDir("deployments", true);
        string memory path = string.concat("deployments/", d.p.env, ".json");
        vm.writeJson(artifact, path);
        console2.log("spoke artifact written:", path);
    }

    // ── RFC 3339 from a unix timestamp (civil-from-days, H. Hinnant) ──

    function _rfc3339(uint256 ts) internal pure returns (string memory) {
        uint256 z = ts / 86400 + 719468;
        uint256 era = z / 146097;
        uint256 doe = z - era * 146097;
        uint256 yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        uint256 y = yoe + era * 400;
        uint256 doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        uint256 mp = (5 * doy + 2) / 153;
        uint256 day = doy - (153 * mp + 2) / 5 + 1;
        uint256 month = mp < 10 ? mp + 3 : mp - 9;
        if (month <= 2) y += 1;
        uint256 secs = ts % 86400;
        return string.concat(
            _pad(y, 4), "-", _pad(month, 2), "-", _pad(day, 2), "T",
            _pad(secs / 3600, 2), ":", _pad((secs / 60) % 60, 2), ":", _pad(secs % 60, 2), "Z"
        );
    }

    function _pad(uint256 v, uint256 width) internal pure returns (string memory s) {
        s = vm.toString(v);
        while (bytes(s).length < width) {
            s = string.concat("0", s);
        }
    }
}
