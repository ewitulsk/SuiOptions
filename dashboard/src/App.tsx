import { NavLink, Navigate, Route, Routes } from "react-router-dom";

import { setEnv, useDashEnv, type DashEnv } from "./config";
import { toggleMode, useThemeMode } from "./theme";
import { Exposures } from "./screens/Exposures";
import { History } from "./screens/History";
import { Overview } from "./screens/Overview";
import { Positions } from "./screens/Positions";
import { VaultScreen } from "./screens/VaultScreen";
import { Venues } from "./screens/Venues";

const NAV = [
  { to: "/overview", label: "Overview" },
  { to: "/exposures", label: "Exposures" },
  { to: "/positions", label: "Positions" },
  { to: "/venues", label: "Venues" },
  { to: "/vault", label: "Vault" },
  { to: "/history", label: "History" },
];

export default function App() {
  const env = useDashEnv();
  const mode = useThemeMode();
  return (
    <div className="dash-shell">
      <header className="dash-header">
        <div className="dash-header__title">
          Desk<span> Dashboard</span>
        </div>
        <nav className="dash-nav">
          {NAV.map((n) => (
            <NavLink key={n.to} to={n.to} className={({ isActive }) => (isActive ? "active" : "")}>
              {n.label}
            </NavLink>
          ))}
        </nav>
        <div className="dash-header__spacer" />
        <select
          className="dash-select"
          value={env}
          onChange={(e) => setEnv(e.target.value as DashEnv)}
          title="Environment"
        >
          <option value="staging">staging</option>
          <option value="prod">prod</option>
          <option value="local">local</option>
        </select>
        <button className="dash-btn" onClick={toggleMode} title="Toggle theme">
          {mode === "dark" ? "☾" : "☀"}
        </button>
      </header>
      <Routes>
        <Route path="/" element={<Navigate to="/overview" replace />} />
        <Route path="/overview" element={<Overview />} />
        <Route path="/exposures" element={<Exposures />} />
        <Route path="/positions" element={<Positions />} />
        <Route path="/venues" element={<Venues />} />
        <Route path="/vault" element={<VaultScreen />} />
        <Route path="/history" element={<History />} />
        <Route path="*" element={<Navigate to="/overview" replace />} />
      </Routes>
    </div>
  );
}
