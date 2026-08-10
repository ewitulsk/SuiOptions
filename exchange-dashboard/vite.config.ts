import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  // 5174: the main frontend dev server owns 5173.
  server: { port: 5174, host: true },
});
