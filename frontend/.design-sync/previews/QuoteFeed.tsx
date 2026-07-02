import { QuoteFeed } from "tideline-frontend";

// Live RFQ feed from the market makers. Trader view ranks asks low→high;
// writer view ranks bids high→low. The first row is highlighted as best.

const now = Date.now();
const quotes = [
  { id: "q1", name: "Aftermath MM", addr: "0x9f3a…21bc", fill: 99, revertRate: 0.4, latency: 38, premium: 3640, ttl: 12, arrivedAt: now },
  { id: "q2", name: "Cetus Desk", addr: "0x1c08…7de2", fill: 97, revertRate: 0.9, latency: 52, premium: 3685, ttl: 9, arrivedAt: now },
  { id: "q3", name: "Kriya Quotes", addr: "0xab8d…0f41", fill: 95, revertRate: 1.6, latency: 71, premium: 3720, ttl: 7, arrivedAt: now },
  { id: "q4", name: "Bluefin OTC", addr: "0xf2cb…9a55", fill: 92, revertRate: 2.3, latency: 96, premium: 3805, ttl: 5, arrivedAt: now },
];

export const TraderAsks = () => (
  <QuoteFeed quotes={quotes} view="trader" docked />
);

export const WriterBids = () => (
  <QuoteFeed quotes={quotes} view="writer" docked />
);

export const Empty = () => (
  <QuoteFeed quotes={[]} view="trader" docked />
);
