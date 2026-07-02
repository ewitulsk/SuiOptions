// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Message} from "./libraries/Message.sol";
import {TransferPayload} from "./libraries/TransferPayload.sol";
import {Outbox} from "./Outbox.sol";
import {IMessageRecipient} from "./interfaces/IMessageRecipient.sol";

interface IERC20 {
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

interface IWrappedToken {
    function mint(address to, uint256 amount) external;
    function burn(address from, uint256 amount) external;
}

/// @title Locker
/// @notice Layer 2 lock-and-mint app, one deployment per asset (bridge-spec.md
///         §3), mirroring the Move `locker::locker`.
///         - Home chain: `Escrow` — holds the native ERC-20 in escrow.
///         - Foreign chain: `Mint` — holds mint/burn authority over a WrappedToken.
///         Inbound delivery arrives via the Inbox `onReceive` callback (EVM has
///         dynamic dispatch, so no hot potato). Amounts cross the wire at
///         `WIRE_DECIMALS`; the Locker scales to/from local decimals with NTT
///         dust rejection. Over-limit inbound transfers queue and are claimable
///         after the window — they never revert (§3.5).
contract Locker is IMessageRecipient {
    enum Mode {
        Escrow, // home
        Mint // foreign
    }

    // --- immutable config ---
    Outbox public immutable outbox;
    address public immutable inbox;
    bytes32 public immutable assetId;
    address public immutable token; // ERC-20 (Escrow) or WrappedToken (Mint)
    Mode public immutable mode;
    uint8 public immutable localDecimals;

    // --- governance ---
    address public admin;

    // --- routing / controls ---
    mapping(uint32 => bytes32) public peers; // chainId => sibling Locker identity
    bool public paused;

    // --- inbound rate limit (wire units); cap == 0 disables ---
    uint64 public rateLimitWindow;
    uint64 public rateLimitCap;
    uint64 public windowStart;
    uint64 public windowUsed;

    // --- overflow queue ---
    struct Queued {
        address recipient;
        uint64 wireAmount;
        uint64 unlockAt;
        bool claimed;
    }

    mapping(uint256 => Queued) public queued;
    uint256 public nextQueueId;

    // --- events ---
    event BridgedOut(uint32 indexed dstChainId, uint64 nonce, uint64 wireAmount, bytes32 recipient);
    event BridgedIn(uint32 indexed srcChainId, uint64 wireAmount, address recipient);
    event TransferQueued(uint256 indexed id, address recipient, uint64 wireAmount, uint64 unlockAt);
    event TransferClaimed(uint256 indexed id, address recipient, uint64 wireAmount);
    event PeerSet(uint32 indexed chainId, bytes32 peer);
    event PausedSet(bool paused);
    event RateLimitSet(uint64 window, uint64 cap);
    event AdminTransferred(address indexed from, address indexed to);

    // --- errors ---
    error NotAdmin();
    error NotInbox();
    error Paused();
    error WrongMode();
    error ZeroAmount();
    error UnknownPeer(uint32 chainId);
    error PeerMismatch();
    error AssetMismatch();
    error AmountHasDust();
    error AmountOverflow();
    error TransferFailed();
    error BadQueueEntry();
    error StillLocked();

    modifier onlyAdmin() {
        if (msg.sender != admin) revert NotAdmin();
        _;
    }

    constructor(
        Outbox outbox_,
        address inbox_,
        bytes32 assetId_,
        address token_,
        Mode mode_,
        uint8 localDecimals_,
        address admin_
    ) {
        outbox = outbox_;
        inbox = inbox_;
        assetId = assetId_;
        token = token_;
        mode = mode_;
        localDecimals = localDecimals_;
        admin = admin_;
    }

    // --- admin ---

    function setPeer(uint32 chainId, bytes32 peer) external onlyAdmin {
        peers[chainId] = peer;
        emit PeerSet(chainId, peer);
    }

    /// @notice Hand admin authority to governance (e.g. a multisig) after wiring.
    function transferAdmin(address newAdmin) external onlyAdmin {
        emit AdminTransferred(admin, newAdmin);
        admin = newAdmin;
    }

    function setPaused(bool paused_) external onlyAdmin {
        paused = paused_;
        emit PausedSet(paused_);
    }

    function setRateLimit(uint64 window, uint64 cap) external onlyAdmin {
        rateLimitWindow = window;
        rateLimitCap = cap;
        windowUsed = 0;
        emit RateLimitSet(window, cap);
    }

    // --- outbound ---

    /// @notice Home-chain escrow-and-send.
    function lock(uint256 amount, uint32 dstChainId, bytes32 recipient) external {
        if (mode != Mode.Escrow) revert WrongMode();
        _bridgeOut(amount, dstChainId, recipient);
    }

    /// @notice Foreign-chain burn-and-send.
    function burn(uint256 amount, uint32 dstChainId, bytes32 recipient) external {
        if (mode != Mode.Mint) revert WrongMode();
        _bridgeOut(amount, dstChainId, recipient);
    }

    function _bridgeOut(uint256 amount, uint32 dstChainId, bytes32 recipient) internal {
        if (paused) revert Paused();
        if (amount == 0) revert ZeroAmount();
        bytes32 peer = peers[dstChainId];
        if (peer == bytes32(0)) revert UnknownPeer(dstChainId);

        uint64 wireAmount = _toWire(amount);

        if (mode == Mode.Escrow) {
            _safeTransferFrom(msg.sender, address(this), amount);
        } else {
            IWrappedToken(token).burn(msg.sender, amount);
        }

        bytes memory payload =
            TransferPayload.encode(TransferPayload.Data(assetId, wireAmount, recipient));
        (uint64 nonce,) = outbox.send(dstChainId, peer, payload);
        emit BridgedOut(dstChainId, nonce, wireAmount, recipient);
    }

    // --- inbound (Inbox callback) ---

    function onReceive(uint32 srcChainId, bytes32 srcApp, bytes calldata payload) external {
        if (msg.sender != inbox) revert NotInbox();
        if (paused) revert Paused();
        bytes32 peer = peers[srcChainId];
        if (peer == bytes32(0) || srcApp != peer) revert PeerMismatch();

        TransferPayload.Data memory tp = TransferPayload.decode(payload);
        if (tp.assetId != assetId) revert AssetMismatch();
        if (tp.amount == 0) revert ZeroAmount();

        address recipient = address(uint160(uint256(tp.recipient)));

        if (_consumeRateLimit(tp.amount)) {
            _deliver(recipient, tp.amount);
            emit BridgedIn(srcChainId, tp.amount, recipient);
        } else {
            // Over the window cap: queue instead of reverting (§3.5). The message
            // is still consumed at the Inbox; only the payout is delayed.
            uint64 unlockAt = windowStart + rateLimitWindow;
            uint256 id = nextQueueId++;
            queued[id] = Queued(recipient, tp.amount, unlockAt, false);
            emit TransferQueued(id, recipient, tp.amount, unlockAt);
        }
    }

    /// @notice Permissionless release of a queued transfer once its window passed.
    ///         The delay itself was the rate-limit control, so a claim does not
    ///         consume budget.
    function claim(uint256 id) external {
        if (paused) revert Paused();
        Queued storage q = queued[id];
        if (q.recipient == address(0) || q.claimed) revert BadQueueEntry();
        if (block.timestamp < q.unlockAt) revert StillLocked();
        q.claimed = true;
        _deliver(q.recipient, q.wireAmount);
        emit TransferClaimed(id, q.recipient, q.wireAmount);
    }

    function _deliver(address recipient, uint64 wireAmount) internal {
        uint256 local = _fromWire(wireAmount);
        if (mode == Mode.Escrow) {
            _safeTransfer(recipient, local);
        } else {
            IWrappedToken(token).mint(recipient, local);
        }
    }

    // --- rate limit ---

    /// @dev Returns true and reserves budget if within the window cap; false if
    ///      over (caller queues). cap == 0 disables the limit (always true).
    function _consumeRateLimit(uint64 wireAmount) internal returns (bool) {
        if (rateLimitCap == 0) return true;
        if (block.timestamp >= windowStart + rateLimitWindow) {
            windowStart = uint64(block.timestamp);
            windowUsed = 0;
        }
        if (windowUsed + wireAmount <= rateLimitCap) {
            windowUsed += wireAmount;
            return true;
        }
        return false;
    }

    // --- decimals scaling (NTT trimmed-amount; mirrors Move to_wire/from_wire) ---

    function _toWire(uint256 localAmount) internal view returns (uint64) {
        uint8 wire = TransferPayload.WIRE_DECIMALS;
        uint256 w;
        if (localDecimals >= wire) {
            uint256 factor = 10 ** (localDecimals - wire);
            if (localAmount % factor != 0) revert AmountHasDust();
            w = localAmount / factor;
        } else {
            w = localAmount * (10 ** (wire - localDecimals));
        }
        if (w > type(uint64).max) revert AmountOverflow();
        return uint64(w);
    }

    function _fromWire(uint64 wireAmount) internal view returns (uint256) {
        uint8 wire = TransferPayload.WIRE_DECIMALS;
        if (localDecimals >= wire) {
            return uint256(wireAmount) * (10 ** (localDecimals - wire));
        } else {
            uint256 factor = 10 ** (wire - localDecimals);
            if (wireAmount % factor != 0) revert AmountHasDust();
            return uint256(wireAmount) / factor;
        }
    }

    // --- ERC-20 safe transfer (tolerates non-bool-returning tokens) ---

    function _safeTransfer(address to, uint256 amount) internal {
        (bool ok, bytes memory data) =
            token.call(abi.encodeWithSelector(IERC20.transfer.selector, to, amount));
        if (!ok || (data.length != 0 && !abi.decode(data, (bool)))) revert TransferFailed();
    }

    function _safeTransferFrom(address from, address to, uint256 amount) internal {
        (bool ok, bytes memory data) =
            token.call(abi.encodeWithSelector(IERC20.transferFrom.selector, from, to, amount));
        if (!ok || (data.length != 0 && !abi.decode(data, (bool)))) revert TransferFailed();
    }

    // --- views ---

    /// @notice Escrowed balance held by this Locker (home chain).
    function escrowed() external view returns (uint256) {
        return IERC20(token).balanceOf(address(this));
    }
}
