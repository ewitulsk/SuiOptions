// The ingress whitelist's four membership domains — mirrors the on-chain
// `u8` constants in `whitelist::whitelist`. Every gate names its domain at
// compile time, so membership on one domain never satisfies another's gate.

export const WHITELIST_DOMAINS = ["options", "exchange", "vaultCreate", "vaultLp"] as const;

export type DomainKey = (typeof WHITELIST_DOMAINS)[number];

/** On-chain `u8` constant per domain. */
export const DOMAIN_CODE: Record<DomainKey, number> = {
  options: 0,
  exchange: 1,
  vaultCreate: 2,
  vaultLp: 3,
};

/** The domain's field name on the shared `Whitelist` object's JSON. */
export const DOMAIN_FIELD: Record<DomainKey, string> = {
  options: "options",
  exchange: "exchange",
  vaultCreate: "vault_create",
  vaultLp: "vault_lp",
};

/** Human label per domain. */
export const DOMAIN_LABEL: Record<DomainKey, string> = {
  options: "options",
  exchange: "exchange",
  vaultCreate: "vault-create",
  vaultLp: "vault-lp",
};

/** What each domain actually gates (Admin screen hints). */
export const DOMAIN_HINT: Record<DomainKey, string> = {
  options: "writing/buying options + bucket creation",
  exchange: "exchange escrow deposits + fills",
  vaultCreate: "creating trading vaults",
  vaultLp: "vault deposits + commitment funding",
};
