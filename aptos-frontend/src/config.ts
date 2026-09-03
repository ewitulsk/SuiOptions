// Central runtime config. Everything environment-driven so the same build
// runs against any backend: Vercel env vars per project, never baked in.
export const API_URL =
  import.meta.env.VITE_NFT_API_URL ?? "http://127.0.0.1:8091";

export const APTOS_NETWORK =
  import.meta.env.VITE_APTOS_NETWORK ?? "mainnet";

export const OUR_VENUE_ADDRESS =
  import.meta.env.VITE_OUR_VENUE_ADDRESS ?? "";

export const ROUTER_PACKAGE =
  import.meta.env.VITE_ROUTER_PACKAGE ?? "";

export const ROUTER_CONFIG =
  import.meta.env.VITE_ROUTER_CONFIG ?? "";

// Venue ids mirror router::router (1..7).
export const VENUES = [
  { id: 1, slug: "wapal", name: "Wapal" },
  { id: 2, slug: "rarible", name: "Rarible" },
  { id: 3, slug: "topaz-v2", name: "Topaz" },
  { id: 4, slug: "bluemove-v2", name: "Bluemove" },
  { id: 5, slug: "tradeport-v2", name: "Tradeport v2" },
  { id: 6, slug: "tradeport", name: "Tradeport v1" },
  { id: 7, slug: "okx", name: "OKX" },
] as const;

export function venueName(slug: string): string {
  return VENUES.find((v) => v.slug === slug)?.name ?? slug;
}

export function octasToApt(octas: number | string): string {
  return (Number(octas) / 1e8).toFixed(4);
}
