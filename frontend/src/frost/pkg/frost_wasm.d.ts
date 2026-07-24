/* tslint:disable */
/* eslint-disable */

/**
 * Result of a completed DKG: the curator's share material and the group
 * identity. `key_package_b64` is the curator's long-lived secret share —
 * the TS wrapper must encrypt it before persisting.
 */
export class KeygenResult {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    /**
     * 32-byte group ed25519 public key, hex (no 0x).
     */
    group_public_key_hex: string;
    key_package_b64: string;
    public_key_package_b64: string;
    /**
     * Sui address derived from the group key — the parent account address.
     */
    sui_address: string;
}

/**
 * Curator side of the two-round DKG. One instance per ceremony:
 * `new()` → send `round1_package_b64()` to the service →
 * `round2(service_round1)` → send the result to the service →
 * `finish(service_round2)`.
 */
export class KeygenSession {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Consume the service's round-2 package and finalize (part 3).
     */
    finish(service_round2_b64: string): KeygenResult;
    /**
     * Run DKG part 1 for the curator (participant 1 of a 2-of-2).
     */
    constructor();
    /**
     * Consume the service's round-1 package, produce the curator's round-2
     * package addressed to the service.
     */
    round2(service_round1_b64: string): string;
    /**
     * The curator's round-1 package for the service.
     */
    readonly round1_package_b64: string;
}

/**
 * Result of the curator's signing round 2: the `SigningPackage` to relay to
 * the service and the curator's own signature share.
 */
export class SignRound2Result {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    signature_share_b64: string;
    signing_package_b64: string;
}

/**
 * Curator side of one two-round FROST signing ceremony:
 * `new(key_package)` → send `commitments_b64()` with the payload to the
 * service's `/frost/sign/round1` → `round2(message_hex, service
 * commitments)` → relay the signing package to `/frost/sign/round2` →
 * `aggregate(...)` with both shares.
 */
export class SignSession {
    free(): void;
    [Symbol.dispose](): void;
    constructor(key_package_b64: string);
    /**
     * Build the `SigningPackage` over the service-approved digest
     * (`message_hex`, 32 bytes) and produce the curator's signature share.
     * Nonces are single-use: a second call fails.
     */
    round2(message_hex: string, service_commitments_b64: string): SignRound2Result;
    /**
     * The curator's nonce commitments for the service's round 1.
     */
    readonly commitments_b64: string;
}

/**
 * Aggregate both signature shares into the group's plain ed25519 signature
 * (64 bytes, hex). Verified against the group key before returning — an
 * invalid share fails here, never on-chain.
 */
export function aggregate_signature(signing_package_b64: string, curator_share_b64: string, service_share_b64: string, public_key_package_b64: string): string;

/**
 * Re-derive the group identity from a stored `PublicKeyPackage` (used when
 * resuming: verify a cached share still matches the vault's parent).
 */
export function group_identity(public_key_package_b64: string): KeygenResult;

/**
 * The 32-byte digest a Sui ed25519 key signs for a personal message:
 * `blake2b256( [3,0,0] || bcs(PersonalMessage{ message }) )`. Mirrors
 * hedge-signer's `policy::bluefin::personal_message_digest`.
 */
export function personal_message_digest(message: Uint8Array): string;

/**
 * The 32-byte digest a Sui ed25519 key signs for a transaction:
 * `blake2b256( [0,0,0] || bcs(TransactionData) )`.
 */
export function transaction_digest(tx_bytes: Uint8Array): string;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_get_keygenresult_group_public_key_hex: (a: number) => [number, number];
    readonly __wbg_get_keygenresult_key_package_b64: (a: number) => [number, number];
    readonly __wbg_get_keygenresult_public_key_package_b64: (a: number) => [number, number];
    readonly __wbg_get_keygenresult_sui_address: (a: number) => [number, number];
    readonly __wbg_keygenresult_free: (a: number, b: number) => void;
    readonly __wbg_keygensession_free: (a: number, b: number) => void;
    readonly __wbg_set_keygenresult_group_public_key_hex: (a: number, b: number, c: number) => void;
    readonly __wbg_set_keygenresult_key_package_b64: (a: number, b: number, c: number) => void;
    readonly __wbg_set_keygenresult_public_key_package_b64: (a: number, b: number, c: number) => void;
    readonly __wbg_set_keygenresult_sui_address: (a: number, b: number, c: number) => void;
    readonly __wbg_signround2result_free: (a: number, b: number) => void;
    readonly __wbg_signsession_free: (a: number, b: number) => void;
    readonly aggregate_signature: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number) => [number, number, number, number];
    readonly group_identity: (a: number, b: number) => [number, number, number];
    readonly keygensession_finish: (a: number, b: number, c: number) => [number, number, number];
    readonly keygensession_new: () => [number, number, number];
    readonly keygensession_round1_package_b64: (a: number) => [number, number];
    readonly keygensession_round2: (a: number, b: number, c: number) => [number, number, number, number];
    readonly personal_message_digest: (a: number, b: number) => [number, number];
    readonly signsession_commitments_b64: (a: number) => [number, number];
    readonly signsession_new: (a: number, b: number) => [number, number, number];
    readonly signsession_round2: (a: number, b: number, c: number, d: number, e: number) => [number, number, number];
    readonly transaction_digest: (a: number, b: number) => [number, number];
    readonly __wbg_set_signround2result_signature_share_b64: (a: number, b: number, c: number) => void;
    readonly __wbg_set_signround2result_signing_package_b64: (a: number, b: number, c: number) => void;
    readonly __wbg_get_signround2result_signature_share_b64: (a: number) => [number, number];
    readonly __wbg_get_signround2result_signing_package_b64: (a: number) => [number, number];
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
