import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// 5174 keeps this clear of the protocol frontend on 5173, so both can run at
// once — and both dev ports are in dakota-service's and auth-service's CORS
// allow-lists.
export default defineConfig({
  plugins: [react()],
  server: { port: 5174 },
});
