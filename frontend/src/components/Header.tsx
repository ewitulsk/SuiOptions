import { useEffect, useRef, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import {
  ConnectModal,
  useAccounts,
  useCurrentAccount,
  useCurrentWallet,
  useDisconnectWallet,
  useSwitchAccount,
} from "@mysten/dapp-kit";
import { useThemeMode, toggleMode } from "../theme";
import {
  useSponsorEnabled,
  useSponsorHealth,
  setSponsorEnabled,
} from "../state/sponsor";
import { useAdminCap, useOrgCaps } from "../api/useAdminCap";
import { ENV } from "../config";

function shortAddress(addr: string): string {
  if (addr.length <= 12) return addr;
  return `${addr.slice(0, 6)}…${addr.slice(-4)}`;
}

// Radix Dialog (used internally by ConnectModal) needs a real, rendered trigger
// element to attach handlers + refs to. Visually hiding it (vs `display:none`)
// keeps the modal able to open via the controlled `open` prop.
const hiddenTriggerStyle: React.CSSProperties = {
  position: "absolute",
  width: 1,
  height: 1,
  padding: 0,
  margin: -1,
  overflow: "hidden",
  clip: "rect(0,0,0,0)",
  border: 0,
  pointerEvents: "none",
};

function WalletMenu() {
  const account = useCurrentAccount();
  const accounts = useAccounts();
  const { currentWallet } = useCurrentWallet();
  const { mutate: switchAccount } = useSwitchAccount();
  const { mutate: disconnect } = useDisconnectWallet();
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    window.addEventListener("mousedown", onClick);
    return () => window.removeEventListener("mousedown", onClick);
  }, [open]);

  if (!account) return null;

  return (
    <div className="header__wallet" ref={wrapRef}>
      <button
        className="header__connect is-connected"
        onClick={() => setOpen((o) => !o)}
        title={account.address}
      >
        {shortAddress(account.address)}
        <span className="header__connect-caret" aria-hidden>▾</span>
      </button>
      {open && (
        <div className="wallet-menu" role="menu">
          {currentWallet && (
            <div className="wallet-menu__header">
              {currentWallet.icon && (
                <img
                  className="wallet-menu__icon"
                  src={currentWallet.icon}
                  alt=""
                />
              )}
              <span className="wallet-menu__wallet-name">
                {currentWallet.name}
              </span>
            </div>
          )}

          <div className="wallet-menu__section-label">Accounts</div>
          {accounts.map((a) => {
            const isActive = a.address === account.address;
            return (
              <button
                key={a.address}
                className={
                  "wallet-menu__item" + (isActive ? " is-active" : "")
                }
                role="menuitemradio"
                aria-checked={isActive}
                onClick={() => {
                  if (!isActive) switchAccount({ account: a });
                  setOpen(false);
                }}
              >
                <span className="wallet-menu__addr">
                  {shortAddress(a.address)}
                </span>
                {a.label && (
                  <span className="wallet-menu__label">{a.label}</span>
                )}
                {isActive && (
                  <span className="wallet-menu__check" aria-hidden>
                    ✓
                  </span>
                )}
              </button>
            );
          })}

          <div className="wallet-menu__divider" />
          <button
            className="wallet-menu__item wallet-menu__item--danger"
            role="menuitem"
            onClick={() => {
              setOpen(false);
              disconnect();
            }}
          >
            Disconnect
          </button>
        </div>
      )}
    </div>
  );
}

function SponsorToggle() {
  const enabled = useSponsorEnabled();
  const health = useSponsorHealth();
  const lowBalance = health ? !health.healthy : false;
  const title = lowBalance
    ? `Gas sponsorship balance low (${health?.balanceSui.toFixed(2)} SUI) — transactions may fall back to wallet-paid`
    : enabled
      ? "Gas sponsorship on — we pay your gas"
      : "Gas sponsorship off — you pay your own gas";
  return (
    <button
      type="button"
      className={
        "header__sponsor" +
        (enabled ? " is-on" : "") +
        (lowBalance ? " is-low" : "")
      }
      onClick={() => setSponsorEnabled(!enabled)}
      role="switch"
      aria-checked={enabled}
      aria-label="Toggle gas sponsorship"
      title={title}
    >
      <span className="header__sponsor-track" aria-hidden>
        <span className="header__sponsor-thumb" />
      </span>
      <span className="header__sponsor-label">Gas{lowBalance ? " ⚠" : ""}</span>
    </button>
  );
}

