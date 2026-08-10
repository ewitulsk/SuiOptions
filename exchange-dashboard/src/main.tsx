import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { SuiClientProvider, WalletProvider, createNetworkConfig } from "@mysten/dapp-kit";
import { getJsonRpcFullnodeUrl } from "@mysten/sui/jsonRpc";
import { BrowserRouter } from "react-router-dom";
import "@mysten/dapp-kit/dist/index.css";
import { App } from "./App";
import { ENV } from "./config";
import "./styles.css";

// Same posture as frontend/src/main.tsx: this JSON-RPC map only names the
// selectable networks and backs dapp-kit's wallet plumbing — chain access
// goes through src/lib/suiGrpc.ts.
const { networkConfig } = createNetworkConfig({
  testnet: { network: "testnet", url: getJsonRpcFullnodeUrl("testnet") },
  mainnet: { network: "mainnet", url: getJsonRpcFullnodeUrl("mainnet") },
  devnet: { network: "devnet", url: getJsonRpcFullnodeUrl("devnet") },
});

const queryClient = new QueryClient();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <SuiClientProvider networks={networkConfig} defaultNetwork={ENV}>
        <WalletProvider autoConnect slushWallet={{ name: "Exchange Dashboard" }}>
          <BrowserRouter>
            <App />
          </BrowserRouter>
        </WalletProvider>
      </SuiClientProvider>
    </QueryClientProvider>
  </React.StrictMode>,
);
