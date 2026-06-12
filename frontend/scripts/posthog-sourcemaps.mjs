// Post-build step: upload source maps to PostHog error tracking, then strip
// .map files from dist so they're never deployed. Upload only happens when
// POSTHOG_CLI_TOKEN (a phx_ personal API key) is set — i.e. on Vercel builds
// once that env var is configured; local builds just strip the maps.
import { execSync } from "node:child_process";
import { readdirSync, rmSync } from "node:fs";
import { join } from "node:path";

const dist = new URL("../dist", import.meta.url).pathname;

if (process.env.POSTHOG_CLI_TOKEN) {
  process.env.POSTHOG_CLI_ENV_ID ??= "466886"; // PostHog project id
  const run = (cmd) => execSync(cmd, { stdio: "inherit" });
  run(`posthog-cli sourcemap inject --directory ${dist}`);
  run(`posthog-cli sourcemap upload --directory ${dist}`);
  console.log("posthog-sourcemaps: injected + uploaded");
} else {
  console.log("posthog-sourcemaps: POSTHOG_CLI_TOKEN not set, skipping upload");
}

let stripped = 0;
for (const f of readdirSync(dist, { recursive: true })) {
  if (String(f).endsWith(".map")) {
    rmSync(join(dist, String(f)));
    stripped++;
  }
}
console.log(`posthog-sourcemaps: stripped ${stripped} .map file(s) from dist`);
