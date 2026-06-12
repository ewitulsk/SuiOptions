import { describe, expect, it } from "vitest";
import { Ed25519Keypair } from "@mysten/sui/keypairs/ed25519";
import { Transaction } from "@mysten/sui/transactions";
import { toBase64 } from "@mysten/sui/utils";
import { messageWithIntent } from "@mysten/sui/cryptography";

import {
  GasStationSponsorClient,
  SponsorUnavailableError,
  type GasStationAdapter,
} from "./sponsor.js";

// The invariant the pipeline must never break: the session key signs the
// EXACT bytes the sponsor signed, and both signatures are submitted.

function simpleTx(): Transaction {
  const tx = new Transaction();
  // Explicit object ref + transferObjects: buildable without any RPC
  // resolution, so the mock client only needs `executeTransactionBlock`.
  tx.transferObjects(
    [
      tx.objectRef({
        objectId: "0x" + "6".padStart(64, "0"),
        version: "1",
        digest: "11111111111111111111111111111111",
      }),
    ],
    tx.pure.address("0x2"),
  );
  return tx;
}

function mockClient(executed: any[]) {
  return {
    executeTransactionBlock: async (args: unknown) => {
      executed.push(args);
      return { digest: "MOCKDIGEST" };
    },
  } as any;
}

describe("GasStationSponsorClient", () => {
  it("signs the sponsor's exact bytes and submits both signatures", async () => {
    const signer = new Ed25519Keypair();
    const executed: any[] = [];
    const sponsorBytes = new Uint8Array([9, 9, 9, 1, 2, 3]);
    let reservedSender: string | null = null;
    let reservedKind: Uint8Array | null = null;

    const adapter: GasStationAdapter = {
      async reserve(sender, kindBytes) {
        reservedSender = sender;
        reservedKind = kindBytes;
        return { txBytes: toBase64(sponsorBytes), sponsorSignature: "U1BPTlNPUg==" };
      },
    };

    const client = new GasStationSponsorClient(mockClient(executed), adapter);
    const res = await client.executeSponsored(simpleTx(), signer);

    expect(res.digest).toBe("MOCKDIGEST");
    expect(reservedSender).toBe(signer.toSuiAddress());
    expect(reservedKind).not.toBeNull();

    expect(executed).toHaveLength(1);
    const { transactionBlock, signature } = executed[0];
    // submits exactly the sponsor's bytes…
    expect(transactionBlock).toBe(toBase64(sponsorBytes));
    // …with [userSig, sponsorSig]
    expect(signature).toHaveLength(2);
    expect(signature[1]).toBe("U1BPTlNPUg==");
    // and the user signature verifies over those same bytes.
    const intentMessage = messageWithIntent("TransactionData", sponsorBytes);
    const ok = await signer
      .getPublicKey()
      .verifyWithIntent(sponsorBytes, signature[0], "TransactionData");
    expect(intentMessage.length).toBeGreaterThan(sponsorBytes.length);
    expect(ok).toBe(true);
  });

  it("wraps reservation failures in SponsorUnavailableError", async () => {
    const adapter: GasStationAdapter = {
      async reserve() {
        throw new Error("boom");
      },
    };
    const client = new GasStationSponsorClient(mockClient([]), adapter);
    await expect(
      client.executeSponsored(simpleTx(), new Ed25519Keypair()),
    ).rejects.toBeInstanceOf(SponsorUnavailableError);
  });
});
