// Exchange BalanceManager (maker escrow) discovery (SO-416).
//
// `balance_manager::new()` emits no event, so a fresh manager is only known
// from its creation tx effects — TradePanel caches that id here right after
// "enable escrow". Recovery for a cleared browser profile rides the
// `DepositEvent{manager, owner, …}` the FIRST deposit emits (every maker
// order requires a deposit, so any manager that ever escrowed funds is
// recoverable). Event queries go through the Sui GraphQL RPC, mirroring the
// old DeepBook BalanceManagerEvent discovery.

import { useQuery } from "@tanstack/react-query";
import { normalizeSuiAddress } from "@mysten/sui/utils";

import { EXCHANGE_PACKAGE_ID } from "../config";
import { suiGraphqlQuery, type SuiNetwork } from "../lib/suiGrpc";

const BM_CACHE_PREFIX = "pismo-exchange-bm-";

type DepositEventsPage = {
  events: {
    pageInfo: { hasPreviousPage: boolean; startCursor: string | null };
    nodes: Array<{
      contents: { json: { owner?: string; manager?: string } | null } | null;
    }>;
  };
};

// `last:` + `before:` walks the event stream newest-first.
const DEPOSIT_EVENTS_QUERY = `
  query($type: String!, $before: String) {
    events(last: 50, before: $before, filter: { type: $type }) {
      pageInfo { hasPreviousPage startCursor }
      nodes { contents { json } }
    }
  }`;

export async function findExchangeBalanceManager(
  network: SuiNetwork,
  owner: string,
): Promise<string | null> {
  const pkg = EXCHANGE_PACKAGE_ID;
  if (!pkg) return null;

  // The exchange package is republished on every redeploy, so key the cache
  // by package too — a stale manager from a dead deployment must not stick.
  const cacheKey = `${BM_CACHE_PREFIX}${pkg}-${owner}`;
  const cached = localStorage.getItem(cacheKey);
  if (cached) return cached;

  const want = normalizeSuiAddress(owner);
  let before: string | null = null;
  for (let page = 0; page < 5; page++) {
    const res: DepositEventsPage = await suiGraphqlQuery<DepositEventsPage>(
      network,
      DEPOSIT_EVENTS_QUERY,
      { type: `${pkg}::balance_manager::DepositEvent`, before },
    );
    // Nodes come back oldest-first within the page; scan newest-first.
    for (const ev of [...res.events.nodes].reverse()) {
      const json = ev.contents?.json;
      if (json?.owner && normalizeSuiAddress(json.owner) === want && json.manager) {
        localStorage.setItem(cacheKey, json.manager);
        return json.manager;
      }
    }
    if (!res.events.pageInfo.hasPreviousPage || !res.events.pageInfo.startCursor) break;
    before = res.events.pageInfo.startCursor;
  }
  return null;
}

export function cacheExchangeBalanceManager(owner: string, bmId: string) {
  if (!EXCHANGE_PACKAGE_ID) return;
  localStorage.setItem(`${BM_CACHE_PREFIX}${EXCHANGE_PACKAGE_ID}-${owner}`, bmId);
}

/** The connected wallet's exchange BalanceManager id, or null until one is
 * created (or before its first deposit on a fresh profile). */
export function useExchangeBalanceManager(owner: string | null, network: SuiNetwork) {
  return useQuery<string | null, Error>({
    queryKey: ["exchange-bm", owner],
    enabled: owner !== null && Boolean(EXCHANGE_PACKAGE_ID),
    refetchInterval: 10_000,
    queryFn: () => (owner ? findExchangeBalanceManager(network, owner) : null),
  });
}
