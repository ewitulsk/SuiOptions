import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { SuiClientProvider, createNetworkConfig } from "@mysten/dapp-kit";
import { getJsonRpcFullnodeUrl } from "@mysten/sui/jsonRpc";
import { App } from "./App";
import { NETWORK } from "./config";
import "./styles.css";

const { networkConfig } = createNetworkConfig({
  testnet: { network: "testnet", url: getJsonRpcFullnodeUrl("testnet") },
  mainnet: { network: "mainnet", url: getJsonRpcFullnodeUrl("mainnet") },
  devnet: { network: "devnet", url: getJsonRpcFullnodeUrl("devnet") },
  localnet: { network: "localnet", url: getJsonRpcFullnodeUrl("localnet") },
});

const queryClient = new QueryClient();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <SuiClientProvider networks={networkConfig} defaultNetwork={NETWORK}>
        <App />
      </SuiClientProvider>
    </QueryClientProvider>
  </React.StrictMode>,
);
