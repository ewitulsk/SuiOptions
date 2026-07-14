//! Anchor error-code extraction + classification — the Solana analog of
//! the Sui stack's Move-abort taxonomy (guide 09): every tx-submission
//! failure is classed **Benign** (lost race / state already advanced —
//! suppress, replan), **Retry** (transient: oracle staleness, RPC,
//! blockhash — alert with `class = "retry"`), or **Fatal** (config /
//! feed-mismatch families — alert + halt the affected flow).
//!
//! Codes come from the program crates' error enums (Anchor custom errors
//! start at 6000), never magic numbers. Unknown codes and non-Anchor
//! failures default to Retry, per the keeper guide.

use auction_venue::error::VenueError;
use options_core::error::CoreError;
use options_vault::error::VaultError;

/// Anchor's custom-error offset: `error_code` variant 0 ⇒ on-chain 6000.
pub const ANCHOR_ERROR_OFFSET: u32 = 6000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// Lost race / state already advanced — log at debug, replan.
    Benign,
    /// Transient — alert (`class = "retry"`) and try again next tick.
    Retry,
    /// Config or invariant breakage — alert (`class = "fatal"`) and halt
    /// the affected flow.
    Fatal,
}

/// Which program's error table to classify against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    OptionsCore,
    AuctionVenue,
    OptionsVault,
}

/// On-chain code for a core error variant — for service benign-sets,
/// instead of magic numbers. (Plain discriminant cast; anchor's own
/// `From<…> for u32` isn't used so the offset is applied exactly once.)
pub fn core_code(e: CoreError) -> u32 {
    ANCHOR_ERROR_OFFSET + e as u32
}

pub fn venue_code(e: VenueError) -> u32 {
    ANCHOR_ERROR_OFFSET + e as u32
}

pub fn vault_code(e: VaultError) -> u32 {
    ANCHOR_ERROR_OFFSET + e as u32
}

/// Extract an Anchor custom error code from failed-transaction logs or a
/// stringified RPC/simulation error. Handles both surfaces:
///
/// - the runtime line `… failed: custom program error: 0x1770`
/// - Anchor's own log `Program log: AnchorError … Error Code: X. Error
///   Number: 6001. …`
pub fn extract_error_code(text: &str) -> Option<u32> {
    if let Some(idx) = text.find("custom program error: 0x") {
        let hex = &text[idx + "custom program error: 0x".len()..];
        let hex: String = hex.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        if let Ok(code) = u32::from_str_radix(&hex, 16) {
            return Some(code);
        }
    }
    if let Some(idx) = text.find("Error Number: ") {
        let dec = &text[idx + "Error Number: ".len()..];
        let dec: String = dec.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(code) = dec.parse() {
            return Some(code);
        }
    }
    None
}

/// Convenience over a log vector (simulation results, tx meta).
pub fn extract_error_code_from_logs<S: AsRef<str>>(logs: &[S]) -> Option<u32> {
    logs.iter().find_map(|l| extract_error_code(l.as_ref()))
}

