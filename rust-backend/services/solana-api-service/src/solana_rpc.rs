//! Direct Solana RPC read for live vault state.
//!
//! solana-api-service is otherwise a pure indexer-read layer, but a few
//! vault fields are *live* view values — the round phase, the selling
//! window, open-RFQ counters and config guardrails change within a round
//! as the keeper cranks — so events can't keep a fresh copy. One JSON-RPC
//! `getAccountInfo` (base64) on the vault pubkey returns them all.
//!
//! Decode path (no anchor / solana-sdk dep, per the workspace isolation
//! rule): strip the 8-byte Anchor account discriminator
//! (`sha256("account:Vault")[..8]`, checked), then borsh-decode a **mirror
//! struct** of `options_vault::state::Vault` — the solana-indexer
//! `events.rs` pattern. Borsh is positional, so the mirror transcribes
//! every prefix field of the real struct in order, stopping after the last
//! field the handlers need (`open_swap_rfqs`); trailing bytes are the
//! remaining fields and are deliberately not consumed.
//!
//! Best-effort like the Sui twin's `sui_getObject`: any failure degrades
//! to omitting the live fields, never a 5xx.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use borsh::{BorshDeserialize, BorshSerialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Live `Vault` view values that the indexer doesn't carry (it
/// materialises only what events state). Raw on-chain integers, atomic
/// units. Solana note: the Sui vault's `Balance` fields (deployable,
/// proceeds, withdrawal pool, …) live in PDA-seeded *token accounts* on
/// Solana, not on the Vault account, so they are not part of this read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultLive {
    /// Ground-truth round phase from the on-chain `Phase` enum:
    /// `active` (selling/holding) | `settling` (between rounds, redeeming).
    pub phase: String,
    /// Expiry of the current bucket; 0 when none is selected.
    pub current_expiry_ms: u64,
    /// `open_rfq` forbidden after this.
    pub selling_ends_ms: u64,
    /// Open option RFQ auctions this round.
    pub open_rfqs: u64,
    /// Open proceeds-swap auctions this round.
    pub open_swap_rfqs: u64,
    /// Config slice guardrail: max underlying per RFQ slice.
    pub max_slice_amount: u64,
    /// Config slice guardrail: max concurrent open RFQs.
    pub max_open_rfqs: u64,
}

// ── borsh mirrors of options_vault::state (prefix only) ────────────────────
//
// Field order transcribed from solana-contracts/programs/options_vault/
// src/state.rs — borsh is positional, so order is load-bearing.
// `BorshSerialize` is derived only so tests can hand-build account bytes.

/// Mirror of `options_vault::state::VaultConfig` — full struct (it sits in
/// the decode prefix, so every field must be consumed in order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshDeserialize, BorshSerialize)]
pub(crate) struct VaultConfigMirror {
    pub mgmt_fee_bps_annual: u64,
    pub perf_fee_bps: u64,
    pub round_ms: u64,
    pub selling_window_ms: u64,
    pub min_strike_bps_over_spot: u64,
    pub max_strike_bps_over_spot: u64,
    pub min_expiry_lead_ms: u64,
    pub max_expiry_lead_ms: u64,
    pub min_reserve_premium_bps: u64,
    pub max_slice_amount: u64,
    pub max_open_rfqs: u64,
    pub rfq_duration_ms: u64,
    pub rfq_snipe_window_ms: u64,
    pub rfq_snipe_extension_ms: u64,
    pub rfq_max_extension_ms: u64,
    pub rfq_min_increment_bps: u64,
    pub hold_premium_in_settlement: bool,
    pub max_swap_slippage_bps: u64,
    pub underlying_feed_id: [u8; 32],
    pub settlement_feed_id: [u8; 32],
    pub max_price_age_secs: u64,
    pub max_conf_bps: u64,
    pub underlying_decimals: u8,
    pub settlement_decimals: u8,
}

/// Mirror of `options_vault::state::Phase` (borsh unit enum, u8 tag —
/// variant order load-bearing: 0 = Settling, 1 = Active).
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshDeserialize, BorshSerialize)]
pub(crate) enum PhaseMirror {
    Settling,
    Active,
}

