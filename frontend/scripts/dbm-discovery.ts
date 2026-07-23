// Manual check of the SO-299 DeepBook-Margin equity-leg discovery
// (src/tx/dbm.ts) against live staging (Sui testnet): walks the registry
// from a DbmOracle-pinned vault and prints every resolved id.
//
//   node scripts/dbm-discovery.ts [vaultId]
//
// Node 24 runs the .ts imports via native type stripping; src/config.ts
// can't load outside vite (import.meta.env), so the pinned testnet ids are
// repeated here — keep them in lockstep with config.ts.

import { SuiGrpcClient } from "@mysten/sui/grpc";
import { resolveDbmLeg } from "../src/tx/dbm.ts";

const VAULT_ID =
  process.argv[2] ?? "0x70bb15b0046b8f6fd736b2cf58b178d84b8c2418925bbdd81caa5e74098d9a6d";
const TOKEN_INFO = "https://sui-options.com/staging/token-info";

// Mirrors config.ts DBM_MARGIN_REGISTRY_IDS / DBM_ORIGINAL_PACKAGE_IDS /
// PYTH_PRICE_INFO_TABLE_IDS and appraisal.ts PYTH_HANDLES (testnet).
const IDS = {
  marginRegistryId: "0x48d7640dfae2c6e9ceeada197a7a1643984b5a24c55a0c6c023dac77e0339f75",
  originalPkg: "0xb8620c24c9ea1a4a41e79613d2b3d1d93648d1bb6f6b789a7c8f261c94110e4b",
  pythPriceInfoTableId: "0xcb858b77d8068c6c8c0d8a4ddfba95053268e4a31f8ecc49adccc4ec1570d3a7",
  pythPkg: "0xabf837e98c26087cba0883c0a7a28326b1fa3c5e1e2c5abdb486f9e8f594c837",
};

function fields(v: unknown): Record<string, unknown> {
  const r = v as Record<string, unknown> | null;
  return ((r?.fields ?? r) as Record<string, unknown>) ?? {};
}

async function main() {
  const info = (await (await fetch(`${TOKEN_INFO}/package-info`)).json()) as {
    dbmOracle?: { packageId: string } | null;
  };
  const oraclePkg = info.dbmOracle?.packageId;
  console.log("token-info dbmOracle package:", oraclePkg ?? "(not served)");

  const client = new SuiGrpcClient({
    network: "testnet",
    baseUrl: "https://fullnode.testnet.sui.io:443",
  });

  const { object: vault } = await client.core.getObject({
    objectId: VAULT_ID,
    include: { json: true },
  });
  const extRaw = fields(vault.json).external;
  const ext = fields(Array.isArray(extRaw) ? extRaw[0] : extRaw);
  const account = ext.account as string;
  const witness = ext.equity_oracle;
  console.log("vault:            ", VAULT_ID);
  console.log("external account: ", account);
  console.log("pinned witness:   ", typeof witness === "string" ? witness : fields(witness).name);

  const leg = await resolveDbmLeg(
    client,
    { oraclePkg: oraclePkg ?? "0x0", ...IDS },
    VAULT_ID,
    account,
  );
  console.log("manager:          ", leg.managerId);
  console.log("deepbook pool:    ", leg.poolId);
  console.log("base type:        ", leg.baseType);
  console.log("quote type:       ", leg.quoteType);
  console.log("base margin pool: ", leg.baseMarginPoolId);
  console.log("quote margin pool:", leg.quoteMarginPoolId);
  console.log("debt side:        ", leg.debt ? `${leg.debt.asset} (${leg.debt.marginPoolId})` : "none (record_no_debt)");
  for (const [t, feed] of Object.entries(leg.feedIdByType)) {
    console.log(`feed ${t}: ${feed} → PriceInfoObject ${leg.priceInfoByFeed[feed]}`);
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
