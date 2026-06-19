// Session policy: what a freshly minted SessionCap may do.
//
// The selector allowlist names every cap-gated `_with_session` entrypoint (the
// full selector constants declared in the Move modules). Caps carry no spend
// limits — in-protocol flows are budget-free and external withdrawal is gated
// by a separate host-wallet signature (see `defaultLimits`).

import type { SpendLimit } from "@yourorg/sui-siws-session";

/** Cap lifetime. Conservative: one trading session. */
export const SESSION_TTL_MS = 12 * 60 * 60 * 1000; // 12h

/**
 * Selectors the cap may call — must match the `SEL_*` constants in
 * `contracts/sources/session_{account,bucket,vault}.move` byte-for-byte.
 */
export const SESSION_ALLOWED: string[] = [
  "options_protocol::session_account::create_and_share_account_with_session",
  // NOTE: external withdrawal is intentionally NOT here. `withdraw_with_root_sig`
  // is gated by a fresh host-wallet signature (verified on-chain), not by the
  // session cap, so it needs no cap-allowlist entry.
  "options_protocol::session_account::set_quote_signing_key_with_session",
  "options_protocol::session_bucket::execute_write_with_session",
  "options_protocol::session_bucket::exercise_with_session",
  "options_protocol::session_bucket::redeem_position_with_session",
  "options_protocol::session_bucket::burn_expired_option_with_session",
  // Covered-call vault twins — allowed now so caps minted today can use the
  // vault once its UI ships (the allowlist is fixed at sign-in).
  "options_protocol::session_vault::deposit_with_session",
  "options_protocol::session_vault::claim_shares_with_session",
  "options_protocol::session_vault::initiate_withdraw_with_session",
  "options_protocol::session_vault::complete_withdraw_with_session",
  "options_protocol::session_vault::instant_withdraw_pending_with_session",
];

/**
 * No per-type spend limits. In-protocol flows (write/exercise/vault) move
 * value only within the user's own custody (collateral, premium, receipts,
 * positions — all redeemable back to the same account) and never to an
 * arbitrary recipient, so they're `authorize`-gated with no budget. The one
 * value-exit, `withdraw_with_root_sig`, requires a fresh host-wallet signature
 * instead of a cap budget. A session key thus has full trading authority but
 * zero withdrawal authority.
 */
export function defaultLimits(): SpendLimit[] {
  return [];
}
