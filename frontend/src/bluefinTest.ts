// ⚠️ STAGING-ONLY TESTING AFFORDANCE (SO-311) — never wire this into a
// user-facing flow, and never let it reach mainnet or prod. ⚠️
//
// Bluefin's staging exchange runs on Sui testnet and margins in THEIR OWN
// testnet USDC, which our token-info catalog does not serve. Because
// `trading_vault::vault::release_external<T>` asserts `T` == the vault's
// deposit asset, a vault that funds a Bluefin account has to be *created*
// with that coin as its deposit asset — hence the catalog-shaped constant
// below, offered as an extra option in the create-vault form.
//
// The private key below is PUBLIC BY DESIGN. Bluefin publishes this account
// in their open-source SDK (github.com/fireflyprotocol/pro-sdk,
// `rust/src/env.rs`) explicitly so anyone can run their examples against
// staging. It is a shared, faucet-like testnet account holding a few million
// test USDC and a little gas — it guards nothing of value. NEVER put a key
// that does guard something here.
//
// Every consumer is gated on `BLUEFIN_TEST_ENABLED`.

import { Ed25519Keypair } from "@mysten/sui/keypairs/ed25519";
import { Transaction, coinWithBalance } from "@mysten/sui/transactions";
import { fromHex, normalizeStructTag } from "@mysten/sui/utils";
import type { SuiGrpcClient } from "@mysten/sui/grpc";

import { ENV, TOKEN_INFO_URL, type SupportedToken } from "./config";

/**
 * Whether the staging test affordances render at all.
 *
 * `ENV` alone is NOT a sufficient gate: the prod Vercel target also builds
 * with `VITE_ENVIRONMENT="testnet"` (prod runs on its own Sui testnet
 * deployment), so staging and prod differ only in the service routes baked
 * into the build — `https://sui-options.com/{staging,prod}/token-info`. Gate
 * on both: never on mainnet, never on a `/prod/` route. Local dev
 * (`http://127.0.0.1:9005`) and the staging preview target keep it.
 */
export const BLUEFIN_TEST_ENABLED: boolean =
  ENV !== "mainnet" && !/\/prod(\/|$)/.test(TOKEN_INFO_URL);

/**
 * Bluefin staging's margin asset, shaped like a token-info catalog entry so
 * the create-vault form and `tokenForCoinType()` can treat it as one.
 *
 * The ticker is plain "USDC" on purpose: `SweepBack` forwards this ticker to
 * Bluefin as their asset symbol, so a decorated one (e.g. "USDC*") would
 * break sweeps for exactly the vaults this exists to create. The
 * `(Bluefin staging)` name is what marks it in the picker.
 */
export const BLUEFIN_TEST_USDC: SupportedToken = {
  coinType: "0x1a67b3b13e8774bd5b746ac5a4acbcc15ed41010096fe642a1abf2e6f6e2285b::coin::COIN",
  ticker: "USDC",
  name: "USDC (Bluefin staging)",
  logoUri: null,
  decimals: 6,
  pythFeedId: null,
  enabled: true,
};

/** The published test account's address (see the file header). */
export const BLUEFIN_TEST_ACCOUNT = "0x9b11fafc580f23932f379d99ab6cc4c638e85ba4c252fc909296f3f9e6cea786";

/** Its ed25519 secret key (32-byte seed, hex). Public by design — see above. */
const BLUEFIN_TEST_SECRET_KEY_HEX =
  "3427d19dcf5781f0874c36c78aec22c03acda435d69efcbf249e8821793567a1";

/** Per-pull cap, in USDC smallest units. The account is shared — be a good
 * citizen and don't drain it in one click. */
export const BLUEFIN_TEST_MAX_PULL = 50_000;

/** True when `coinType` is the Bluefin staging USDC (canonicalized compare —
 * chain `TypeName`s arrive without the `0x`). */
export function isBluefinTestUsdc(coinType: string | null | undefined): boolean {
  if (!coinType) return false;
  try {
    return normalizeStructTag(coinType) === normalizeStructTag(BLUEFIN_TEST_USDC.coinType);
  } catch {
    return false;
  }
}

/**
 * Transfer `amountRaw` of the staging USDC from the published test account to
 * `recipient`, signed with the embedded key and paid for by the test
 * account's own gas. Resolves once the transfer is visible to reads, so the
 * caller can immediately spend the coins.
 */
export async function transferBluefinTestUsdc(
  client: SuiGrpcClient,
  recipient: string,
  amountRaw: bigint,
): Promise<string> {
  const keypair = Ed25519Keypair.fromSecretKey(fromHex(BLUEFIN_TEST_SECRET_KEY_HEX));
  const tx = new Transaction();
  tx.setSender(keypair.toSuiAddress());
  const coin = tx.add(coinWithBalance({ balance: amountRaw, type: BLUEFIN_TEST_USDC.coinType }));
  tx.transferObjects([coin], recipient);

  const res = await client.core.signAndExecuteTransaction({ transaction: tx, signer: keypair });
  // Like JSON-RPC's executeTransactionBlock, a FailedTransaction still carries
  // a digest and a status — mirror `tx/submit.ts` and read through both.
  const exec = res.Transaction ?? res.FailedTransaction;
  if (!exec.status.success) {
    throw new Error(`Bluefin test transfer failed: ${exec.status.error.message}`);
  }
  await client.core.waitForTransaction({ digest: exec.digest });
  return exec.digest;
}
