import { Navigate, Route, Routes } from "react-router-dom";
import { Composer } from "./screens/Composer";
import { Dashboard } from "./screens/Dashboard";
import { Activity } from "./screens/Activity";

export function App() {
  return (
    <Routes>
      <Route path="/" element={<Navigate to="/earn" replace />} />
      {/* key= forces a fresh Composer (and its useComposerState) when toggling views */}
      <Route path="/earn" element={<Composer key="writer" initialView="writer" />} />
      <Route path="/buy" element={<Composer key="trader" initialView="trader" />} />
      <Route path="/dashboard" element={<Dashboard />} />
      <Route path="/activity" element={<Activity />} />
      <Route path="*" element={<Navigate to="/earn" replace />} />
    </Routes>
  );
}
