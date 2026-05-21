import { useState } from "react";
import { ConnectModal, useCurrentAccount, useDisconnectWallet } from "@mysten/dapp-kit";
import type { View } from "../types";

export type Screen = "composer" | "dashboard" | "activity";

type Props = {
  screen: Screen;
  view?: View;
  setView?: (v: View) => void;
  onNavigate: (target: string) => void;
};

function shortAddress(addr: string): string {
  if (addr.length <= 12) return addr;
  return `${addr.slice(0, 6)}…${addr.slice(-4)}`;
}

export function Header({ screen, view, setView, onNavigate }: Props) {
  const earnActive = screen === "composer" && view === "writer";
  const buyActive = screen === "composer" && view === "trader";
  const account = useCurrentAccount();
  const { mutate: disconnect } = useDisconnectWallet();
  const [pickerOpen, setPickerOpen] = useState(false);

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
      {account ? (
        <button
          className="header__connect is-connected"
          onClick={() => disconnect()}
          title={`${account.address} — click to disconnect`}
        >
          {shortAddress(account.address)}
        </button>
      ) : (
        <ConnectModal
          open={pickerOpen}
          onOpenChange={setPickerOpen}
          trigger={
            <button className="header__connect" onClick={() => setPickerOpen(true)}>
              Connect wallet
            </button>
          }
        />
      )}
    </header>
  );
}
