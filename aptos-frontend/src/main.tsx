import React from "react";
import ReactDOM from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { AptosWalletAdapterProvider } from "@aptos-labs/wallet-adapter-react";
import { Network } from "@aptos-labs/ts-sdk";
import App from "./App";
import { APTOS_NETWORK } from "./config";
import "./index.css";

const queryClient = new QueryClient({
  defaultOptions: { queries: { refetchOnWindowFocus: false, retry: 1 } },
});

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <AptosWalletAdapterProvider
        autoConnect
        dappConfig={{
          network:
            APTOS_NETWORK === "mainnet" ? Network.MAINNET : Network.TESTNET,
        }}
        onError={(e) => console.error("wallet:", e)}
      >
        <BrowserRouter>
          <App />
        </BrowserRouter>
      </AptosWalletAdapterProvider>
    </QueryClientProvider>
  </React.StrictMode>,
);
