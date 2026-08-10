import { ConnectButton } from "@mysten/dapp-kit";
import { NavLink, Navigate, Route, Routes } from "react-router-dom";

import { ENV } from "./config";
import { SwapScreen } from "./screens/SwapScreen";
import { CreateMarketScreen } from "./screens/CreateMarketScreen";

export function App() {
  return (
    <div className="app">
      <header className="header">
        <div className="header-left">
          <span className="brand">Exchange</span>
          <span className="net-badge">{ENV}</span>
          <nav className="nav">
            <NavLink to="/swap" className={({ isActive }) => (isActive ? "active" : "")}>
              Swap
            </NavLink>
            <NavLink to="/create-market" className={({ isActive }) => (isActive ? "active" : "")}>
              Create Market
            </NavLink>
          </nav>
        </div>
        <ConnectButton />
      </header>
      <main className="main">
        <Routes>
          <Route path="/swap" element={<SwapScreen />} />
          <Route path="/create-market" element={<CreateMarketScreen />} />
          <Route path="*" element={<Navigate to="/swap" replace />} />
        </Routes>
      </main>
    </div>
  );
}
