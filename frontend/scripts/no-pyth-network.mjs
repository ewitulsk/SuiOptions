// Build gate (SO-356): the frontend must NEVER talk to an oracle network
// directly — all prices and signed payloads come from oracle-service.
// Fails the build on any `pyth.network` reference under src/.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

const ROOT = new URL("../src", import.meta.url).pathname;
const offenders = [];

function walk(dir) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) walk(p);
    else if (/\.(ts|tsx|js|jsx|mjs)$/.test(name)) {
      const src = readFileSync(p, "utf8");
      let line = 0;
      for (const l of src.split("\n")) {
        line++;
        if (l.includes("pyth.network")) offenders.push(`${p}:${line}`);
      }
    }
  }
}

walk(ROOT);
if (offenders.length > 0) {
  console.error(
    "pyth.network reference(s) in frontend/src — the frontend must only consume oracle-service (SO-356):",
  );
  for (const o of offenders) console.error(`  ${o}`);
  process.exit(1);
}
