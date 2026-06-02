import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  // `src/config.ts` imports rust-backend/deployments.json, which lives above
  // the frontend root; allow the dev server to read it.
  server: { port: 5173, host: true, fs: { allow: [".."] } },
});