function ThemeToggle() {
  const mode = useThemeMode();
  const isDark = mode === "dark";
  return (
    <button
      type="button"
      className="header__theme"
      onClick={toggleMode}
      aria-label={isDark ? "Switch to light mode" : "Switch to dark mode"}
      title={isDark ? "Switch to light mode" : "Switch to dark mode"}
    >
      {isDark ? (
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
          <circle cx="12" cy="12" r="4" />
          <path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M4.93 19.07l1.41-1.41M17.66 6.34l1.41-1.41" />
        </svg>
      ) : (
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
          <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
        </svg>
      )}
    </button>
  );
}

export function Header() {
  const navigate = useNavigate();
  const { pathname } = useLocation();
  const account = useCurrentAccount();
  const [pickerOpen, setPickerOpen] = useState(false);
  const adminCap = useAdminCap(account?.address ?? null);
  const orgCaps = useOrgCaps(account?.address ?? null);
  // The /admin console serves both the platform admin (AdminCap) and org
  // admins (any OrgCap holder).
  const isAdmin =
    (adminCap.data?.isAdmin ?? false) || (orgCaps.data?.length ?? 0) > 0;

  return (
    <header className="header">
      <div className="header__waves" aria-hidden>
        <svg viewBox="0 0 2400 84" preserveAspectRatio="none">
          <path
            className="wave-back"
            d="M0,52 C200,72 400,32 600,52 C800,72 1000,32 1200,52 C1400,72 1600,32 1800,52 C2000,72 2200,32 2400,52 L2400,84 L0,84 Z"
            fill="#C7E6FF"
            opacity="0.5"
          />
          <path
            className="wave-mid"
            d="M0,58 C200,40 400,76 600,58 C800,40 1000,76 1200,58 C1400,40 1600,76 1800,58 C2000,40 2200,76 2400,58 L2400,84 L0,84 Z"
            fill="#9CD4FF"
            opacity="0.55"
          />
          <path
            className="wave-front"
            d="M0,64 C200,80 400,48 600,64 C800,80 1000,48 1200,64 C1400,80 1600,48 1800,64 C2000,80 2200,48 2400,64 L2400,84 L0,84 Z"
            fill="#6FBEFF"
            opacity="0.65"
          />
        </svg>
      </div>
      <div className="header__brand">
        <span className="header__brand-mark"></span>
        tideline
      </div>
      <nav className="header__nav">
        <button
          className={pathname === "/earn" ? "is-active" : ""}
          onClick={() => navigate("/earn")}
        >
          Earn
        </button>
        <button
          className={pathname === "/buy" ? "is-active" : ""}
          onClick={() => navigate("/buy")}
        >
          Buy
        </button>
        <button
          className={pathname === "/dashboard" ? "is-active" : ""}
          onClick={() => navigate("/dashboard")}
        >
          Dashboard
        </button>
        <button
          className={pathname === "/activity" ? "is-active" : ""}
          onClick={() => navigate("/activity")}
        >
          Activity
        </button>
        {ENV === "testnet" && (
          <button
            className={pathname === "/faucet" ? "is-active" : ""}
            onClick={() => navigate("/faucet")}
          >
            Faucet
          </button>
        )}
        {isAdmin && (
          <button
            className={pathname === "/admin" ? "is-active" : ""}
            onClick={() => navigate("/admin")}
          >
            Admin
          </button>
        )}
        <button>Docs</button>
      </nav>
      <SponsorToggle />
      <ThemeToggle />
      {account ? (
        <WalletMenu />
      ) : (
        <button className="header__connect" onClick={() => setPickerOpen(true)}>
          Connect wallet
        </button>
      )}

      {/* Single stable ConnectModal mount, controlled by `pickerOpen`.
          Sharing one instance avoids unmount/remount across the menu's
          open/close cycle. */}
      <ConnectModal
        open={pickerOpen}
        onOpenChange={setPickerOpen}
        trigger={<button type="button" aria-hidden="true" tabIndex={-1} style={hiddenTriggerStyle} />}
      />
    </header>
  );
}
