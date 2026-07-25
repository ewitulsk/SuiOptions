import { posthog } from "./lib/posthog";
import React from "react";
import ReactDOM from "react-dom/client";
import { PostHogErrorBoundary, PostHogProvider } from "posthog-js/react";
import { QueryCache, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { SuiClientProvider, WalletProvider, createNetworkConfig } from "@mysten/dapp-kit";
import { getJsonRpcFullnodeUrl } from "@mysten/sui/jsonRpc";
import { BrowserRouter } from "react-router-dom";
import "@mysten/dapp-kit/dist/index.css";
import { App } from "./App";
import { ENV, TOKEN_INFO_URL, initConfig } from "./config";
import { initSponsorHealth } from "./state/sponsor";
import "./theme";
import "./styles/aqua.css";
import "./styles/global.css";

// dapp-kit's provider still wants a JSON-RPC client per network, but after the
// JSON-RPC migration (docs/sui-json-rpc-migration.md) nothing routes reads or
// writes through it — chain access goes through the gRPC/GraphQL clients in
// src/lib/suiGrpc.ts. This map now only names the selectable networks and
// backs dapp-kit's wallet plumbing.
const { networkConfig } = createNetworkConfig({
  testnet: { network: "testnet", url: getJsonRpcFullnodeUrl("testnet") },
  mainnet: { network: "mainnet", url: getJsonRpcFullnodeUrl("mainnet") },
  devnet: { network: "devnet", url: getJsonRpcFullnodeUrl("devnet") },
});

const queryClient = new QueryClient({
  queryCache: new QueryCache({
    // Mutation (tx) errors are captured at their call sites; failed reads
    // (RPC/API) would otherwise vanish into per-component error state.
    onError: (error, query) =>
      posthog.captureException(error, {
        source: "query",
        query_key: JSON.stringify(query.queryKey),
      }),
  }),
});

const crashFallback = (
  <div
    style={{
      maxWidth: 560,
      margin: "15vh auto",
      padding: "0 24px",
      fontFamily: "ui-sans-serif,system-ui,sans-serif",
      color: "#cdd6e0",
    }}
  >
    <h1 style={{ fontSize: 18, margin: "0 0 8px" }}>Something went wrong</h1>
    <p style={{ fontSize: 14, lineHeight: 1.5, color: "#8b97a6" }}>
      The error has been reported. Reload the page to continue.
    </p>
  </div>
);

function renderApp() {
  ReactDOM.createRoot(document.getElementById("root")!).render(
    <React.StrictMode>
      <PostHogProvider client={posthog}>
        <PostHogErrorBoundary fallback={crashFallback}>
          <QueryClientProvider client={queryClient}>
            <SuiClientProvider networks={networkConfig} defaultNetwork={ENV}>
              {/* slushWallet registers the extension-free Slush web wallet, so
                  mobile browsers (no extension) still have something to connect
                  to instead of an empty wallet list. */}
              <WalletProvider autoConnect slushWallet={{ name: "Pismo Protocol" }}>
                <BrowserRouter>
                  <App />
                </BrowserRouter>
              </WalletProvider>
            </SuiClientProvider>
          </QueryClientProvider>
        </PostHogErrorBoundary>
      </PostHogProvider>
    </React.StrictMode>,
  );
}

function renderBootError(err: unknown) {
  // The most user-visible outage mode: token-info down means no app at all.
  posthog.captureException(err, { source: "boot", token_info_url: TOKEN_INFO_URL });
  const root = document.getElementById("root")!;
  root.innerHTML = `
    <div style="max-width:560px;margin:15vh auto;padding:0 24px;font-family:ui-sans-serif,system-ui,sans-serif;color:#cdd6e0;">
      <h1 style="font-size:18px;margin:0 0 8px;">Can't reach the token-info service</h1>
      <p style="font-size:14px;line-height:1.5;color:#8b97a6;margin:0 0 12px;">
        The app loads its on-chain ids and supported-token catalog from
        <code>${TOKEN_INFO_URL}</code>. It didn't respond, so the app can't start.
      </p>
      <pre style="font-size:12px;color:#6b7785;white-space:pre-wrap;">${String(err)}</pre>
    </div>`;
}

// Hard cutover: fetch config from token-info before rendering. No
// deployments.json fallback — if token-info is down, the app shows a boot error.
initConfig()
  .then(renderApp)
  .catch(renderBootError);

// Non-blocking: seed the sponsor toggle's default from the gas station's
// balance. The gas station being down must NOT fail boot (unlike token-info).
void initSponsorHealth();
