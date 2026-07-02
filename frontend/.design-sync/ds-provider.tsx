// design-sync preview provider.
//
// The Tideline components assume the app's runtime context (src/main.tsx):
// react-query, dapp-kit's Sui client + wallet, a router, and the `data-theme="aqua"`
// wrapper that App.tsx puts around everything (aqua.css scopes every token under
// `[data-theme="aqua"]`, and dark-mode overrides under `html[data-mode="dark"] [data-theme="aqua"]`).
// Without this stack, hook-driven components throw ("No QueryClient", "WalletContext",
// "useNavigate outside Router") and nothing picks up aqua tokens.
//
// Wired in via cfg.provider + cfg.extraEntries so every preview card is wrapped here.
// Networked queries simply stay in their loading/empty state (retry off, no fetch in
// headless) — authored previews pass real data as props instead of relying on fetches.
import React from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { SuiClientProvider, WalletProvider, createNetworkConfig } from "@mysten/dapp-kit";
import { getJsonRpcFullnodeUrl } from "@mysten/sui/jsonRpc";
import { MemoryRouter } from "react-router-dom";

// Render previews in the app's default "dark" mode (theme.ts falls back to dark;
// aqua.css scopes its dark palette under html[data-mode="dark"]). Set at module
// load AND in a layout effect so it's deterministic regardless of whether
// theme.ts is in the bundle or which module evaluates last.
function applyDarkMode() {
  if (typeof document !== "undefined") {
    document.documentElement.setAttribute("data-mode", "dark");
  }
}
applyDarkMode();

const { networkConfig } = createNetworkConfig({
  testnet: { url: getJsonRpcFullnodeUrl("testnet") },
});

// retry:false so a failed/absent fetch resolves immediately to an error state
// rather than spinning; previews never depend on a live network.
const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false, gcTime: Infinity } },
});

export function DSProvider({ children }: { children?: React.ReactNode }) {
  React.useLayoutEffect(applyDarkMode, []);
  return (
    <QueryClientProvider client={queryClient}>
      <SuiClientProvider networks={networkConfig} defaultNetwork="testnet">
        <WalletProvider>
          <MemoryRouter>
            {/* data-theme="aqua" paints the gradient canvas + ink color. The
                explicit height/padding keeps short leaf components from
                collapsing the canvas to nothing, and `transform` establishes a
                containing block so the app's position:fixed/sticky bits (toasts,
                header, modal overlays) stay inside the preview card instead of
                escaping to the viewport. */}
            <div
              data-theme="aqua"
              style={{
                minHeight: 160,
                padding: 28,
                position: "relative",
                transform: "translateZ(0)",
                fontFamily: "Sora, -apple-system, system-ui, sans-serif",
              }}
            >
              {children}
            </div>
          </MemoryRouter>
        </WalletProvider>
      </SuiClientProvider>
    </QueryClientProvider>
  );
}
