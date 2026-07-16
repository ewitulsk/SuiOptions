// Sui data-plane clients for the JSON-RPC deprecation (SO: see
// docs/sui-json-rpc-migration.md). Sui deactivated JSON-RPC on public testnet
// fullnodes in July 2026, so all chain reads, simulations, and transaction
// execution go through gRPC-web on the public fullnodes, and event queries go
// through the hosted GraphQL RPC. dapp-kit's `SuiClientProvider` (a JSON-RPC
// client) stays mounted only for wallet plumbing — nothing routes RPC calls
// through it anymore.

import { useSuiClientContext } from "@mysten/dapp-kit";
import type { SuiClientTypes } from "@mysten/sui/client";
import { SuiGrpcClient } from "@mysten/sui/grpc";

export type SuiNetwork = "testnet" | "mainnet" | "devnet";

// gRPC-web rides the fullnodes' regular HTTPS port (CORS `*`), so the same
// URLs the JSON-RPC client used keep working for the gRPC transport.
const GRPC_URLS: Record<SuiNetwork, string> = {
  testnet: "https://fullnode.testnet.sui.io:443",
  mainnet: "https://fullnode.mainnet.sui.io:443",
  devnet: "https://fullnode.devnet.sui.io:443",
};

const GRAPHQL_URLS: Record<SuiNetwork, string> = {
  testnet: "https://graphql.testnet.sui.io/graphql",
  mainnet: "https://graphql.mainnet.sui.io/graphql",
  devnet: "https://graphql.devnet.sui.io/graphql",
};

const grpcClients = new Map<SuiNetwork, SuiGrpcClient>();

export function getGrpcClient(network: SuiNetwork): SuiGrpcClient {
  let client = grpcClients.get(network);
  if (!client) {
    client = new SuiGrpcClient({ network, baseUrl: GRPC_URLS[network] });
    grpcClients.set(network, client);
  }
  return client;
}

/** dapp-kit's currently selected network (keys of `main.tsx`'s networks map). */
export function useSuiNetwork(): SuiNetwork {
  return useSuiClientContext().network as SuiNetwork;
}

/** The gRPC client for dapp-kit's currently selected network. */
export function useSuiGrpcClient(): SuiGrpcClient {
  return getGrpcClient(useSuiNetwork());
}

/** An owned object with its Move contents rendered as JSON. */
export type OwnedObjectJson = SuiClientTypes.Object<{ json: true }>;

/**
 * Every owned object of one struct type, JSON contents included, across all
 * pages of `StateService.ListOwnedObjects`.
 */
export async function listAllOwnedObjects(
  client: SuiGrpcClient,
  owner: string,
  type: string,
): Promise<OwnedObjectJson[]> {
  const out: OwnedObjectJson[] = [];
  let cursor: string | null | undefined = undefined;
  do {
    const page: SuiClientTypes.ListOwnedObjectsResponse<{ json: true }> =
      await client.core.listOwnedObjects({ owner, type, include: { json: true }, cursor });
    out.push(...page.objects);
    cursor = page.hasNextPage ? page.cursor : undefined;
  } while (cursor);
  return out;
}

/** Every coin balance the address holds, across all pages of `ListBalances`. */
export async function listAllBalances(
  client: SuiGrpcClient,
  owner: string,
): Promise<SuiClientTypes.Balance[]> {
  const out: SuiClientTypes.Balance[] = [];
  let cursor: string | null | undefined = undefined;
  do {
    const page: SuiClientTypes.ListBalancesResponse = await client.core.listBalances({
      owner,
      cursor,
    });
    out.push(...page.balances);
    cursor = page.hasNextPage ? page.cursor : undefined;
  } while (cursor);
  return out;
}

/**
 * One Sui GraphQL RPC query. Throws on transport or GraphQL-level errors —
 * callers treat a failed read like any other failed RPC read.
 */
export async function suiGraphqlQuery<T>(
  network: SuiNetwork,
  query: string,
  variables: Record<string, unknown>,
): Promise<T> {
  const res = await fetch(GRAPHQL_URLS[network], {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ query, variables }),
  });
  if (!res.ok) throw new Error(`Sui GraphQL: HTTP ${res.status}`);
  const body = (await res.json()) as { data?: T; errors?: Array<{ message: string }> };
  if (body.errors?.length) throw new Error(`Sui GraphQL: ${body.errors[0].message}`);
  if (body.data == null) throw new Error("Sui GraphQL: empty response");
  return body.data;
}
