// Demo sponsor / gas payer.
//
// DEMO ONLY: a real deployment runs the sponsor as a backend relayer (see the
// SDK's HttpSponsorClient). Here we keep a gas keypair in localStorage purely
// so the demo runs end-to-end in the browser without a separate service. The
// sponsor pays gas and seeds the user's Account; it can NEVER move user funds
// (that needs the SessionCap, which it doesn't hold).

import type { SuiJsonRpcClient } from "@mysten/sui/jsonRpc";
import { Ed25519Keypair } from "@mysten/sui/keypairs/ed25519";
import { Transaction } from "@mysten/sui/transactions";
import { LocalSponsorClient } from "@yourorg/sui-siws-session";

import { COIN_TYPE, PACKAGE_ID, SPONSORED_TARGETS } from "./config";

const STORAGE_KEY = "siws-demo-sponsor-key";

let cached: Ed25519Keypair | null = null;

/** Load (or lazily create) the demo sponsor keypair. */
export function getSponsorKeypair(): Ed25519Keypair {
  if (cached) return cached;
  const stored = localStorage.getItem(STORAGE_KEY);
  if (stored) {
    cached = Ed25519Keypair.fromSecretKey(stored);
  } else {
    cached = Ed25519Keypair.generate();
    localStorage.setItem(STORAGE_KEY, cached.getSecretKey());
  }
  return cached;
}

export function sponsorAddress(): string {
  return getSponsorKeypair().toSuiAddress();
}

export function makeSponsorClient(client: SuiJsonRpcClient): LocalSponsorClient {
  return new LocalSponsorClient(client, getSponsorKeypair(), {
    allowedTargets: SPONSORED_TARGETS,
  });
}

/**
 * Seed the user's shared Account with `amount` MIST, paid by the sponsor.
 * Anyone may deposit (funds only leave via a SessionCap), so this is a plain
 * sponsor-signed tx — not part of the session flow.
 */
export async function fundAccount(
  client: SuiJsonRpcClient,
  accountId: string,
  amount: bigint,
): Promise<string> {
  const sponsor = getSponsorKeypair();
  const tx = new Transaction();
  const [coin] = tx.splitCoins(tx.gas, [amount]);
  tx.moveCall({
    target: `${PACKAGE_ID}::account::deposit`,
    typeArguments: [COIN_TYPE],
    arguments: [tx.object(accountId), coin],
  });
  const res = await client.signAndExecuteTransaction({
    transaction: tx,
    signer: sponsor,
    options: { showEffects: true },
  });
  return res.digest;
}

/**
 * Coins that were transferred straight to the Account's object id (they sit as
 * `AddressOwner` objects "at" that id, NOT inside `funds`). These are the ones a
 * sweep would reclaim. The Account's internal `funds` Balance is not a coin
 * object, so it never shows up here.
 */
export async function strayCoins(
  client: SuiJsonRpcClient,
  accountId: string,
): Promise<{ count: number; total: bigint }> {
  const { data } = await client.getCoins({ owner: accountId, coinType: COIN_TYPE });
  return {
    count: data.length,
    total: data.reduce((sum, c) => sum + BigInt(c.balance), 0n),
  };
}

/**
 * Sweep every stray coin at the Account's id into its `funds`, batched into one
 * sponsor-signed tx via the permissionless `account::receive` entrypoint.
 */
export async function sweepAccount(
  client: SuiJsonRpcClient,
  accountId: string,
): Promise<{ count: number; digest: string | null }> {
  const { data: coins } = await client.getCoins({ owner: accountId, coinType: COIN_TYPE });
  if (coins.length === 0) return { count: 0, digest: null };

  const tx = new Transaction();
  for (const c of coins) {
    tx.moveCall({
      target: `${PACKAGE_ID}::account::receive`,
      typeArguments: [COIN_TYPE],
      arguments: [
        tx.object(accountId),
        tx.receivingRef({
          objectId: c.coinObjectId,
          version: c.version,
          digest: c.digest,
        }),
      ],
    });
  }
  const res = await client.signAndExecuteTransaction({
    transaction: tx,
    signer: getSponsorKeypair(),
    options: { showEffects: true },
  });
  return { count: coins.length, digest: res.digest };
}
