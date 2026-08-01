import { Navigate, Route, Routes } from "react-router-dom";
import { Header } from "./components/Header";
import { Composer } from "./screens/Composer";
import { Dashboard } from "./screens/Dashboard";
import { TradingVaults } from "./screens/TradingVaults";
import { TradingVaultDetailScreen } from "./screens/TradingVaultDetail";
import { Activity } from "./screens/Activity";
import { Bridge } from "./screens/Bridge";
import { Admin } from "./screens/Admin";
import { Debug } from "./screens/Debug";
import { Faucet } from "./screens/Faucet";

export function App() {
  return (
    // Header lives here (not per-screen) so it stays mounted across route
    // changes — preserving its wave animation and the sliding nav pill's
    // from/to position instead of resetting on every navigation.
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <Header />
      <Routes>
        <Route path="/" element={<Navigate to="/earn" replace />} />
        {/* key= forces a fresh Composer (and its useComposerState) when toggling views */}
        <Route path="/earn" element={<Composer key="writer" initialView="writer" />} />
        <Route path="/buy" element={<Composer key="trader" initialView="trader" />} />
        {/* The covered-call vault product is deprecated (SO-332): /vault is
            unrouted and screens/Vault.tsx is no longer mounted. Send the old
            path to the curated vaults so bookmarks don't dead-end. */}
        <Route path="/vault" element={<Navigate to="/vaults" replace />} />
        {/* Curated trading vaults (SO-288). */}
        <Route path="/vaults" element={<TradingVaults />} />
        <Route path="/vaults/:vaultId" element={<TradingVaultDetailScreen />} />
        <Route path="/dashboard" element={<Dashboard />} />
        <Route path="/activity" element={<Activity />} />
        <Route path="/bridge" element={<Bridge />} />
        {/* Admin self-gates on AdminCap and redirects non-admins to /earn. */}
        <Route path="/admin" element={<Admin />} />
        {/* Faucet is testnet-only; on other envs it renders a "testnet only" notice. */}
        <Route path="/faucet" element={<Faucet />} />
        {/* Unlisted internals page: reachable at /debug, not linked in nav (SO-107). */}
        <Route path="/debug" element={<Debug />} />
        <Route path="*" element={<Navigate to="/earn" replace />} />
      </Routes>
    </div>
  );
}
