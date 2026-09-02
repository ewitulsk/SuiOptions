// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {AccessControlDefaultAdminRules} from
    "@openzeppelin/contracts/access/extensions/AccessControlDefaultAdminRules.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

import {Wire} from "./lib/Wire.sol";
import {IMessageEndpoint} from "./interfaces/IMessageEndpoint.sol";
import {ISpokeIntegration} from "./interfaces/ISpokeIntegration.sol";
import {ISpokeVault} from "./interfaces/ISpokeVault.sol";

/// @title SpokeVault — dumb spoke vault for the multichain trading vault
/// @notice Implements the spoke side of docs/multichain-vault-plan.md:
///         per-asset fund states (§1), deposit/reclaim (§4), hub-directed
///         withdrawals with a FIFO payout queue (§5), the curator
///         integration interface (§6), roles (§6.1), the message fee pot
///         (§2.4), and `handleMessage` dispatch with per-lane seq ordering
///         (§2.1). The spoke never values anything and never computes
///         shares or NAV — the hub is the single source of truth; the
///         share amounts recorded here are a NON-authoritative UX mirror.
contract SpokeVault is AccessControlDefaultAdminRules, ISpokeVault {
    using SafeERC20 for IERC20;

    // ────────────────────────────── roles ──────────────────────────────

    /// @notice Manages the spoke depositor allowlist.
    bytes32 public constant WHITELIST_ROLE = keccak256("WHITELIST_ROLE");
    /// @notice Local break-glass pause of deposits and payouts. The hub
    ///         `ConfigSync` pause remains the governed path.
    bytes32 public constant PAUSER_ROLE = keccak256("PAUSER_ROLE");
    // NOTE: RELAYER_ROLE lives on RelayerEndpoint, not the vault. Curator,
    // active endpoint, and the integration set are NOT roles — only hub
    // `ConfigSync` changes them (plan §6.1).

    // ─────────────────────────────── types ─────────────────────────────

    enum DepositStatus {
        None,
        Pending, // escrowed, awaiting hub ACK
        Acked, // hub accepted; funds moved pending → active
        Refunded, // hub rejected; escrow auto-refunded
        Reclaimed // depositor reclaimed after DEPOSIT_TIMEOUT
    }

    struct DepositRecord {
        address depositor;
        uint8 asset;
        uint8 tranche;
        DepositStatus status;
        uint128 amount;
        uint64 ts; // spoke block timestamp (seconds)
    }

    struct WithdrawRecord {
        address user;
        uint8 tranche;
        bool all;
        bool open; // awaiting WithdrawAck
        uint128 shares;
    }

    /// @notice Per-asset fund states (plan §1). `pending` is escrowed and
    ///         unusable until hub ACK; `active` backs NAV; `reserved` is
    ///         owed to hub-ACK'd withdrawals in the payout queue.
    struct FundState {
        uint128 pending;
        uint128 active;
        uint128 reserved;
    }

    struct Payout {
        address user;
        uint64 requestSeq;
        uint128 owed; // total pay_amount from the hub WithdrawAck
        uint128 reservedAmt; // portion already moved active → reserved
    }

    /// @notice Constructor configuration (bootstrap values; curator,
    ///         endpoint, and integrations are hub-governed thereafter).
    struct Config {
        address admin;
        uint48 adminTransferDelay;
        address curator;
        uint8 endpointId;
        address endpoint;
        uint64 spokeId;
        uint64 localChainId; // protocol chain id of this spoke
        uint64 hubChainId; // protocol chain id of the hub
        bytes32 hubApp; // hub vault app id (Sui object id)
        uint8[] assetCodes;
        address[] assetTokens;
        uint8 payoutAssetCode; // asset all withdrawal payouts denominate in
        uint64 depositTimeout; // seconds until reclaim is allowed
        uint64 heartbeatTimeout; // seconds of hub silence → local risk_off
    }

    // ───────────────────────────── errors ──────────────────────────────

    error NotEndpoint(address caller);
    error BadOrigin();
    error SeqNotIncreasing(uint64 got, uint64 last);
    error UnexpectedMsgType(uint8 msgType);
    error NotWhitelisted(address user);
    error VaultPaused();
    error UnknownAsset(uint8 assetCode);
    error BadTranche(uint8 tranche);
    error ZeroAmount();
    error AmountTooLarge(uint256 amount);
    error FeePotInsufficient(uint256 needed, uint256 available);
    error NotDepositor(address caller);
    error DepositNotPending(uint64 depositSeq);
    error TimeoutNotElapsed(uint64 readyAt);
    error WithdrawInFlight(uint64 requestSeq);
    error ZeroShares();
    error NotCurator(address caller);
    error IntegrationNotRegistered(address integration);
    error PayoutsQueued();
    error RiskOffActive();
    error InsufficientActive(uint128 wanted, uint128 have);
    error IntegrationsRootMismatch(bytes32 computed, bytes32 expected);
    error IntegrationsNotSorted();
    error ZeroAddress();
    error ConfigLengthMismatch();

    // ───────────────────────────── events ──────────────────────────────

    event Deposited(
        uint64 indexed depositSeq, address indexed depositor, uint8 asset, uint128 amount, uint8 tranche
    );
    event DepositAcked(uint64 indexed depositSeq, uint128 shares);
    event DepositRejected(uint64 indexed depositSeq);
    event DepositReclaimed(uint64 indexed depositSeq);
    /// @notice A DepositAck arrived for a seq that is reclaimed, refunded,
    ///         or unknown. Recorded and alarmed; the message lane continues.
    event AlarmAckForReclaimed(uint64 indexed depositSeq, bool accepted, uint128 shares);
    /// @notice A WithdrawAck arrived for a request that is not open or
    ///         names a different user. Alarmed; the lane continues.
    event AlarmUnknownWithdrawAck(uint64 indexed requestSeq, bytes32 user, uint128 payAmount);
    /// @notice Non-authoritative share mirror updated (UX only; the hub
    ///         ledger is the single source of truth).
    event SharesRecorded(address indexed user, uint8 indexed tranche, uint256 mirrorTotal);
    event WithdrawRequested(
        uint64 indexed requestSeq, address indexed user, uint8 tranche, uint128 shares, bool all
    );
    event WithdrawRejected(uint64 indexed requestSeq);
    event PayoutQueued(uint64 indexed requestSeq, address indexed user, uint8 asset, uint128 owed);
    event PayoutPaid(uint64 indexed requestSeq, address indexed user, uint8 asset, uint128 amount);
    event ConfigSynced(bool paused, bool riskOff, address curator, uint8 endpointId, bytes32 integrationsRoot);
    /// @notice ConfigSync named an endpoint id with no bound candidate;
    ///         the active endpoint is left unchanged. Alarmed.
    event AlarmUnknownEndpointId(uint8 endpointId);
    event EndpointBound(uint8 indexed endpointId, address endpoint);
    event EndpointSwitched(uint8 indexed endpointId, address endpoint);
    event IntegrationsSet(bytes32 indexed root, address[] integrations);
    event ExtendedTo(address indexed integration, uint8 asset, uint256 amount);
    event FundsReturned(address indexed integration, uint8 asset, uint256 amount, address from);
    event PayoutsFunded(uint8 asset, uint256 amount, address from);
    event FeesFunded(address indexed from, uint256 amount);
    event LocalPauseSet(bool paused);
    event WhitelistEnabledSet(bool enabled);
    event WhitelistSet(address indexed user, bool allowed);
    event StateSynced(uint64 seq);

    // ─────────────────────────── immutables ────────────────────────────

    uint64 public immutable SPOKE_ID;
    uint64 public immutable LOCAL_CHAIN_ID;
    uint64 public immutable HUB_CHAIN_ID;
    bytes32 public immutable HUB_APP;
    uint64 public immutable DEPOSIT_TIMEOUT;
    uint64 public immutable HEARTBEAT_TIMEOUT;
    uint8 public immutable PAYOUT_ASSET_CODE;

    // ───────────────────────────── storage ─────────────────────────────

    /// @notice Registered assets: spoke-local code → token.
    mapping(uint8 => IERC20) public assets;
    uint8[] public assetCodes;

    mapping(uint8 => FundState) public funds;

    // Depositor whitelist (disabled or empty = open).
    bool public whitelistEnabled;
    mapping(address => bool) public whitelisted;

    // Deposits.
    uint64 public depositSeq;
    mapping(uint64 => DepositRecord) public deposits;

    // Withdrawals.
    uint64 public requestSeq;
    mapping(uint64 => WithdrawRecord) public withdrawals;
    /// @notice One in-flight request per (user, tranche): the open requestSeq, 0 = none.
    mapping(address => mapping(uint8 => uint64)) public inFlightRequest;

    // FIFO payout queue, per asset.
    mapping(uint8 => mapping(uint256 => Payout)) public payoutQueue;
    mapping(uint8 => uint256) public payoutHead;
    mapping(uint8 => uint256) public payoutTail;
    /// @notice Unpaid queued payouts across all assets; > 0 freezes `extendTo`.
    uint256 public totalQueuedPayouts;

    /// @notice Non-authoritative per-(user, tranche) share mirror (UX only).
    mapping(address => mapping(uint8 => uint256)) public shareMirror;

    // Messaging.
    address public activeEndpoint;
    /// @notice Admin-bound endpoint candidates; ConfigSync switches among them by id.
    mapping(uint8 => address) public endpointById;
    uint64 public lastInboundSeq;
    uint64 public outboundSeq;
    /// @notice Timestamp of the last inbound hub message (heartbeat).
    uint64 public lastInboundAt;

    // Hub-governed config (ConfigSync only).
    bool public hubPaused;
    bool public localPaused;
    bool public riskOff;
    address public curator;
    bytes32 public integrationsRoot;

    // Integration set (must match integrationsRoot; see setIntegrations).
    address[] public integrations;
    mapping(address => bool) public isIntegration;
    /// @notice Informational: funds extended to an integration and not yet returned.
    mapping(address => uint256) public extendedOutstanding;

    /// @notice Native fee pot for outbound message fees (plan §2.4).
    ///         Tracked separately from any other native the contract holds.
    uint256 public feePot;

    // ──────────────────────────── constructor ──────────────────────────

    constructor(Config memory cfg)
        AccessControlDefaultAdminRules(cfg.adminTransferDelay, cfg.admin)
    {
        if (cfg.curator == address(0) || cfg.endpoint == address(0)) revert ZeroAddress();
        if (cfg.assetCodes.length != cfg.assetTokens.length || cfg.assetCodes.length == 0) {
            revert ConfigLengthMismatch();
        }
        SPOKE_ID = cfg.spokeId;
        LOCAL_CHAIN_ID = cfg.localChainId;
        HUB_CHAIN_ID = cfg.hubChainId;
        HUB_APP = cfg.hubApp;
        DEPOSIT_TIMEOUT = cfg.depositTimeout;
        HEARTBEAT_TIMEOUT = cfg.heartbeatTimeout;
        PAYOUT_ASSET_CODE = cfg.payoutAssetCode;

        bool payoutAssetSeen = false;
        for (uint256 i = 0; i < cfg.assetCodes.length; i++) {
            uint8 code = cfg.assetCodes[i];
            if (cfg.assetTokens[i] == address(0)) revert ZeroAddress();
            if (address(assets[code]) != address(0)) revert ConfigLengthMismatch();
            assets[code] = IERC20(cfg.assetTokens[i]);
            assetCodes.push(code);
            if (code == cfg.payoutAssetCode) payoutAssetSeen = true;
        }
        if (!payoutAssetSeen) revert UnknownAsset(cfg.payoutAssetCode);

        curator = cfg.curator;
        activeEndpoint = cfg.endpoint;
        endpointById[cfg.endpointId] = cfg.endpoint;
        emit EndpointBound(cfg.endpointId, cfg.endpoint);
        lastInboundAt = uint64(block.timestamp);
    }

    // ──────────────────────────── views ────────────────────────────────

    /// @notice Deposits and payouts halt when either the hub-propagated
    ///         pause or the local break-glass pause is set.
    function paused() public view returns (bool) {
        return hubPaused || localPaused;
    }

    /// @notice risk_off as this spoke enforces it: the hub-propagated flag,
    ///         OR'd with the stale-heartbeat freeze — no inbound hub
    ///         message for HEARTBEAT_TIMEOUT means integrations freeze
    ///         while deposits and payouts continue (plan §3).
    function effectiveRiskOff() public view returns (bool) {
        return riskOff || block.timestamp > uint256(lastInboundAt) + HEARTBEAT_TIMEOUT;
    }

    function integrationCount() external view returns (uint256) {
        return integrations.length;
    }

    /// @notice Number of unpaid queued payouts for an asset.
    function queueLength(uint8 assetCode) external view returns (uint256) {
        return payoutTail[assetCode] - payoutHead[assetCode];
    }

    // ──────────────────────────── deposits ─────────────────────────────

    /// @notice Deposit `amount` of the registered asset `assetCode` into
    ///         tranche `tranche`. Escrows the tokens as `pending` and sends
    ///         a `DepositNotice` to the hub (fee pot pays the transport
    ///         fee). Funds are unusable until the hub ACKs. The tranche is
    ///         only range-checked here — the hub owns tranche policy.
    function deposit(uint8 assetCode, uint256 amount, uint8 tranche) external {
        _checkWhitelist(msg.sender);
        if (paused()) revert VaultPaused();
        IERC20 token = _asset(assetCode);
        if (amount == 0) revert ZeroAmount();
        if (amount > type(uint128).max) revert AmountTooLarge(amount);
        if (tranche > 2) revert BadTranche(tranche);

        uint64 seq = ++depositSeq;
        deposits[seq] = DepositRecord({
            depositor: msg.sender,
            asset: assetCode,
            tranche: tranche,
            status: DepositStatus.Pending,
            amount: uint128(amount),
            ts: uint64(block.timestamp)
        });
        funds[assetCode].pending += uint128(amount);
        token.safeTransferFrom(msg.sender, address(this), amount);
        emit Deposited(seq, msg.sender, assetCode, uint128(amount), tranche);

        _send(
            Wire.encodeDepositNotice(
                _outboundEnvelope(),
                Wire.DepositNotice({
                    spokeId: SPOKE_ID,
                    depositSeq: seq,
                    depositor: bytes32(uint256(uint160(msg.sender))),
                    asset: assetCode,
                    amount: uint128(amount),
                    tranche: tranche,
                    tsMs: uint64(block.timestamp) * 1000
                })
            )
        );
    }

    /// @notice Reclaim an escrowed deposit that received no hub ACK within
    ///         DEPOSIT_TIMEOUT. Depositor-only; refunds the escrow and
    ///         marks the seq dead (a later ACK is alarmed, not applied).
    function reclaim(uint64 depositSeq_) external {
        DepositRecord storage d = deposits[depositSeq_];
        if (d.status != DepositStatus.Pending) revert DepositNotPending(depositSeq_);
        if (msg.sender != d.depositor) revert NotDepositor(msg.sender);
        uint64 readyAt = d.ts + DEPOSIT_TIMEOUT;
        if (block.timestamp < readyAt) revert TimeoutNotElapsed(readyAt);

        d.status = DepositStatus.Reclaimed;
        funds[d.asset].pending -= d.amount;
        assets[d.asset].safeTransfer(d.depositor, d.amount);
        emit DepositReclaimed(depositSeq_);
    }

    // ─────────────────────────── withdrawals ───────────────────────────

    /// @notice Request a share-denominated withdrawal from `tranche`
    ///         (`shares`, or everything when `all`). One in-flight request
    ///         per (user, tranche). Sends a `WithdrawRequest` to the hub in
    ///         the same transaction (fee pot pays); the hub burns shares in
    ///         full at ACK and directs payment via `WithdrawAck`.
    function requestWithdraw(uint8 tranche, uint128 shares, bool all) external {
        _checkWhitelist(msg.sender);
        if (tranche > 2) revert BadTranche(tranche);
        if (!all && shares == 0) revert ZeroShares();
        uint64 existing = inFlightRequest[msg.sender][tranche];
        if (existing != 0) revert WithdrawInFlight(existing);

        uint64 seq = ++requestSeq;
        withdrawals[seq] = WithdrawRecord({
            user: msg.sender,
            tranche: tranche,
            all: all,
            open: true,
            shares: shares
        });
        inFlightRequest[msg.sender][tranche] = seq;
        emit WithdrawRequested(seq, msg.sender, tranche, shares, all);

        _send(
            Wire.encodeWithdrawRequest(
                _outboundEnvelope(),
                Wire.WithdrawRequest({
                    spokeId: SPOKE_ID,
                    requestSeq: seq,
                    user: bytes32(uint256(uint160(msg.sender))),
                    tranche: tranche,
                    shares: shares,
                    all: all
                })
            )
        );
    }

    /// @notice Service the FIFO payout queue for `assetCode` from `active`
    ///         funds. Permissionless. Each fully-funded payout transfers to
    ///         the user and sends a `PayoutReceipt` to the hub.
    function processPayoutQueue(uint8 assetCode) external {
        if (paused()) revert VaultPaused();
        _asset(assetCode);
        _serviceQueue(assetCode);
    }

    // ───────────────────────────── fee pot ─────────────────────────────

    /// @notice Top up the message fee pot. Permissionless (plan §2.4).
    function fundFees() external payable {
        feePot += msg.value;
        emit FeesFunded(msg.sender, msg.value);
    }

    /// @notice Native sent directly also tops up the fee pot.
    receive() external payable {
        feePot += msg.value;
        emit FeesFunded(msg.sender, msg.value);
    }

    // ──────────────────────────── state sync ───────────────────────────

    /// @notice Send a `StateSync` snapshot to the hub: per-asset
    ///         {free = active, reserved}, fee pot balance, concatenated
    ///         raw integration state (capped at 4096 bytes), and the block
    ///         timestamp. Permissionless; the fee pot pays.
    function syncState() external {
        Wire.StateSyncAsset[] memory entries = new Wire.StateSyncAsset[](assetCodes.length);
        for (uint256 i = 0; i < assetCodes.length; i++) {
            uint8 code = assetCodes[i];
            FundState storage f = funds[code];
            entries[i] =
                Wire.StateSyncAsset({asset: code, free: f.active, reserved: f.reserved});
        }
        bytes memory raw;
        for (uint256 i = 0; i < integrations.length; i++) {
            bytes memory r = ISpokeIntegration(integrations[i]).rawState();
            if (raw.length + r.length > Wire.MAX_INTEGRATION_RAW_LEN) break;
            raw = bytes.concat(raw, r);
        }
        if (feePot > type(uint128).max) revert AmountTooLarge(feePot);

        _send(
            Wire.encodeStateSync(
                _outboundEnvelope(),
                Wire.StateSync({
                    spokeId: SPOKE_ID,
                    assets: entries,
                    feePotBalance: uint128(feePot),
                    integrationRaw: raw,
                    tsMs: uint64(block.timestamp) * 1000
                })
            )
        );
        emit StateSynced(outboundSeq);
    }

    // ─────────────────────────── integrations ──────────────────────────

    /// @notice Extend `active` funds to a hub-registered integration.
    ///         Curator-only; reverts while any payout is queued and while
    ///         risk_off (hub-flagged or stale-heartbeat) is in effect.
    function extendTo(address integration, uint8 assetCode, uint256 amount) external {
        if (msg.sender != curator) revert NotCurator(msg.sender);
        if (!isIntegration[integration]) revert IntegrationNotRegistered(integration);
        IERC20 token = _asset(assetCode);
        if (amount == 0) revert ZeroAmount();
        if (amount > type(uint128).max) revert AmountTooLarge(amount);
        if (totalQueuedPayouts != 0) revert PayoutsQueued();
        if (effectiveRiskOff()) revert RiskOffActive();
        FundState storage f = funds[assetCode];
        if (f.active < amount) revert InsufficientActive(uint128(amount), f.active);

        f.active -= uint128(amount);
        extendedOutstanding[integration] += amount;
        token.safeTransfer(integration, amount);
        ISpokeIntegration(integration).onFundsReceived(address(token), amount);
        emit ExtendedTo(integration, assetCode, amount);
    }

    /// @notice Push funds back from an integration into `active`.
    ///         Permissionless: tokens are pulled from the caller. Queued
    ///         payouts are serviced before the funds become usable.
    function returnFunds(address integration, uint8 assetCode, uint256 amount) external {
        IERC20 token = _asset(assetCode);
        if (amount == 0) revert ZeroAmount();
        if (amount > type(uint128).max) revert AmountTooLarge(amount);
        token.safeTransferFrom(msg.sender, address(this), amount);
        uint256 outstanding = extendedOutstanding[integration];
        extendedOutstanding[integration] = amount >= outstanding ? 0 : outstanding - amount;
        funds[assetCode].active += uint128(amount);
        emit FundsReturned(integration, assetCode, amount, msg.sender);
        _serviceQueue(assetCode);
    }

    /// @notice Donate funds directly to the payout queue (then `active`).
    ///         Permissionless: tokens are pulled from the caller.
    function fundPayouts(uint8 assetCode, uint256 amount) external {
        IERC20 token = _asset(assetCode);
        if (amount == 0) revert ZeroAmount();
        if (amount > type(uint128).max) revert AmountTooLarge(amount);
        token.safeTransferFrom(msg.sender, address(this), amount);
        funds[assetCode].active += uint128(amount);
        emit PayoutsFunded(assetCode, amount, msg.sender);
        _serviceQueue(assetCode);
    }

    /// @notice Install the integration set matching the hub-committed root
    ///         from the last ConfigSync. Admin-free: anyone may supply the
    ///         list; it is accepted iff `keccak256(abi.encode(list))`
    ///         equals `integrationsRoot` and the list is strictly ascending
    ///         (the canonical sorted form the hub commits to).
    function setIntegrations(address[] calldata list) external {
        bytes32 computed = keccak256(abi.encode(list));
        if (integrationsRoot == bytes32(0) || computed != integrationsRoot) {
            revert IntegrationsRootMismatch(computed, integrationsRoot);
        }
        for (uint256 i = 1; i < list.length; i++) {
            if (list[i] <= list[i - 1]) revert IntegrationsNotSorted();
        }
        for (uint256 i = 0; i < integrations.length; i++) {
            isIntegration[integrations[i]] = false;
        }
        delete integrations;
        for (uint256 i = 0; i < list.length; i++) {
            if (list[i] == address(0)) revert ZeroAddress();
            integrations.push(list[i]);
            isIntegration[list[i]] = true;
        }
        emit IntegrationsSet(integrationsRoot, list);
    }

    // ───────────────────────────── admin ───────────────────────────────

    /// @notice Bind an endpoint candidate contract to an endpoint id.
    ///         Binding alone changes nothing: only a hub `ConfigSync`
    ///         naming the id activates it (plan §2.3).
    function bindEndpoint(uint8 endpointId, address endpoint) external onlyRole(DEFAULT_ADMIN_ROLE) {
        if (endpoint == address(0)) revert ZeroAddress();
        endpointById[endpointId] = endpoint;
        emit EndpointBound(endpointId, endpoint);
    }

    /// @notice Local break-glass pause (deposits and payouts halt).
    function setLocalPause(bool paused_) external onlyRole(PAUSER_ROLE) {
        localPaused = paused_;
        emit LocalPauseSet(paused_);
    }

    function setWhitelistEnabled(bool enabled) external onlyRole(WHITELIST_ROLE) {
        whitelistEnabled = enabled;
        emit WhitelistEnabledSet(enabled);
    }

    function setWhitelisted(address user, bool allowed) external onlyRole(WHITELIST_ROLE) {
        whitelisted[user] = allowed;
        emit WhitelistSet(user, allowed);
    }

    // ─────────────────────── inbound message lane ──────────────────────

    /// @inheritdoc ISpokeVault
    /// @dev Enforces: caller is the active endpoint; wire version; origin
    ///      matches the hub binding; strictly-increasing per-lane seq.
    ///      Dispatches DepositAck / WithdrawAck / ConfigSync; any spoke →
    ///      hub message type arriving inbound reverts.
    function handleMessage(bytes calldata envelope, bytes calldata payload) external {
        if (msg.sender != activeEndpoint) revert NotEndpoint(msg.sender);
        (uint8 msgType, Wire.Envelope memory env) = Wire.decodeEnvelope(envelope);
        if (
            env.srcChainId != HUB_CHAIN_ID || env.srcApp != HUB_APP
                || env.dstChainId != LOCAL_CHAIN_ID
                || env.dstApp != bytes32(uint256(uint160(address(this))))
        ) revert BadOrigin();
        if (env.seq <= lastInboundSeq) revert SeqNotIncreasing(env.seq, lastInboundSeq);
        lastInboundSeq = env.seq;
        lastInboundAt = uint64(block.timestamp);

        if (msgType == Wire.MSG_DEPOSIT_ACK) {
            _handleDepositAck(Wire.decodeDepositAck(payload));
        } else if (msgType == Wire.MSG_WITHDRAW_ACK) {
            _handleWithdrawAck(Wire.decodeWithdrawAck(payload));
        } else if (msgType == Wire.MSG_CONFIG_SYNC) {
            _handleConfigSync(Wire.decodeConfigSync(payload));
        } else {
            revert UnexpectedMsgType(msgType);
        }
    }

    // ──────────────────────── inbound handlers ─────────────────────────

    function _handleDepositAck(Wire.DepositAck memory a) private {
        DepositRecord storage d = deposits[a.depositSeq];
        if (d.status != DepositStatus.Pending) {
            // Reclaimed/refunded/unknown seq: never revert the lane —
            // record the anomaly and continue (plan §4.4).
            emit AlarmAckForReclaimed(a.depositSeq, a.accepted, a.shares);
            return;
        }
        FundState storage f = funds[d.asset];
        if (a.accepted) {
            d.status = DepositStatus.Acked;
            f.pending -= d.amount;
            f.active += d.amount;
            shareMirror[d.depositor][d.tranche] += a.shares;
            emit DepositAcked(a.depositSeq, a.shares);
            emit SharesRecorded(d.depositor, d.tranche, shareMirror[d.depositor][d.tranche]);
            // NOTE: freshly ACKed deposits do NOT auto-service the payout
            // queue; anyone may drain it via processPayoutQueue().
        } else {
            d.status = DepositStatus.Refunded;
            f.pending -= d.amount;
            assets[d.asset].safeTransfer(d.depositor, d.amount);
            emit DepositRejected(a.depositSeq);
        }
    }

    function _handleWithdrawAck(Wire.WithdrawAck memory a) private {
        WithdrawRecord storage r = withdrawals[a.requestSeq];
        if (!r.open || bytes32(uint256(uint160(r.user))) != a.user) {
            emit AlarmUnknownWithdrawAck(a.requestSeq, a.user, a.payAmount);
            return;
        }
        r.open = false;
        inFlightRequest[r.user][r.tranche] = 0;

        if (a.payAmount == 0) {
            // Hub rejected the request: just unlock.
            emit WithdrawRejected(a.requestSeq);
            return;
        }

        // Update the non-authoritative mirror (hub burned the shares).
        uint256 mirror = shareMirror[r.user][r.tranche];
        uint256 burned = r.all ? mirror : (r.shares > mirror ? mirror : r.shares);
        shareMirror[r.user][r.tranche] = mirror - burned;
        emit SharesRecorded(r.user, r.tranche, mirror - burned);

        // WithdrawAck carries no asset code: payouts denominate in the
        // spoke's deposit asset (PAYOUT_ASSET_CODE).
        uint8 assetCode = PAYOUT_ASSET_CODE;
        FundState storage f = funds[assetCode];
        bool queueEmpty = payoutHead[assetCode] == payoutTail[assetCode];
        if (queueEmpty && !paused() && f.active >= a.payAmount) {
            // Pay immediately from active.
            f.active -= a.payAmount;
            assets[assetCode].safeTransfer(r.user, a.payAmount);
            emit PayoutPaid(a.requestSeq, r.user, assetCode, a.payAmount);
            _sendPayoutReceipt(a.requestSeq, a.payAmount);
        } else {
            // Move what exists to reserved and queue the remainder FIFO.
            payoutQueue[assetCode][payoutTail[assetCode]++] = Payout({
                user: r.user,
                requestSeq: a.requestSeq,
                owed: a.payAmount,
                reservedAmt: 0
            });
            totalQueuedPayouts += 1;
            emit PayoutQueued(a.requestSeq, r.user, assetCode, a.payAmount);
            _serviceQueue(assetCode);
        }
    }

    function _handleConfigSync(Wire.ConfigSync memory c) private {
        hubPaused = c.paused;
        riskOff = c.riskOff;
        curator = address(uint160(uint256(c.curator)));
        integrationsRoot = c.integrationsRoot;

        address ep = endpointById[c.endpoint];
        if (ep == address(0)) {
            emit AlarmUnknownEndpointId(c.endpoint);
        } else if (ep != activeEndpoint) {
            activeEndpoint = ep;
            emit EndpointSwitched(c.endpoint, ep);
        }
        emit ConfigSynced(c.paused, c.riskOff, curator, c.endpoint, c.integrationsRoot);
    }

    // ─────────────────────────── internals ─────────────────────────────

    function _asset(uint8 assetCode) private view returns (IERC20 token) {
        token = assets[assetCode];
        if (address(token) == address(0)) revert UnknownAsset(assetCode);
    }

    function _checkWhitelist(address user) private view {
        if (whitelistEnabled && !whitelisted[user]) revert NotWhitelisted(user);
    }

    /// @dev FIFO drain: move `active` into `reserved` toward the head
    ///      payout; once a payout is fully reserved, pay it out and send
    ///      the `PayoutReceipt`. No payouts while paused.
    function _serviceQueue(uint8 assetCode) private {
        if (paused()) return;
        FundState storage f = funds[assetCode];
        while (payoutHead[assetCode] < payoutTail[assetCode]) {
            Payout storage p = payoutQueue[assetCode][payoutHead[assetCode]];
            uint128 need = p.owed - p.reservedAmt;
            uint128 take = need <= f.active ? need : f.active;
            if (take > 0) {
                f.active -= take;
                f.reserved += take;
                p.reservedAmt += take;
            }
            if (p.reservedAmt < p.owed) break; // still short: stay queued
            f.reserved -= p.owed;
            address user = p.user;
            uint64 rSeq = p.requestSeq;
            uint128 owed = p.owed;
            delete payoutQueue[assetCode][payoutHead[assetCode]];
            payoutHead[assetCode] += 1;
            totalQueuedPayouts -= 1;
            assets[assetCode].safeTransfer(user, owed);
            emit PayoutPaid(rSeq, user, assetCode, owed);
            _sendPayoutReceipt(rSeq, owed);
        }
    }

    function _sendPayoutReceipt(uint64 requestSeq_, uint128 amount) private {
        _send(
            Wire.encodePayoutReceipt(
                _outboundEnvelope(),
                Wire.PayoutReceipt({spokeId: SPOKE_ID, requestSeq: requestSeq_, amount: amount})
            )
        );
    }

    function _outboundEnvelope() private returns (Wire.Envelope memory) {
        return Wire.Envelope({
            srcChainId: LOCAL_CHAIN_ID,
            dstChainId: HUB_CHAIN_ID,
            srcApp: bytes32(uint256(uint160(address(this)))),
            dstApp: HUB_APP,
            seq: ++outboundSeq
        });
    }

    /// @dev Quote the transport fee and pay it from the fee pot (§2.4).
    function _send(bytes memory message) private {
        IMessageEndpoint ep = IMessageEndpoint(activeEndpoint);
        uint256 fee = ep.quoteFee(message);
        if (feePot < fee) revert FeePotInsufficient(fee, feePot);
        feePot -= fee;
        ep.send{value: fee}(message);
    }
}
