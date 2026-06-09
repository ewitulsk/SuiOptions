import { describe, expect, it } from "vitest";
import { serializeRevokeMessage, serializeSessionMessage } from "./message.js";

// These two reference strings are byte-for-byte identical to the literals
// asserted in `contracts/tests/session_tests.move`. If you change the
// serializer, change BOTH sides and regenerate the references.

const fill = (v: number) => new Uint8Array(32).fill(v);

const SESSION_REF =
  "siws-session-v1\n" +
  "domain: 0x0000000000000000000000000000000000000000000000000000000000000001\n" +
  "chain: sui:testnet\n" +
  "account: 0x1111111111111111111111111111111111111111111111111111111111111111\n" +
  "session_key: 0x0000000000000000000000000000000000000000000000000000000000000002\n" +
  "generation: 7\n" +
  "nonce: 0x2222222222222222222222222222222222222222222222222222222222222222\n" +
  "expires_at_ms: 1700000000000";

const REVOKE_REF =
  "siws-session-revoke-v1\n" +
  "domain: 0x0000000000000000000000000000000000000000000000000000000000000001\n" +
  "chain: sui:testnet\n" +
  "account: 0x1111111111111111111111111111111111111111111111111111111111111111\n" +
  "account_id: 0x0000000000000000000000000000000000000000000000000000000000000003\n" +
  "nonce: 0x2222222222222222222222222222222222222222222222222222222222222222\n" +
  "expires_at_ms: 1700000000000";

describe("canonical serializer", () => {
  it("matches the on-chain session message byte-for-byte", () => {
    const bytes = serializeSessionMessage({
      domain: "0x1",
      network: "testnet",
      solanaPk: fill(0x11),
      sessionKey: "0x2",
      generation: 7,
      nonce: fill(0x22),
      expiresAtMs: 1700000000000,
    });
    expect(new TextDecoder().decode(bytes)).toBe(SESSION_REF);
  });

  it("matches the on-chain revoke message byte-for-byte", () => {
    const bytes = serializeRevokeMessage({
      domain: "0x1",
      network: "testnet",
      solanaPk: fill(0x11),
      accountId: "0x3",
      nonce: fill(0x22),
      expiresAtMs: 1700000000000,
    });
    expect(new TextDecoder().decode(bytes)).toBe(REVOKE_REF);
  });
});
