// Sponsored-transaction execution (spec §2.5).
//
// The sponsor pays gas and co-signs; it CANNOT move user funds (that requires
// the SessionCap, which the sponsor never holds).
//
// The production shape is `GasStationSponsorClient`: it owns the pipeline that
// must never vary between integrations — set sender → build the tx *kind* →
// obtain a gas reservation → sign the sponsor's EXACT bytes with the session
// key → submit both signatures — and delegates only the reservation step to a
// `GasStationAdapter`. Any backend that can take kind-bytes and return signed
// full-tx bytes plugs in as an adapter; `suiOptionsGasStation` ships the
// adapter for the SuiOptions rust gas-station wire format.
//
// `LocalSponsorClient` is dev/demo only: it plays the relayer role in-process
// with a local gas keypair. DO NOT ship a gas key to the browser in production.

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

// ----------------------------------------------------------- external station

/** What a gas station returns for a kind-only tx. */
export interface SponsoredReservation {
  /** Base64 full transaction bytes (sender + gas data filled in). */
  txBytes: string;
  /** The sponsor's signature over `txBytes`. */
  sponsorSignature: string;
}

export interface GasStationHealth {
  ok: boolean;
  /** Sponsor balance in MIST, when the station reports one. */
  balanceMist?: bigint;
}

/**
 * The integration contract for an external gas station: attach gas to and
 * sponsor-sign a kind-only transaction. Implement `reserve` against your
 * station's wire format; the SDK owns everything else.
 */
export interface GasStationAdapter {
  reserve(sender: string, txKindBytes: Uint8Array): Promise<SponsoredReservation>;
  /** Optional: sponsor health, for UI (e.g. a balance indicator). */
  health?(): Promise<GasStationHealth>;
}

/**
 * Thrown when the gas station refuses or fails a reservation. Session keys
 * own no gas, so there is no wallet-pays fallback — surface this to the user.
 */
export class SponsorUnavailableError extends Error {
  constructor(message: string, readonly cause?: unknown) {
    super(message);
    this.name = "SponsorUnavailableError";
  }
}

export class GasStationSponsorClient implements SponsorClient {
  constructor(
    private readonly client: SuiRpcClient,
    readonly adapter: GasStationAdapter,
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

    let reservation: SponsoredReservation;
    try {
      reservation = await this.adapter.reserve(sender, txKindBytes);
    } catch (e) {
      throw e instanceof SponsorUnavailableError
        ? e
        : new SponsorUnavailableError(
            `gas station reservation failed: ${e instanceof Error ? e.message : String(e)}`,
            e,
          );
    }

    // Sign the SAME bytes the sponsor signed.
    const userSig = (await signer.signTransaction(
      fromBase64(reservation.txBytes),
    )).signature;

    const res = await this.client.executeTransactionBlock({
      transactionBlock: reservation.txBytes,
      signature: [userSig, reservation.sponsorSignature],
      options: RESPONSE_OPTIONS,
    });
    // `executeTransactionBlock` defaults to WaitForEffectsCert: the tx is
    // finalized but the fullnode we read from may not have applied its effects
    // yet. Block until it's queryable so the immediate custody re-read
    // (refreshSession → readAccountBalances) sees the new state.
    await this.client.waitForTransaction({ digest: res.digest });
    return res;
  }
}

export interface SuiOptionsGasStationOptions {
  /** Extra headers (bearer token / api key). */
  headers?: Record<string, string>;
}

/**
 * Adapter for the SuiOptions rust gas-station service
 * (`rust-backend/services/gas-station`): `POST /sponsor` with
 * `{ sender, kind_bytes }`, `GET /balance` for health. The station validates
 * the PTB against its template allowlist and dry-runs before signing.
 */
export function suiOptionsGasStation(
  baseUrl: string,
  options: SuiOptionsGasStationOptions = {},
): GasStationAdapter {
  const base = baseUrl.replace(/\/+$/, "");
  return {
    async reserve(sender, txKindBytes) {
      const res = await fetch(`${base}/sponsor`, {
        method: "POST",
        headers: { "content-type": "application/json", ...options.headers },
        body: JSON.stringify({ sender, kind_bytes: toBase64(txKindBytes) }),
      });
      if (!res.ok) {
        throw new SponsorUnavailableError(
          `gas station refused sponsorship: ${res.status} ${await res.text()}`,
        );
      }
      const body = (await res.json()) as {
        tx_bytes: string;
        sponsor_signature: string;
      };
      return { txBytes: body.tx_bytes, sponsorSignature: body.sponsor_signature };
    },
    async health() {
      try {
        const res = await fetch(`${base}/balance`, { headers: options.headers });
        if (!res.ok) return { ok: false };
        const body = (await res.json()) as { balance_mist: string; healthy: boolean };
        return { ok: body.healthy, balanceMist: BigInt(body.balance_mist) };
      } catch {
        return { ok: false };
      }
    },
  };
}

// ------------------------------------------------------------------ dev/demo

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
