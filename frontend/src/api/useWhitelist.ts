// Live state of the ONE ingress whitelist (guarded launch): the standalone
// whitelist package's shared `Whitelist` object — four membership domains
// (options, exchange, vault_create, vault_lp), each `members:
// VecSet<address>` plus per-domain enforcement / pause flags. Direct chain
// read — no service serves this.

import { useQuery } from "@tanstack/react-query";
import { normalizeSuiAddress } from "@mysten/sui/utils";

import { WHITELIST_ID } from "../config";
import {
  DOMAIN_FIELD,
  WHITELIST_DOMAINS,
  type DomainKey,
} from "../lib/whitelistDomains";
import { useSuiGrpcClient } from "../lib/suiGrpc";
import { asRecord, structFields, vecSetItems } from "./vaultHoldings";

export type DomainState = {
  members: string[];
  enabled: boolean;
  paused: boolean;
};

export type WhitelistState = Record<DomainKey, DomainState>;

/** The domains `addr` is currently a member of. */
export function domainsOf(state: WhitelistState, addr: string): Set<DomainKey> {
  const normalized = normalizeSuiAddress(addr);
  return new Set(WHITELIST_DOMAINS.filter((d) => state[d].members.includes(normalized)));
}

/** Every address that is a member of at least one domain. */
export function allMembers(state: WhitelistState): string[] {
  const out = new Set<string>();
  for (const d of WHITELIST_DOMAINS) {
    for (const m of state[d].members) out.add(m);
  }
  return [...out].sort((a, b) => a.localeCompare(b));
}

export function useWhitelist() {
  const client = useSuiGrpcClient();
  return useQuery<WhitelistState, Error>({
    queryKey: ["ingress-whitelist", WHITELIST_ID],
    enabled: !!WHITELIST_ID,
    // Only moves on admin action; the section refetches after every tx.
    refetchInterval: 30_000,
    queryFn: async () => {
      const { object } = await client.core.getObject({
        objectId: WHITELIST_ID as string,
        include: { json: true },
      });
      const f = structFields(object.json) ?? asRecord(object.json);
      const parseDomain = (key: DomainKey): DomainState => {
        const d = structFields(f?.[DOMAIN_FIELD[key]]) ?? asRecord(f?.[DOMAIN_FIELD[key]]);
        return {
          // VecSet renders as `{ contents: [addr, …] }` (or a bare array).
          members: vecSetItems(d?.members)
            .filter((m): m is string => typeof m === "string")
            .map((m) => normalizeSuiAddress(m)),
          enabled: d?.enabled === true,
          paused: d?.paused === true,
        };
      };
      return {
        options: parseDomain("options"),
        exchange: parseDomain("exchange"),
        vaultCreate: parseDomain("vaultCreate"),
        vaultLp: parseDomain("vaultLp"),
      };
    },
  });
}
