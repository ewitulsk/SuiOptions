import type { View } from "../types";

export type Screen = "composer" | "dashboard" | "activity";

type Props = {
  connected: boolean;
  onConnect: () => void;
  screen: Screen;
  view?: View;
  setView?: (v: View) => void;
  onNavigate: (target: string) => void;
};

export function Header({ connected, onConnect, screen, view, setView, onNavigate }: Props) {
  const earnActive = screen === "composer" && view === "writer";
  const buyActive = screen === "composer" && view === "trader";
  return (
    <header className="header">
      <div className="header__brand">
        <span className="header__brand-mark"></span>
        tideline
      </div>
      <nav className="header__nav">
        <button
          className={earnActive ? "is-active" : ""}
          onClick={() => (setView ? setView("writer") : onNavigate("composer:writer"))}
        >
          Earn
        </button>
        <button
          className={buyActive ? "is-active" : ""}
          onClick={() => (setView ? setView("trader") : onNavigate("composer:trader"))}
        >
          Buy
        </button>
        <button
          className={screen === "dashboard" ? "is-active" : ""}
          onClick={() => onNavigate("dashboard")}
        >
          Dashboard
        </button>
        <button
          className={screen === "activity" ? "is-active" : ""}
          onClick={() => onNavigate("activity")}
        >
          Activity
        </button>
        <button>Docs</button>
      </nav>
      <span className="header__status">
        <span className="dot"></span>WSS live
      </span>
      <button
        className={"header__connect" + (connected ? " is-connected" : "")}
        onClick={onConnect}
      >
        {connected ? "0x9f3a…42b1" : "Connect wallet"}
      </button>
    </header>
  );
}
