// Sponsored-transaction execution (spec §2.5).
//
// The sponsor pays gas and co-signs; it CANNOT move user funds (that requires
// the SessionCap, which the sponsor never holds). Two implementations:
//
//   - HttpSponsorClient: production shape. Hands the tx *kind* to a backend
//     relayer you run; the relayer attaches gas, signs the gas, and returns the
//     full tx bytes + sponsor signature. The client then signs those same bytes
//     with the session key and submits. The relayer enforces its own call
//     allowlist (defense in depth).
//
//   - LocalSponsorClient: dev/demo only. Plays the relayer role in-process with
//     a local gas keypair. Convenient for running the demo end-to-end without a
//     separate service. DO NOT ship a gas key to the browser in production.

import type { Signer } from "@mysten/sui/cryptography";
import { Transaction } from "@mysten/sui/transactions";
import { fromBase64, toBase64 } from "@mysten/sui/utils";
import type { SuiRpcClient, SuiTransactionBlockResponse } from "./client.js";

export interface SponsorClient {
  /** Co-sign + pay gas for `tx`, authorized by the session `signer`. */
  executeSponsored(
    tx: Transaction,
    signer: Signer,
  ): Promise<SuiTransactionBlockResponse>;
}

const SUI_COIN = "0x2::sui::SUI";
const DEFAULT_GAS_BUDGET = 100_000_000n;

const RESPONSE_OPTIONS = {
  showEffects: true,
  showObjectChanges: true,
} as const;

/** Extract `pkg::module::function` targets from a tx for allowlist checks. */
function moveCallTargets(tx: Transaction): string[] {
  return tx
    .getData()
    .commands.filter((c) => c.$kind === "MoveCall")
    .map((c) => {
      const mc = c.MoveCall!;
      return `${mc.package}::${mc.module}::${mc.function}`;
    });
}

function assertAllowed(tx: Transaction, allowedTargets?: string[]) {
  if (!allowedTargets) return;
  for (const target of moveCallTargets(tx)) {
    // `package` is "0x0" inside an unpublished tx; compare by module::function
    // suffix as well so config can list either form.
    const suffix = target.split("::").slice(1).join("::");
    const ok = allowedTargets.some((a) => a === target || a.endsWith(`::${suffix}`));
    if (!ok) throw new Error(`sponsor: call to ${target} is not on the allowlist`);
  }
}

export interface LocalSponsorOptions {
  /** Restrict which move-call targets the sponsor will pay for. */
  allowedTargets?: string[];
  gasBudget?: bigint;
}

export class LocalSponsorClient implements SponsorClient {
  constructor(
    private readonly client: SuiRpcClient,
    private readonly sponsor: Signer,
    private readonly options: LocalSponsorOptions = {},
  ) {}

  async executeSponsored(
    tx: Transaction,
    signer: Signer,
  ): Promise<SuiTransactionBlockResponse> {
    assertAllowed(tx, this.options.allowedTargets);

    const sponsorAddress = this.sponsor.toSuiAddress();
    tx.setSender(signer.toSuiAddress());
    tx.setGasOwner(sponsorAddress);
    tx.setGasBudget(this.options.gasBudget ?? DEFAULT_GAS_BUDGET);

    const { data: coins } = await this.client.getCoins({
      owner: sponsorAddress,
      coinType: SUI_COIN,
    });
    if (coins.length === 0) {
      throw new Error(`sponsor ${sponsorAddress} has no SUI gas coins`);
    }
    tx.setGasPayment(
      coins.slice(0, 8).map((c) => ({
        objectId: c.coinObjectId,
        version: c.version,
        digest: c.digest,
      })),
    );

    const bytes = await tx.build({ client: this.client });
    const userSig = (await signer.signTransaction(bytes)).signature;
    const sponsorSig = (await this.sponsor.signTransaction(bytes)).signature;

    return this.client.executeTransactionBlock({
      transactionBlock: bytes,
      signature: [userSig, sponsorSig],
      options: RESPONSE_OPTIONS,
    });
  }
}

/** What a relayer backend returns for a kind-only tx. */
export interface SponsoredReservation {
  /** Base64 full transaction bytes (sender + gas data filled in). */
  txBytes: string;
  /** The sponsor's signature over `txBytes`. */
  sponsorSignature: string;
}

export interface HttpSponsorOptions {
  /** POST endpoint that accepts `{ txKindBytes, sender, allowedTargets }`. */
  endpoint: string;
  /** Optional bearer token / api key. */
  headers?: Record<string, string>;
}

export class HttpSponsorClient implements SponsorClient {
  constructor(
    private readonly client: SuiRpcClient,
    private readonly options: HttpSponsorOptions,
  ) {}

  async executeSponsored(
    tx: Transaction,
    signer: Signer,
  ): Promise<SuiTransactionBlockResponse> {
    const sender = signer.toSuiAddress();
    tx.setSender(sender);
    const txKindBytes = await tx.build({
      client: this.client,
      onlyTransactionKind: true,
    });

    const res = await fetch(this.options.endpoint, {
      method: "POST",
      headers: { "content-type": "application/json", ...this.options.headers },
      body: JSON.stringify({
        sender,
        txKindBytes: toBase64(txKindBytes),
      }),
    });
    if (!res.ok) {
      throw new Error(`sponsor request failed: ${res.status} ${await res.text()}`);
    }
    const reservation = (await res.json()) as SponsoredReservation;

    // Sign the SAME bytes the sponsor signed.
    const userSig = (await signer.signTransaction(
      fromBase64(reservation.txBytes),
    )).signature;

    return this.client.executeTransactionBlock({
      transactionBlock: reservation.txBytes,
      signature: [userSig, reservation.sponsorSignature],
      options: RESPONSE_OPTIONS,
    });
  }
}
