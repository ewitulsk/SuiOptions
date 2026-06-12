// Session policy: what a freshly minted SessionCap may do and spend.
//
// The selector allowlist names every `_with_session` entrypoint (the full
// selector constants declared in the Move modules), and the per-type spend
// limits — covered by the root signature the user reviews in their wallet —
// bound how much of each asset can leave custody per tx / per session.

import type { SpendLimit } from "@yourorg/sui-siws-session";

import { SUPPORTED_TOKENS } from "../config";

/** Cap lifetime. Conservative: one trading session. */
export const SESSION_TTL_MS = 12 * 60 * 60 * 1000; // 12h

/**
 * Selectors the cap may call — must match the `SEL_*` constants in
 * `contracts/sources/{account,bucket}.move` byte-for-byte.
 */
export const SESSION_ALLOWED: string[] = [
  "options_protocol::account::create_and_share_account_with_session",
  "options_protocol::account::withdraw_with_session",
  "options_protocol::account::set_quote_signing_key_with_session",
  "options_protocol::bucket::execute_write_with_session",
  "options_protocol::bucket::exercise_with_session",
  "options_protocol::bucket::redeem_position_with_session",
  "options_protocol::bucket::burn_expired_option_with_session",
  // Covered-call vault twins — allowed now so caps minted today can use the
  // vault once its UI ships (the allowlist is fixed at sign-in).
  "options_protocol::vault::deposit_with_session",
  "options_protocol::vault::claim_shares_with_session",
  "options_protocol::vault::initiate_withdraw_with_session",
  "options_protocol::vault::complete_withdraw_with_session",
  "options_protocol::vault::instant_withdraw_pending_with_session",
];

/**
 * Per-whole-token spend limits. Value can only leave custody in the catalog
 * assets (settlement payments, premium, collateral, withdrawals) — call
 * coins move only inside exercise/burn flows whose proceeds return to
 * custody, so they need no limit entry.
 */
const PER_TX_WHOLE_TOKENS = 1_000n;
const TOTAL_WHOLE_TOKENS = 10_000n;

/** One limit entry per supported token, scaled by its decimals. */
export function defaultLimits(): SpendLimit[] {
  return SUPPORTED_TOKENS.filter((t) => t.enabled).map((t) => {
    const unit = 10n ** BigInt(t.decimals);
    return {
      coinType: t.coinType,
      perTx: PER_TX_WHOLE_TOKENS * unit,
      total: TOTAL_WHOLE_TOKENS * unit,
    };
  });
}
