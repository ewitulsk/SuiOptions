// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @title Wire — canonical multichain vault message codec
/// @notice Solidity implementation of the wire format defined in
///         `rust-backend/crates/vault-messages/src/lib.rs` (the SOURCE OF
///         TRUTH; see docs/multichain-vault-plan.md §2.1). Pinned to the
///         golden fixtures under that crate's `fixtures/` directory by
///         `test/WireGolden.t.sol`.
///
/// Layout rules (mirrors the Rust crate header):
/// - All integers are FIXED-WIDTH BIG-ENDIAN (no varints).
/// - Addresses/ids travel as 32 bytes; EVM addresses left-padded with zeros.
/// - Amounts travel as u128; EVM producers must revert above u128 max.
/// - Booleans are one byte, 0 or 1; any other value is a decode error.
///
/// Envelope (90 bytes): version(1) ‖ msg_type(1) ‖ src_chain_id(8) ‖
/// dst_chain_id(8) ‖ src_app(32) ‖ dst_app(32) ‖ seq(8), then the payload.
library Wire {
    uint8 internal constant WIRE_VERSION = 1;
    uint256 internal constant ENVELOPE_LEN = 90;
    /// @dev Hard cap on `StateSync.integration_raw` (mirrored in all codecs).
    uint256 internal constant MAX_INTEGRATION_RAW_LEN = 4096;
    uint256 internal constant MAX_STATE_SYNC_ASSETS = 16;

    // MsgType discriminants (match the Rust `MsgType` enum).
    uint8 internal constant MSG_DEPOSIT_NOTICE = 1;
    uint8 internal constant MSG_WITHDRAW_REQUEST = 2;
    uint8 internal constant MSG_PAYOUT_RECEIPT = 3;
    uint8 internal constant MSG_STATE_SYNC = 4;
    uint8 internal constant MSG_DEPOSIT_ACK = 5;
    uint8 internal constant MSG_WITHDRAW_ACK = 6;
    uint8 internal constant MSG_CONFIG_SYNC = 7;

    // Exact payload byte lengths for the fixed-size payloads.
    uint256 internal constant DEPOSIT_NOTICE_LEN = 74; // 8+8+32+1+16+1+8
    uint256 internal constant WITHDRAW_REQUEST_LEN = 66; // 8+8+32+1+16+1
    uint256 internal constant PAYOUT_RECEIPT_LEN = 32; // 8+8+16
    uint256 internal constant DEPOSIT_ACK_LEN = 25; // 8+1+16
    uint256 internal constant WITHDRAW_ACK_LEN = 56; // 8+32+16
    uint256 internal constant CONFIG_SYNC_LEN = 67; // 1+1+32+1+32

    error UnsupportedVersion(uint8 version);
    error UnknownMsgType(uint8 msgType);
    error Truncated(uint256 wanted, uint256 had);
    error TrailingBytes(uint256 extra);
    error InvalidBool(uint8 value);
    error IntegrationRawTooLong(uint256 length);
    error TooManyAssets(uint256 count);

    struct Envelope {
        uint64 srcChainId;
        uint64 dstChainId;
        bytes32 srcApp;
        bytes32 dstApp;
        uint64 seq;
    }

    /// @notice Spoke → hub: raw deposit fact (no valuation fields by design).
    ///         `tsMs` is the spoke block timestamp of the deposit.
    struct DepositNotice {
        uint64 spokeId;
        uint64 depositSeq;
        bytes32 depositor;
        uint8 asset;
        uint128 amount;
        uint8 tranche;
        uint64 tsMs;
    }

    /// @notice Spoke → hub: share-denominated withdrawal ask.
    struct WithdrawRequest {
        uint64 spokeId;
        uint64 requestSeq;
        bytes32 user;
        uint8 tranche;
        uint128 shares;
        bool all;
    }

    /// @notice Spoke → hub: a queued payout physically settled.
    struct PayoutReceipt {
        uint64 spokeId;
        uint64 requestSeq;
        uint128 amount;
    }

    struct StateSyncAsset {
        uint8 asset;
        uint128 free;
        uint128 reserved;
    }

    /// @notice Spoke → hub: raw balances snapshot (never a valuation).
    struct StateSync {
        uint64 spokeId;
        StateSyncAsset[] assets;
        uint128 feePotBalance;
        bytes integrationRaw;
        uint64 tsMs;
    }

    /// @notice Hub → spoke: mint record instruction (shares hub-computed).
    struct DepositAck {
        uint64 depositSeq;
        bool accepted;
        uint128 shares;
    }

    /// @notice Hub → spoke: pay instruction.
    struct WithdrawAck {
        uint64 requestSeq;
        bytes32 user;
        uint128 payAmount;
    }

    /// @notice Hub → spoke: gate/identity propagation.
    struct ConfigSync {
        bool paused;
        bool riskOff;
        bytes32 curator;
        uint8 endpoint;
        bytes32 integrationsRoot;
    }

    // ───────────────────────────── encode ─────────────────────────────

    /// @notice Encode the 90-byte envelope header for a given message type.
    function encodeEnvelope(uint8 msgType, Envelope memory e) internal pure returns (bytes memory) {
        return abi.encodePacked(
            WIRE_VERSION, msgType, e.srcChainId, e.dstChainId, e.srcApp, e.dstApp, e.seq
        );
    }

    function encodeDepositNotice(Envelope memory e, DepositNotice memory p)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(
            encodeEnvelope(MSG_DEPOSIT_NOTICE, e),
            p.spokeId,
            p.depositSeq,
            p.depositor,
            p.asset,
            p.amount,
            p.tranche,
            p.tsMs
        );
    }

    function encodeWithdrawRequest(Envelope memory e, WithdrawRequest memory p)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(
            encodeEnvelope(MSG_WITHDRAW_REQUEST, e),
            p.spokeId,
            p.requestSeq,
            p.user,
            p.tranche,
            p.shares,
            p.all
        );
    }

    function encodePayoutReceipt(Envelope memory e, PayoutReceipt memory p)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(
            encodeEnvelope(MSG_PAYOUT_RECEIPT, e), p.spokeId, p.requestSeq, p.amount
        );
    }

    function encodeStateSync(Envelope memory e, StateSync memory p)
        internal
        pure
        returns (bytes memory)
    {
        if (p.assets.length > MAX_STATE_SYNC_ASSETS) revert TooManyAssets(p.assets.length);
        if (p.integrationRaw.length > MAX_INTEGRATION_RAW_LEN) {
            revert IntegrationRawTooLong(p.integrationRaw.length);
        }
        bytes memory entries;
        for (uint256 i = 0; i < p.assets.length; i++) {
            entries =
                abi.encodePacked(entries, p.assets[i].asset, p.assets[i].free, p.assets[i].reserved);
        }
        return abi.encodePacked(
            encodeEnvelope(MSG_STATE_SYNC, e),
            p.spokeId,
            uint8(p.assets.length),
            entries,
            p.feePotBalance,
            uint16(p.integrationRaw.length),
            p.integrationRaw,
            p.tsMs
        );
    }

    function encodeDepositAck(Envelope memory e, DepositAck memory p)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(
            encodeEnvelope(MSG_DEPOSIT_ACK, e), p.depositSeq, p.accepted, p.shares
        );
    }

    function encodeWithdrawAck(Envelope memory e, WithdrawAck memory p)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(
            encodeEnvelope(MSG_WITHDRAW_ACK, e), p.requestSeq, p.user, p.payAmount
        );
    }

    function encodeConfigSync(Envelope memory e, ConfigSync memory p)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(
            encodeEnvelope(MSG_CONFIG_SYNC, e),
            p.paused,
            p.riskOff,
            p.curator,
            p.endpoint,
            p.integrationsRoot
        );
    }

    // ───────────────────────────── decode ─────────────────────────────

    /// @notice Decode a standalone 90-byte envelope header.
    /// @dev Rejects wrong version, unknown msg types, and any length other
    ///      than exactly `ENVELOPE_LEN`.
    function decodeEnvelope(bytes memory b)
        internal
        pure
        returns (uint8 msgType, Envelope memory e)
    {
        if (b.length < ENVELOPE_LEN) revert Truncated(ENVELOPE_LEN, b.length);
        if (b.length > ENVELOPE_LEN) revert TrailingBytes(b.length - ENVELOPE_LEN);
        uint8 version = _u8(b, 0);
        if (version != WIRE_VERSION) revert UnsupportedVersion(version);
        msgType = _u8(b, 1);
        if (msgType == 0 || msgType > MSG_CONFIG_SYNC) revert UnknownMsgType(msgType);
        e.srcChainId = _u64(b, 2);
        e.dstChainId = _u64(b, 10);
        e.srcApp = _b32(b, 18);
        e.dstApp = _b32(b, 50);
        e.seq = _u64(b, 82);
    }

    function decodeDepositNotice(bytes memory b) internal pure returns (DepositNotice memory p) {
        _exactLen(b, DEPOSIT_NOTICE_LEN);
        p.spokeId = _u64(b, 0);
        p.depositSeq = _u64(b, 8);
        p.depositor = _b32(b, 16);
        p.asset = _u8(b, 48);
        p.amount = _u128(b, 49);
        p.tranche = _u8(b, 65);
        p.tsMs = _u64(b, 66);
    }

    function decodeWithdrawRequest(bytes memory b)
        internal
        pure
        returns (WithdrawRequest memory p)
    {
        _exactLen(b, WITHDRAW_REQUEST_LEN);
        p.spokeId = _u64(b, 0);
        p.requestSeq = _u64(b, 8);
        p.user = _b32(b, 16);
        p.tranche = _u8(b, 48);
        p.shares = _u128(b, 49);
        p.all = _bool(b, 65);
    }

    function decodePayoutReceipt(bytes memory b) internal pure returns (PayoutReceipt memory p) {
        _exactLen(b, PAYOUT_RECEIPT_LEN);
        p.spokeId = _u64(b, 0);
        p.requestSeq = _u64(b, 8);
        p.amount = _u128(b, 16);
    }

    function decodeStateSync(bytes memory b) internal pure returns (StateSync memory p) {
        // spoke_id(8) ‖ count(1) ‖ count*(1+16+16) ‖ fee_pot(16) ‖
        // raw_len(2) ‖ raw ‖ ts_ms(8)
        _need(b, 9);
        p.spokeId = _u64(b, 0);
        uint256 n = _u8(b, 8);
        if (n > MAX_STATE_SYNC_ASSETS) revert TooManyAssets(n);
        uint256 o = 9;
        _need(b, o + n * 33 + 18);
        p.assets = new StateSyncAsset[](n);
        for (uint256 i = 0; i < n; i++) {
            p.assets[i] =
                StateSyncAsset({asset: _u8(b, o), free: _u128(b, o + 1), reserved: _u128(b, o + 17)});
            o += 33;
        }
        p.feePotBalance = _u128(b, o);
        o += 16;
        uint256 rawLen = _u16(b, o);
        o += 2;
        if (rawLen > MAX_INTEGRATION_RAW_LEN) revert IntegrationRawTooLong(rawLen);
        _need(b, o + rawLen + 8);
        p.integrationRaw = _slice(b, o, rawLen);
        o += rawLen;
        p.tsMs = _u64(b, o);
        o += 8;
        if (b.length != o) revert TrailingBytes(b.length - o);
    }

    function decodeDepositAck(bytes memory b) internal pure returns (DepositAck memory p) {
        _exactLen(b, DEPOSIT_ACK_LEN);
        p.depositSeq = _u64(b, 0);
        p.accepted = _bool(b, 8);
        p.shares = _u128(b, 9);
    }

    function decodeWithdrawAck(bytes memory b) internal pure returns (WithdrawAck memory p) {
        _exactLen(b, WITHDRAW_ACK_LEN);
        p.requestSeq = _u64(b, 0);
        p.user = _b32(b, 8);
        p.payAmount = _u128(b, 40);
    }

    function decodeConfigSync(bytes memory b) internal pure returns (ConfigSync memory p) {
        _exactLen(b, CONFIG_SYNC_LEN);
        p.paused = _bool(b, 0);
        p.riskOff = _bool(b, 1);
        p.curator = _b32(b, 2);
        p.endpoint = _u8(b, 34);
        p.integrationsRoot = _b32(b, 35);
    }

    // ─────────────────────────── raw readers ───────────────────────────

    function _exactLen(bytes memory b, uint256 wanted) private pure {
        if (b.length < wanted) revert Truncated(wanted, b.length);
        if (b.length > wanted) revert TrailingBytes(b.length - wanted);
    }

    function _need(bytes memory b, uint256 wanted) private pure {
        if (b.length < wanted) revert Truncated(wanted, b.length);
    }

    /// @dev Loads the 32-byte word starting at `offset`. Callers guarantee
    ///      via length checks that the bytes actually consumed (the top
    ///      `width` bytes of the word) are in bounds; lower garbage bits are
    ///      shifted away by the typed readers.
    function _word(bytes memory b, uint256 offset) private pure returns (bytes32 w) {
        assembly {
            w := mload(add(add(b, 32), offset))
        }
    }

    function _u8(bytes memory b, uint256 o) private pure returns (uint8) {
        return uint8(uint256(_word(b, o)) >> 248);
    }

    function _u16(bytes memory b, uint256 o) private pure returns (uint16) {
        return uint16(uint256(_word(b, o)) >> 240);
    }

    function _u64(bytes memory b, uint256 o) private pure returns (uint64) {
        return uint64(uint256(_word(b, o)) >> 192);
    }

    function _u128(bytes memory b, uint256 o) private pure returns (uint128) {
        return uint128(uint256(_word(b, o)) >> 128);
    }

    function _b32(bytes memory b, uint256 o) private pure returns (bytes32) {
        return _word(b, o);
    }

    function _bool(bytes memory b, uint256 o) private pure returns (bool) {
        uint8 v = _u8(b, o);
        if (v > 1) revert InvalidBool(v);
        return v == 1;
    }

    function _slice(bytes memory b, uint256 o, uint256 len) private pure returns (bytes memory out) {
        out = new bytes(len);
        for (uint256 i = 0; i < len; i++) {
            out[i] = b[o + i];
        }
    }
}
