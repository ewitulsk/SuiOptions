// Ceremony drivers (SO-305): the curator side of the hedge-signer's
// two-round DKG and two-round FROST signing, with progress callbacks for
// the UI. Pure async functions — React state stays in the components.

import {
  keygenRound1,
  keygenRound2,
  signRound1,
  signRound2,
  type FrostPayloadKind,
} from "../api/hedgeSigner";
import { bytesToBase64, loadFrost, suiSerializedSignature } from "./frost";

export type CeremonyProgress = (phase: string) => void;

export type KeygenOutcome = {
  keyPackageB64: string;
  publicKeyPackageB64: string;
  groupPublicKeyHex: string;
  /** The parent account: the group key's derived Sui address. */
  parentAddress: string;
};

/** Run the full two-round DKG against the hedge-signer. Both halves must
 * arrive at the same group key — verified here before returning. */
export async function runKeygenCeremony(
  vaultId: string,
  onProgress: CeremonyProgress,
): Promise<KeygenOutcome> {
  onProgress("Loading FROST participant…");
  const frost = await loadFrost();

  onProgress("DKG round 1: generating curator package…");
  const session = new frost.KeygenSession();
  try {
    const r1 = await keygenRound1(vaultId, session.round1_package_b64);

    onProgress("DKG round 2: exchanging packages…");
    const curatorRound2 = session.round2(r1.serviceRound1B64);
    const r2 = await keygenRound2(vaultId, curatorRound2);

    onProgress("Finalizing key shares…");
    const done = session.finish(r2.serviceRound2B64);
    if (done.group_public_key_hex !== r2.groupPublicKeyHex) {
      throw new Error(
        "DKG mismatch: curator and service derived different group keys — aborting",
      );
    }
    return {
      keyPackageB64: done.key_package_b64,
      publicKeyPackageB64: done.public_key_package_b64,
      groupPublicKeyHex: done.group_public_key_hex,
      parentAddress: done.sui_address,
    };
  } finally {
    session.free();
  }
}

export type SignOutcome = {
  /** 64-byte ed25519 group signature, hex. */
  signatureHex: string;
  /** base64( 0x00 || signature || group pubkey ) — the Sui serialized form
   * both Sui tx auth and Bluefin's request verifier consume. */
  suiSignatureB64: string;
  messageHex: string;
};

/** Run one two-round signing ceremony over `payloadBytes` (the exact JSON a
 * Bluefin request signs, or bcs(TransactionData) for `sui_tx`). The digest
 * is computed locally and compared with the digest the service approved —
 * the ceremony never signs a message this client didn't derive itself. */
export async function runSignCeremony(params: {
  vaultId: string;
  payloadKind: FrostPayloadKind;
  payloadBytes: Uint8Array;
  keyPackageB64: string;
  publicKeyPackageB64: string;
  groupPublicKeyHex: string;
  onProgress: CeremonyProgress;
}): Promise<SignOutcome> {
  const { onProgress } = params;
  onProgress("Loading FROST participant…");
  const frost = await loadFrost();

  const expectedHex =
    params.payloadKind === "sui_tx"
      ? frost.transaction_digest(params.payloadBytes)
      : frost.personal_message_digest(params.payloadBytes);

  onProgress("Sign round 1: requesting policy approval…");
  const session = new frost.SignSession(params.keyPackageB64);
  try {
    const r1 = await signRound1(
      params.vaultId,
      params.payloadKind,
      bytesToBase64(params.payloadBytes),
    );
    if (r1.messageHex !== expectedHex) {
      throw new Error(
        "Digest mismatch: the signer approved a different message than this payload — aborting",
      );
    }

    onProgress("Sign round 2: exchanging signature shares…");
    const r2 = session.round2(r1.messageHex, r1.commitmentsB64);
    const svc = await signRound2(r1.sessionId, r2.signing_package_b64);

    onProgress("Aggregating + verifying signature…");
    const signatureHex = frost.aggregate_signature(
      r2.signing_package_b64,
      r2.signature_share_b64,
      svc.signatureShareB64,
      params.publicKeyPackageB64,
    );
    return {
      signatureHex,
      suiSignatureB64: suiSerializedSignature(signatureHex, params.groupPublicKeyHex),
      messageHex: r1.messageHex,
    };
  } finally {
    session.free();
  }
}
