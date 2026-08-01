import { useEffect, useLayoutEffect, useRef, useState } from "react";
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
import { useAdminCap } from "../api/useAdminCap";
import { ENV } from "../config";
import { posthog } from "../lib/posthog";

// Module scope: every screen mounts its own <Header>, so a per-mount ref would
// re-fire identify + wallet_connected on each route change.
let identifiedAddress: string | null = null;

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

  const handleDisconnect = () => {
    setOpen(false);
    posthog.capture("wallet_disconnected", {
      wallet_address: account?.address,
      wallet_name: currentWallet?.name,
    });
    posthog.reset();
    disconnect();
  };

  useEffect(() => {
    if (!open) return;
    // pointerdown covers mouse + touch; mousedown alone left the menu open
    // while scrolling on touch devices.
    const onClick = (e: PointerEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    window.addEventListener("pointerdown", onClick);
    return () => window.removeEventListener("pointerdown", onClick);
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
            onClick={handleDisconnect}
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
  const isAdmin = adminCap.data?.isAdmin ?? false;

  useEffect(() => {
    const addr = account?.address ?? null;
    if (addr && addr !== identifiedAddress) {
      posthog.identify(addr, { wallet_address: addr });
      posthog.capture("wallet_connected", { wallet_address: addr });
    }
    identifiedAddress = addr;
  }, [account?.address]);

  const navRef = useRef<HTMLElement>(null);
  const [pill, setPill] = useState({ left: 0, width: 0, ready: false });
  // Suppress the slide/fade on first paint so a refresh snaps the pill onto
  // the active tab; transitions turn on once mounted for subsequent switches.
  const [animated, setAnimated] = useState(false);

  useLayoutEffect(() => {
    const sync = () => {
      const nav = navRef.current;
      const active = nav?.querySelector<HTMLButtonElement>("button.is-active");
      if (!active) {
        setPill((p) => ({ ...p, ready: false }));
        return;
      }
      setPill({ left: active.offsetLeft, width: active.offsetWidth, ready: true });
    };
    sync();
    window.addEventListener("resize", sync);
    return () => window.removeEventListener("resize", sync);
  }, [pathname, isAdmin]);

  useEffect(() => {
    setAnimated(true);
  }, []);

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
        <img className="header__brand-mark" src="/pismo-mark.svg" width={36} height={36} alt="" />
        pismo protocol
      </div>
      <nav className="header__nav" ref={navRef}>
        <span
          className="header__nav-pill"
          aria-hidden
          style={{
            transform: `translateX(${pill.left}px)`,
            width: pill.width,
            opacity: pill.ready ? 1 : 0,
            transition: animated ? undefined : "none",
          }}
        />
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
        {/* The covered-call "Vaults" tab (/vault) went away with that product
            (SO-332); the curated vaults take over the name. */}
        <button
          className={pathname.startsWith("/vaults") ? "is-active" : ""}
          onClick={() => navigate("/vaults")}
        >
          Vaults
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
        <button
          className={pathname === "/bridge" ? "is-active" : ""}
          onClick={() => navigate("/bridge")}
        >
          Bridge
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
        <button
          className="header__github"
          onClick={() =>
            window.open(
              "https://github.com/ewitulsk/SuiOptions",
              "_blank",
              "noopener,noreferrer"
            )
          }
        >
          <svg width="15" height="15" viewBox="0 0 16 16" fill="currentColor" aria-hidden>
            <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z" />
          </svg>
          Github
        </button>
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
