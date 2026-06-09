import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  server: { port: 5174, host: true },
  // The SDK is a `file:` dependency; dedupe so it shares ONE copy of
  // @mysten/* and react with the app (avoids dual-package instanceof bugs).
  resolve: {
    dedupe: ["@mysten/sui", "@mysten/dapp-kit", "@mysten/bcs", "react", "react-dom"],
  },
});
