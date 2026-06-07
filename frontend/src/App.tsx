import { Navigate, Route, Routes } from "react-router-dom";
import { Composer } from "./screens/Composer";
import { Dashboard } from "./screens/Dashboard";
import { Activity } from "./screens/Activity";
import { Admin } from "./screens/Admin";
import { Debug } from "./screens/Debug";
import { Faucet } from "./screens/Faucet";

export function App() {
  return (
    <Routes>
      <Route path="/" element={<Navigate to="/earn" replace />} />
      {/* key= forces a fresh Composer (and its useComposerState) when toggling views */}
      <Route path="/earn" element={<Composer key="writer" initialView="writer" />} />
      <Route path="/buy" element={<Composer key="trader" initialView="trader" />} />
      <Route path="/dashboard" element={<Dashboard />} />
      <Route path="/activity" element={<Activity />} />
      {/* Admin self-gates on AdminCap and redirects non-admins to /earn. */}
      <Route path="/admin" element={<Admin />} />
      {/* Faucet is testnet-only; on other envs it renders a "testnet only" notice. */}
      <Route path="/faucet" element={<Faucet />} />
      {/* Unlisted internals page: reachable at /debug, not linked in nav (SO-107). */}
      <Route path="/debug" element={<Debug />} />
      <Route path="*" element={<Navigate to="/earn" replace />} />
    </Routes>
  );
}
