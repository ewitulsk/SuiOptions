import { describe, expect, it } from "vitest";
import {
  buildSiweSessionMessage,
  buildSiweWithdrawMessage,
  ethSignatureToSui,
} from "./siwe.js";

// Byte-for-byte identical to the literal asserted in
// `contracts/tests/siwe_tests.move` (and the message the reference secp256k1
// key signed in that vector). Change both sides together if the format
// changes; regenerate with `gen-siwe.mjs`.

const ETH = Uint8Array.from(
  "1a642f0e3c3af545e7acbd38b07251b3990914f1".match(/../g)!.map((h) => parseInt(h, 16)),
);
const nonce = new Uint8Array(32).fill(0x22);

const LIMITS = [
  { coinType: "0x2::sui::SUI", perTx: 200n, total: 500n },
  {
    coinType:
      "0x00000000000000000000000000000000000000000000000000000000000000aa::tusdc::TUSDC",
    perTx: 1000000n,
    total: 5000000n,
  },
];

const SIWE_REF =
  "siws-session.demo wants you to sign in with your Ethereum account:\n" +
  "0x1a642f0E3c3aF545E7AcBD38b07251B3990914F1\n\n" +
  "Authorize a Sui session key.\n\n" +
  "URI: https://siws-session.demo\n" +
  "Version: 1\n" +
  "Chain ID: 1\n" +
  "Nonce: 2222222222222222222222222222222222222222222222222222222222222222\n" +
  "Issued At: 2026-06-09T00:00:00.000Z\n" +
  "Resources:\n" +
  "- siws-session://sui-registry/0x0000000000000000000000000000000000000000000000000000000000000001\n" +
  "- siws-session://session-key/0x0000000000000000000000000000000000000000000000000000000000000002\n" +
  "- siws-session://generation/0\n" +
  "- siws-session://expires/1700000000000\n" +
  "- siws-session://limits/0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI=200/500," +
  "0x00000000000000000000000000000000000000000000000000000000000000aa::tusdc::TUSDC=1000000/5000000";

const SIWE_WITHDRAW_REF =
  "siws-session.demo wants you to sign in with your Ethereum account:\n" +
  "0x1a642f0E3c3aF545E7AcBD38b07251B3990914F1\n\n" +
  "Authorize a Sui withdrawal.\n\n" +
  "URI: https://siws-session.demo\n" +
  "Version: 1\n" +
  "Chain ID: 1\n" +
  "Nonce: 2222222222222222222222222222222222222222222222222222222222222222\n" +
  "Issued At: 2026-06-09T00:00:00.000Z\n" +
  "Resources:\n" +
  "- siws-session://sui-registry/0x0000000000000000000000000000000000000000000000000000000000000001\n" +
  "- siws-session://account/0x0000000000000000000000000000000000000000000000000000000000000003\n" +
  "- siws-session://coin-type/0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI\n" +
  "- siws-session://amount/250000\n" +
  "- siws-session://recipient/0x0000000000000000000000000000000000000000000000000000000000000004\n" +
  "- siws-session://expires/1700000000000";

describe("SIWE serializer", () => {
  it("matches the on-chain EIP-4361 message byte-for-byte (incl. EIP-55)", () => {
    const msg = buildSiweSessionMessage({
      registryDomain: "0x1",
      ethAddress: ETH,
      sessionKey: "0x2",
      generation: 0,
      nonce,
      expiresAtMs: 1700000000000,
      chainId: 1,
      issuedAt: "2026-06-09T00:00:00.000Z",
      limits: LIMITS,
    });
    expect(msg).toBe(SIWE_REF);
  });

  it("matches the on-chain EIP-4361 withdraw message byte-for-byte", () => {
    const msg = buildSiweWithdrawMessage({
      registryDomain: "0x1",
      ethAddress: ETH,
      accountId: "0x3",
      coinType: "0x2::sui::SUI",
      amount: 250000n,
      recipient: "0x4",
      nonce,
      expiresAtMs: 1700000000000,
      chainId: 1,
      issuedAt: "2026-06-09T00:00:00.000Z",
    });
    expect(msg).toBe(SIWE_WITHDRAW_REF);
  });

  it("normalizes MetaMask v=27/28 to Sui's 0/1", () => {
    const sig = "0x" + "ab".repeat(64) + "1c"; // v = 0x1c = 28
    const out = ethSignatureToSui(sig);
    expect(out.length).toBe(65);
    expect(out[64]).toBe(1);
  });
});
