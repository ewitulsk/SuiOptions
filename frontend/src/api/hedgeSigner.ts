// HTTP client for the hedge-signer's browser-facing surface (SO-305):
// the /frost/* ceremony endpoints (two-round DKG + two-round signing, the
// service being participant 2) and the /bluefin/* REST relay.

import { HEDGE_SIGNER_URL } from "../config";

/** A non-2xx response, carrying the status so callers can branch on it —
 * keygen round 1 answers 409 when the service already holds a share. */
export class HedgeSignerError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "HedgeSignerError";
  }
}

async function post<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(`${HEDGE_SIGNER_URL}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    const text = await res.text();
    throw new HedgeSignerError(
      res.status,
      `hedge-signer ${path}: ${res.status} ${text || res.statusText}`,
    );
  }
  return (await res.json()) as T;
}

/** The vault's FROST group key + parent address, or null when no keygen has
 * run yet (404). */
export async function fetchFrostPubkey(vaultId: string): Promise<{
  vaultId: string;
  groupPublicKeyHex: string;
  suiAddress: string;
} | null> {
  const res = await fetch(
    `${HEDGE_SIGNER_URL}/frost/pubkey/${encodeURIComponent(vaultId)}`,
  );
  if (res.status === 404) return null;
  if (!res.ok) {
    throw new Error(`hedge-signer /frost/pubkey: ${res.status} ${res.statusText}`);
  }
  const body = (await res.json()) as {
    vault_id: string;
    group_public_key_hex: string;
    sui_address: string;
  };
  return {
    vaultId: body.vault_id,
    groupPublicKeyHex: body.group_public_key_hex,
    suiAddress: body.sui_address,
  };
}

/** The registrar's attestation over this vault's FROST parent address
 * (SO-308): what `vault::set_external_account_attested` verifies so the
 * curator can register the account themselves, capped, without an admin.
 * 404 = no share for this vault yet (keygen hasn't run). */
export async function fetchRegistrationAttestation(vaultId: string): Promise<{
  parentAddress: string;
  /** Raw 64-byte ed25519 signature, hex — the `attestation` argument. */
  signatureHex: string;
}> {
  const res = await fetch(
    `${HEDGE_SIGNER_URL}/frost/registration/${encodeURIComponent(vaultId)}`,
  );
  if (res.status === 404) {
    throw new HedgeSignerError(
      404,
      "The signer holds no key share for this vault — complete the key ceremony first.",
    );
  }
  if (!res.ok) {
    const text = await res.text();
    throw new HedgeSignerError(
      res.status,
      `hedge-signer /frost/registration: ${res.status} ${text || res.statusText}`,
    );
  }
  const body = (await res.json()) as {
    parent_address: string;
    signature_hex: string;
  };
  return { parentAddress: body.parent_address, signatureHex: body.signature_hex };
}

export async function keygenRound1(
  vaultId: string,
  curatorRound1B64: string,
): Promise<{ serviceRound1B64: string }> {
  const body = await post<{ service_round1_b64: string }>("/frost/keygen/round1", {
    vault_id: vaultId,
    curator_round1_b64: curatorRound1B64,
  });
  return { serviceRound1B64: body.service_round1_b64 };
}

export async function keygenRound2(
  vaultId: string,
  curatorRound2B64: string,
): Promise<{ serviceRound2B64: string; groupPublicKeyHex: string; suiAddress: string }> {
  const body = await post<{
    service_round2_b64: string;
    group_public_key_hex: string;
    sui_address: string;
  }>("/frost/keygen/round2", {
    vault_id: vaultId,
    curator_round2_b64: curatorRound2B64,
  });
  return {
    serviceRound2B64: body.service_round2_b64,
    groupPublicKeyHex: body.group_public_key_hex,
    suiAddress: body.sui_address,
  };
}

export type FrostPayloadKind = "login" | "authorize_account" | "withdraw" | "sui_tx";

export async function signRound1(
  vaultId: string,
  payloadKind: FrostPayloadKind,
  payloadB64: string,
): Promise<{ sessionId: string; commitmentsB64: string; messageHex: string }> {
  const body = await post<{
    session_id: string;
    commitments_b64: string;
    message_hex: string;
  }>("/frost/sign/round1", {
    vault_id: vaultId,
    payload_kind: payloadKind,
    payload_b64: payloadB64,
  });
  return {
    sessionId: body.session_id,
    commitmentsB64: body.commitments_b64,
    messageHex: body.message_hex,
  };
}

export async function signRound2(
  sessionId: string,
  signingPackageB64: string,
): Promise<{ signatureShareB64: string }> {
  const body = await post<{ signature_share_b64: string }>("/frost/sign/round2", {
    session_id: sessionId,
    signing_package_b64: signingPackageB64,
  });
  return { signatureShareB64: body.signature_share_b64 };
}

// ── /bluefin/* relay ────────────────────────────────────────────────────────

/** One relayed Bluefin request. `host` selects the upstream (auth / data /
 * trade base URL from the signer's config); only allowlisted method+path
 * pairs are forwarded. Returns the raw Response — Bluefin errors keep their
 * status + JSON body for the caller to interpret. */
export function bluefinFetch(
  host: "auth" | "data" | "trade",
  path: string,
  init?: RequestInit,
): Promise<Response> {
  return fetch(`${HEDGE_SIGNER_URL}/bluefin/${host}${path}`, init);
}
