import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  server: { port: 5173, host: true },
  // "hidden": emit .map files (for PostHog error tracking upload) without a
  // sourceMappingURL reference in the served JS. Maps are stripped from dist
  // after the build by scripts/posthog-sourcemaps.mjs.
  build: { sourcemap: "hidden" },
});
