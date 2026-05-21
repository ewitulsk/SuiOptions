import { useEffect, useState } from "react";
import { Composer } from "./screens/Composer";
import { Dashboard } from "./screens/Dashboard";
import { Activity } from "./screens/Activity";
import type { View } from "./types";

type Screen = "composer" | "dashboard" | "activity";

// Simple hash-based routing — easy to swap for a router later.
function parseHash(): { screen: Screen; view: View } {
  const h = (window.location.hash || "").replace(/^#/, "");
  if (h === "dashboard") return { screen: "dashboard", view: "writer" };
  if (h === "activity") return { screen: "activity", view: "writer" };
  if (h === "buy") return { screen: "composer", view: "trader" };
  return { screen: "composer", view: "writer" };
}

function hashFor(screen: Screen, view: View): string {
  if (screen === "dashboard") return "dashboard";
  if (screen === "activity") return "activity";
  return view === "trader" ? "buy" : "earn";
}

export function App() {
  const [{ screen, view }, setRoute] = useState(parseHash);

  useEffect(() => {
    const sync = () => setRoute(parseHash());
    window.addEventListener("hashchange", sync);
    return () => window.removeEventListener("hashchange", sync);
  }, []);

  const navigate = (target: string) => {
    let next: { screen: Screen; view: View };
    if (target === "dashboard") next = { screen: "dashboard", view };
    else if (target === "activity") next = { screen: "activity", view };
    else if (target === "composer:writer") next = { screen: "composer", view: "writer" };
    else if (target === "composer:trader") next = { screen: "composer", view: "trader" };
    else next = { screen: "composer", view };
    setRoute(next);
    window.location.hash = hashFor(next.screen, next.view);
  };

  if (screen === "dashboard") return <Dashboard onNavigate={navigate} />;
  if (screen === "activity") return <Activity onNavigate={navigate} />;
  // Re-key on view so the composer resets its internal state when toggling.
  return <Composer key={view} initialView={view} onNavigate={navigate} />;
}