/// Default classification table for `code` under `program`. Anchor
/// framework codes (< 6000) and unknown custom codes are Retry.
pub fn classify(program: Program, code: u32) -> Classification {
    use Classification::*;
    let hit = |set: &[u32]| set.contains(&code);
    match program {
        Program::OptionsCore => {
            use CoreError as E;
            // Lost race / state advanced: someone consumed the quote, time
            // passed the quote or the bucket, the nonce isn't prunable yet.
            let benign = [
                core_code(E::QuoteExpired),
                core_code(E::QuoteNonceUsed),
                core_code(E::BucketExpired),
                core_code(E::BucketNotExpired),
                core_code(E::BucketInvalidated),
                core_code(E::BucketNotInvalidated),
                core_code(E::BucketNotDrained),
                core_code(E::NonceStillValid),
            ];
            // Balance shortfalls resolve on their own (MM tops up,
            // treasury accrues) — retry, don't halt.
            let retry = [
                core_code(E::InsufficientAccountBalance),
                core_code(E::InsufficientTreasuryBalance),
            ];
            // Everything else in the core enum is a mis-built transaction
            // or bad configuration (signature/protocol/bucket mismatches,
            // scheme/length validation, overflow) — operator attention.
            let fatal_last = core_code(E::RecipientMismatch);
            if hit(&benign) {
                Benign
            } else if hit(&retry) {
                Retry
            } else if (ANCHOR_ERROR_OFFSET..=fatal_last).contains(&code) {
                Fatal
            } else {
                Retry
            }
        }
        Program::AuctionVenue => {
            use VenueError as E;
            // Bid/settle races: outbid, deadline crossed mid-flight, crank
            // fired early, expired-recovery path not open yet.
            let benign = [
                venue_code(E::AuctionClosed),
                venue_code(E::AuctionNotClosed),
                venue_code(E::BidTooLow),
                venue_code(E::BucketStillLive),
                venue_code(E::BucketExpiredOrInvalid),
            ];
            // The rest (mode/authority/recipient mismatches, duration and
            // collateral validation) means a mis-built auction or settle.
            let fatal_last = venue_code(E::ForceRefundUnauthorized);
            if hit(&benign) {
                Benign
            } else if (ANCHOR_ERROR_OFFSET..=fatal_last).contains(&code) {
                Fatal
            } else {
                Retry
            }
        }
        Program::OptionsVault => {
            use VaultError as E;
            // The permissionless-crank benign set: another cranker advanced
            // the state machine (or the planner's view was a tick stale).
            let benign = [
                vault_code(E::WrongPhase),
                vault_code(E::BucketNotSelected),
                vault_code(E::BucketAlreadySelected),
                vault_code(E::SellingClosed),
                vault_code(E::PositionsPending),
                vault_code(E::RfqsOpen),
                vault_code(E::RoundNotFinalized),
                vault_code(E::TooManyRfqs),
                vault_code(E::WrongIndex),
                vault_code(E::ProceedsUnswapped),
                vault_code(E::BucketInvalidated),
                vault_code(E::DepositsPaused),
            ];
            // Oracle transients (repost Pyth, re-crank) and planner inputs
            // that go stale between plan and land (spot moved).
            let retry = [
                vault_code(E::OraclePriceStale),
                vault_code(E::OracleConfidence),
                vault_code(E::StrikeOutOfBand),
                vault_code(E::ExpiryOutOfBand),
                vault_code(E::SliceTooLarge),
            ];
            // Feed/config/identity families (OracleFeedMismatch,
            // OraclePriceInvalid, ConfigInvalid, WrongOrigin, NotAdmin,
            // ReceiptMismatch, AccountMismatch, overflow, zero amounts).
            let fatal_last = vault_code(E::AccountMismatch);
            if hit(&benign) {
                Benign
            } else if hit(&retry) {
                Retry
            } else if (ANCHOR_ERROR_OFFSET..=fatal_last).contains(&code) {
                Fatal
            } else {
                Retry
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_runtime_custom_error_line() {
        let line = "Error processing Instruction 1: custom program error: 0x1770";
        assert_eq!(extract_error_code(line), Some(0x1770));
        // 0x1770 = 6000 = variant 0.
        assert_eq!(0x1770, ANCHOR_ERROR_OFFSET);
    }

    #[test]
    fn parses_anchor_log_line() {
        let line = "Program log: AnchorError occurred. Error Code: BidTooLow. \
                    Error Number: 6002. Error Message: Bid below reserve or minimum increment.";
        assert_eq!(extract_error_code(line), Some(6002));
    }

    #[test]
    fn parses_from_log_vector_and_handles_absence() {
        let logs = vec![
            "Program 8cvpWnJaQ4kTEPypwrZvBPzEM4R7FbivgybXBm2ahvKk invoke [1]",
            "Program log: AnchorError occurred. Error Code: AuctionClosed. Error Number: 6000.",
            "Program 8cvpWnJaQ4kTEPypwrZvBPzEM4R7FbivgybXBm2ahvKk failed",
        ];
        assert_eq!(extract_error_code_from_logs(&logs), Some(6000));
        assert_eq!(extract_error_code("blockhash not found"), None);
    }

    #[test]
    fn code_helpers_use_enum_discriminants() {
        assert_eq!(venue_code(VenueError::BidTooLow), 6002);
        assert_eq!(core_code(CoreError::QuoteExpired), 6000);
        assert_eq!(vault_code(VaultError::WrongPhase), 6000);
    }

    #[test]
    fn classifies_venue_races_as_benign() {
        for e in [
            VenueError::BidTooLow,
            VenueError::AuctionClosed,
            VenueError::AuctionNotClosed,
        ] {
            assert_eq!(
                classify(Program::AuctionVenue, venue_code(e)),
                Classification::Benign,
            );
        }
        assert_eq!(
            classify(Program::AuctionVenue, venue_code(VenueError::WrongSettleAuthority)),
            Classification::Fatal
        );
    }

    #[test]
    fn classifies_vault_oracle_taxonomy() {
        assert_eq!(
            classify(Program::OptionsVault, vault_code(VaultError::OraclePriceStale)),
            Classification::Retry
        );
        assert_eq!(
            classify(Program::OptionsVault, vault_code(VaultError::OracleConfidence)),
            Classification::Retry
        );
        assert_eq!(
            classify(Program::OptionsVault, vault_code(VaultError::OracleFeedMismatch)),
            Classification::Fatal
        );
        assert_eq!(
            classify(Program::OptionsVault, vault_code(VaultError::WrongPhase)),
            Classification::Benign
        );
    }

    #[test]
    fn classifies_core_quote_races_and_bugs() {
        assert_eq!(
            classify(Program::OptionsCore, core_code(CoreError::QuoteNonceUsed)),
            Classification::Benign
        );
        assert_eq!(
            classify(Program::OptionsCore, core_code(CoreError::QuoteSignatureInvalid)),
            Classification::Fatal
        );
        assert_eq!(
            classify(Program::OptionsCore, core_code(CoreError::InsufficientAccountBalance)),
            Classification::Retry
        );
    }

    #[test]
    fn unknown_and_framework_codes_default_to_retry() {
        // Anchor framework range (< 6000).
        assert_eq!(classify(Program::OptionsCore, 3012), Classification::Retry);
        // Beyond the last variant of every program enum.
        assert_eq!(classify(Program::OptionsCore, 6999), Classification::Retry);
        assert_eq!(classify(Program::AuctionVenue, 6999), Classification::Retry);
        assert_eq!(classify(Program::OptionsVault, 6999), Classification::Retry);
    }
}