/// Prefix mirror of `options_vault::state::Vault`, through
/// `open_swap_rfqs` — the last field the handlers need. The real account
/// continues (positions_head … bump); those bytes are left unread.
#[derive(Debug, Clone, PartialEq, Eq, BorshDeserialize, BorshSerialize)]
pub(crate) struct VaultPrefixMirror {
    pub admin: [u8; 32],
    pub underlying_mint: [u8; 32],
    pub settlement_mint: [u8; 32],
    pub share_mint: [u8; 32],
    pub config: VaultConfigMirror,
    pub pending_config: Option<VaultConfigMirror>,
    pub round: u64,
    pub phase: PhaseMirror,
    pub current_bucket: Option<[u8; 32]>,
    pub current_expiry_ms: u64,
    pub selling_ends_ms: u64,
    pub open_rfqs: u64,
    pub open_swap_rfqs: u64,
}

/// Anchor account discriminator: `sha256("account:Vault")[..8]`.
pub(crate) fn vault_discriminator() -> [u8; 8] {
    let digest = Sha256::digest(b"account:Vault");
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

/// Decode a raw `Vault` account's data (discriminator included) into the
/// live view. Pure — unit-tested against hand-built fixture bytes.
pub(crate) fn decode_vault_account(data: &[u8]) -> Result<VaultLive> {
    if data.len() < 8 {
        bail!("vault account data too short ({} bytes)", data.len());
    }
    if data[..8] != vault_discriminator() {
        bail!(
            "account discriminator mismatch: not an options_vault Vault (got {:02x?})",
            &data[..8]
        );
    }
    // `deserialize` (not `try_from_slice`) — the mirror is a prefix, so
    // trailing bytes are expected and must not error.
    let mut rest = &data[8..];
    let v = VaultPrefixMirror::deserialize(&mut rest).context("borsh decode of Vault prefix")?;
    Ok(VaultLive {
        phase: match v.phase {
            PhaseMirror::Settling => "settling".to_string(),
            PhaseMirror::Active => "active".to_string(),
        },
        current_expiry_ms: v.current_expiry_ms,
        selling_ends_ms: v.selling_ends_ms,
        open_rfqs: v.open_rfqs,
        open_swap_rfqs: v.open_swap_rfqs,
        max_slice_amount: v.config.max_slice_amount,
        max_open_rfqs: v.config.max_open_rfqs,
    })
}

/// Read one vault's live fields via JSON-RPC `getAccountInfo` (base64).
/// `Ok(None)` if the node doesn't know the account (closed / wrong
/// cluster); `Err` on transport or unexpected-shape failures. Callers
/// degrade to omitting live fields.
pub async fn fetch_vault_live(
    http: &reqwest::Client,
    rpc_url: &str,
    vault_id: &str,
) -> Result<Option<VaultLive>> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getAccountInfo",
        "params": [vault_id, { "encoding": "base64", "commitment": "confirmed" }],
    });
    let resp = http
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("getAccountInfo request")?
        .error_for_status()
        .context("getAccountInfo http status")?;
    let parsed: Value = resp.json().await.context("decoding getAccountInfo")?;

    if let Some(err) = parsed.get("error") {
        bail!("getAccountInfo rpc error: {err}");
    }
    // `result.value` is null for an unknown account.
    let result = parsed
        .get("result")
        .ok_or_else(|| anyhow!("getAccountInfo missing result: {parsed}"))?;
    let Some(value) = result.get("value").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    // `data` is `[<base64>, "base64"]`.
    let b64 = value
        .get("data")
        .and_then(|d| d.get(0))
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow!("vault account has no base64 data"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("base64 decode of vault account data")?;
    decode_vault_account(&bytes).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixture bytes are hand-built by borsh-serializing the mirror structs
    // themselves. That proves discriminator handling, prefix decode and
    // Option/enum encodings — but NOT that the mirror matches the deployed
    // program byte-for-byte. A litesvm-derived golden fixture (dump a real
    // Vault account from a program test in solana-contracts and commit the
    // bytes) would be strictly stronger; generate one once the program
    // test harness exposes it.

    fn config(max_slice_amount: u64, max_open_rfqs: u64) -> VaultConfigMirror {
        VaultConfigMirror {
            mgmt_fee_bps_annual: 200,
            perf_fee_bps: 1_000,
            round_ms: 604_800_000,
            selling_window_ms: 86_400_000,
            min_strike_bps_over_spot: 200,
            max_strike_bps_over_spot: 2_000,
            min_expiry_lead_ms: 3_600_000,
            max_expiry_lead_ms: 1_209_600_000,
            min_reserve_premium_bps: 10,
            max_slice_amount,
            max_open_rfqs,
            rfq_duration_ms: 600_000,
            rfq_snipe_window_ms: 60_000,
            rfq_snipe_extension_ms: 60_000,
            rfq_max_extension_ms: 600_000,
            rfq_min_increment_bps: 25,
            hold_premium_in_settlement: false,
            max_swap_slippage_bps: 100,
            underlying_feed_id: [0xaa; 32],
            settlement_feed_id: [0xbb; 32],
            max_price_age_secs: 60,
            max_conf_bps: 100,
            underlying_decimals: 8,
            settlement_decimals: 6,
        }
    }

    fn vault_prefix(phase: PhaseMirror, pending: Option<VaultConfigMirror>) -> VaultPrefixMirror {
        VaultPrefixMirror {
            admin: [1; 32],
            underlying_mint: [2; 32],
            settlement_mint: [3; 32],
            share_mint: [4; 32],
            config: config(1_000_000_000_000, 4),
            pending_config: pending,
            round: 3,
            phase,
            current_bucket: Some([5; 32]),
            current_expiry_ms: 1_760_000_000_000,
            selling_ends_ms: 1_759_990_000_000,
            open_rfqs: 2,
            open_swap_rfqs: 1,
        }
    }

    /// Account bytes = discriminator ‖ borsh(prefix) ‖ trailing tail
    /// (emulating the real struct's remaining fields, which the prefix
    /// decode must leave unread).
    fn account_bytes(v: &VaultPrefixMirror, tail: &[u8]) -> Vec<u8> {
        let mut out = vault_discriminator().to_vec();
        out.extend(borsh::to_vec(v).unwrap());
        out.extend_from_slice(tail);
        out
    }

    #[test]
    fn decodes_active_vault_with_trailing_fields() {
        // Tail = the real struct's remaining fields (positions_head..bump):
        // 8×u64 + bool + u8 → arbitrary bytes here, must be ignored.
        let tail = vec![0x7f; 8 * 8 + 2];
        let data = account_bytes(&vault_prefix(PhaseMirror::Active, None), &tail);
        let live = decode_vault_account(&data).unwrap();
        assert_eq!(live.phase, "active");
        assert_eq!(live.current_expiry_ms, 1_760_000_000_000);
        assert_eq!(live.selling_ends_ms, 1_759_990_000_000);
        assert_eq!(live.open_rfqs, 2);
        assert_eq!(live.open_swap_rfqs, 1);
        assert_eq!(live.max_slice_amount, 1_000_000_000_000);
        assert_eq!(live.max_open_rfqs, 4);
    }

    #[test]
    fn decodes_settling_phase_and_pending_config() {
        // `pending_config: Some(_)` shifts every later field by the full
        // VaultConfig size — the decode must consume the Option correctly.
        let pending = config(7, 9);
        let data = account_bytes(&vault_prefix(PhaseMirror::Settling, Some(pending)), &[]);
        let live = decode_vault_account(&data).unwrap();
        assert_eq!(live.phase, "settling");
        // Guardrails come from the ACTIVE config, not the pending one.
        assert_eq!(live.max_slice_amount, 1_000_000_000_000);
        assert_eq!(live.max_open_rfqs, 4);
        assert_eq!(live.open_rfqs, 2);
    }

    #[test]
    fn rejects_wrong_discriminator() {
        let mut data = account_bytes(&vault_prefix(PhaseMirror::Active, None), &[]);
        data[0] ^= 0xff;
        let err = decode_vault_account(&data).unwrap_err().to_string();
        assert!(err.contains("discriminator mismatch"), "{err}");
    }

    #[test]
    fn rejects_truncated_data() {
        assert!(decode_vault_account(&[0u8; 4]).is_err());
        // Discriminator ok but body truncated mid-struct.
        let data = account_bytes(&vault_prefix(PhaseMirror::Active, None), &[]);
        assert!(decode_vault_account(&data[..data.len() / 2]).is_err());
    }

    #[test]
    fn discriminator_is_sha256_of_account_vault() {
        let digest = Sha256::digest(b"account:Vault");
        assert_eq!(vault_discriminator(), digest[..8]);
    }
}
