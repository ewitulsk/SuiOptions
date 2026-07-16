import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      // dev-mode: forward API + auth to the local backend so cookies are same-origin
      "/api": "http://localhost:8000",
      "/auth": "http://localhost:8000",
    },
  },
});
