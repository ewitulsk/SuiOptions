// Session-login UI for the nav bar (siws_session integration). Two pieces:
//
//   - <ConnectMenu>: replaces the bare "Connect wallet" button. Offers a Sui
//     wallet (dapp-kit ConnectModal) plus "Sign in with Phantom / MetaMask"
//     when the session deployment exists.
//   - <SessionMenu>: the account dropdown while a session is active — root
//     identity, expiry countdown, per-asset spend budgets, custody balances,
//     testnet funding, withdraw, re-sign-in, revoke, sign out.
//
// All session/account management lives here, per the integration spec.

import { useEffect, useMemo, useRef, useState } from "react";

import {
  ENV,
  SUPPORTED_TOKENS,
  TEST_TOKENS,
  findToken,
  type TestToken,
} from "../config";
import {
  fundFromFaucet,
  revokeSession,
  sessionLoginAvailable,
  signInWithLastEth,
  signInWithMetaMask,
  signInWithPhantom,
  signInWithWalletConnect,
  signOutSession,
  useSession,
  withdrawFromCustody,
} from "../session/store";
import { hasMetaMask, hasPhantom, hasWalletConnect } from "../session/wallets";

function hex(bytes: Uint8Array): string {
  let out = "0x";
  for (const b of bytes) out += b.toString(16).padStart(2, "0");
  return out;
}

function shortHex(s: string): string {
  if (s.length <= 12) return s;
  return `${s.slice(0, 6)}…${s.slice(-4)}`;
}

function formatAmount(raw: bigint, decimals: number): string {
  const unit = 10n ** BigInt(decimals);
  const whole = raw / unit;
  const frac = raw % unit;
  if (frac === 0n) return whole.toString();
  const fracStr = frac.toString().padStart(decimals, "0").replace(/0+$/, "");
  return `${whole}.${fracStr.slice(0, 6)}`;
}

function useOutsideClose(open: boolean, close: () => void) {
  const wrapRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) close();
    };
    window.addEventListener("mousedown", onClick);
    return () => window.removeEventListener("mousedown", onClick);
  }, [open, close]);
  return wrapRef;
}

function useCountdown(toMs: number | undefined): string {
  const [, setTick] = useState(0);
  useEffect(() => {
    const t = setInterval(() => setTick((n) => n + 1), 30_000);
    return () => clearInterval(t);
  }, []);
  if (!toMs) return "";
  const left = toMs - Date.now();
  if (left <= 0) return "expired";
  const h = Math.floor(left / 3_600_000);
  const m = Math.floor((left % 3_600_000) / 60_000);
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

/** Sign-in options shown while disconnected. */
export function ConnectMenu({ onSuiWallet }: { onSuiWallet: () => void }) {
  const session = useSession();
  const [open, setOpen] = useState(false);
  const wrapRef = useOutsideClose(open, () => setOpen(false));
  const busy = session.phase === "signing-in" || session.phase === "restoring";

  if (!sessionLoginAvailable()) {
    // No session deployment — keep the plain connect button.
    return (
      <button className="header__connect" onClick={onSuiWallet}>
        Connect wallet
      </button>
    );
  }

  const signIn = async (fn: () => Promise<void>) => {
    setOpen(false);
    try {
      await fn();
    } catch {
      // surfaced via session.error in the menu / toast below
    }
  };

  return (
    <div className="header__wallet" ref={wrapRef}>
      <button
        className="header__connect"
        onClick={() => setOpen((o) => !o)}
        disabled={busy}
      >
        {busy ? "Connecting…" : "Connect wallet"}
        <span className="header__connect-caret" aria-hidden>▾</span>
      </button>
      {open && (
        <div className="wallet-menu" role="menu">
          <button
            className="wallet-menu__item"
            role="menuitem"
            onClick={() => {
              setOpen(false);
              onSuiWallet();
            }}
          >
            Sui wallet
          </button>
          <div className="wallet-menu__divider" />
          <div className="wallet-menu__section-label">Sign in with</div>
          <button
            className="wallet-menu__item"
            role="menuitem"
            disabled={!hasPhantom()}
            title={hasPhantom() ? "Sign in once with your Solana wallet" : "Phantom not detected"}
            onClick={() => signIn(signInWithPhantom)}
          >
            Phantom (Solana)
          </button>
          <button
            className="wallet-menu__item"
            role="menuitem"
            disabled={!hasMetaMask()}
            title={hasMetaMask() ? "Sign in once with your Ethereum wallet" : "MetaMask not detected"}
            onClick={() => signIn(signInWithMetaMask)}
          >
            MetaMask (Ethereum)
          </button>
          {hasWalletConnect() && (
            <button
              className="wallet-menu__item"
              role="menuitem"
              title="Sign in once with any WalletConnect wallet"
              onClick={() => signIn(signInWithWalletConnect)}
            >
              WalletConnect (Ethereum)
            </button>
          )}
          {session.error && (
            <div className="wallet-menu__section-label" style={{ color: "var(--danger, #c33)" }}>
              {session.error}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

const FUND_WHOLE_TOKENS = 10n;

function FundRow({ token }: { token: TestToken }) {
  const [busy, setBusy] = useState(false);
  const amount = FUND_WHOLE_TOKENS * 10n ** BigInt(token.decimals);
  return (
    <button
      className="wallet-menu__item"
      disabled={busy}
      onClick={async () => {
        setBusy(true);
        try {
          await fundFromFaucet(token, amount);
        } catch {
          /* surfaced via session.error */
        } finally {
          setBusy(false);
        }
      }}
    >
      {busy ? `Minting ${token.symbol}…` : `+${FUND_WHOLE_TOKENS} ${token.symbol}`}
    </button>
  );
}

function WithdrawForm({ balances }: { balances: Record<string, bigint> }) {
  const held = useMemo(
    () =>
      SUPPORTED_TOKENS.filter((t) => {
        const raw = Object.entries(balances).find(([type]) =>
          type.toLowerCase().endsWith(`::${t.ticker.toLowerCase()}::${t.ticker.toUpperCase()}`.toLowerCase()),
        );
        return raw && raw[1] > 0n;
      }),
    [balances],
  );
  const [ticker, setTicker] = useState("");
  const [amount, setAmount] = useState("");
  const [recipient, setRecipient] = useState("");
  const [busy, setBusy] = useState(false);
  if (held.length === 0) return null;
  const token = findToken(ticker || held[0].ticker);

  const submit = async () => {
    if (!token || !amount || !recipient) return;
    const raw = BigInt(Math.round(parseFloat(amount) * 10 ** token.decimals));
    if (raw <= 0n) return;
    setBusy(true);
    try {
      await withdrawFromCustody(token.coinType, raw, recipient.trim());
      setAmount("");
      setRecipient("");
    } catch {
      /* surfaced via session.error */
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="wallet-menu__withdraw">
      <div className="wallet-menu__section-label">Withdraw to Sui address</div>
      <div style={{ display: "flex", gap: 4, padding: "0 8px 6px" }}>
        <select value={ticker || held[0].ticker} onChange={(e) => setTicker(e.target.value)}>
          {held.map((t) => (
            <option key={t.ticker} value={t.ticker}>
              {t.ticker}
            </option>
          ))}
        </select>
        <input
          style={{ width: 70 }}
          placeholder="amount"
          value={amount}
          onChange={(e) => setAmount(e.target.value)}
        />
      </div>
      <div style={{ display: "flex", gap: 4, padding: "0 8px 6px" }}>
        <input
          style={{ flex: 1 }}
          placeholder="0x recipient"
          value={recipient}
          onChange={(e) => setRecipient(e.target.value)}
        />
        <button disabled={busy || !amount || !recipient} onClick={submit}>
          {busy ? "…" : "Send"}
        </button>
      </div>
    </div>
  );
}

/** Shows the Sui session account address with a one-click copy. */
function CopyAddressRow({ address }: { address: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      className="wallet-menu__item"
      role="menuitem"
      title={`Copy session account ${address}`}
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(address);
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        } catch {
          /* clipboard unavailable */
        }
      }}
    >
      <span className="wallet-menu__addr">{shortHex(address)}</span>
      <span className="wallet-menu__label" style={{ marginLeft: "auto" }}>
        {copied ? "Copied!" : "Copy address"}
      </span>
    </button>
  );
}

/** The account dropdown while a session is active. */
export function SessionMenu() {
  const session = useSession();
  const [open, setOpen] = useState(false);
  const wrapRef = useOutsideClose(open, () => setOpen(false));
  const countdown = useCountdown(session.handle?.expiresAtMs);

  if (!session.handle) return null;
  const handle = session.handle;
  const rootLabel =
    handle.scheme === "solana"
      ? `Solana ${shortHex(hex(handle.rootIdentity))}`
      : `Ethereum ${shortHex(hex(handle.rootIdentity))}`;
  const expired = Date.now() > handle.expiresAtMs;
  const revoked =
    session.status != null && session.status.accountGeneration !== session.status.generation;

  const balanceRows = Object.entries(session.balances)
    .map(([type, raw]) => {
      const token = SUPPORTED_TOKENS.find(
        (t) => type.toLowerCase().endsWith(t.coinType.slice(t.coinType.indexOf("::")).toLowerCase()),
      );
      return { type, raw, token };
    })
    .filter((r) => r.raw > 0n);

  return (
    <div className="header__wallet" ref={wrapRef}>
      <button
        className={"header__connect is-connected" + (expired || revoked ? " is-low" : "")}
        onClick={() => setOpen((o) => !o)}
        title={`Session account ${handle.accountId}`}
      >
        {rootLabel}
        <span className="header__connect-caret" aria-hidden>▾</span>
      </button>
      {open && (
        <div className="wallet-menu" role="menu">
          <div className="wallet-menu__header">
            <span className="wallet-menu__wallet-name">{rootLabel}</span>
          </div>

          <div className="wallet-menu__section-label">Session account</div>
          <CopyAddressRow address={handle.accountId} />

          <div className="wallet-menu__section-label">
            {expired
              ? "Session expired"
              : revoked
                ? "Session revoked"
                : `Session active · ${countdown} left`}
          </div>
          {(expired || revoked) && (
            <button
              className="wallet-menu__item"
              onClick={() =>
                (handle.scheme === "solana" ? signInWithPhantom() : signInWithLastEth()).catch(
                  () => {},
                )
              }
            >
              Sign in again (same account)
            </button>
          )}

          {session.status && session.status.limits.length > 0 && (
            <>
              <div className="wallet-menu__section-label">Session budgets</div>
              {session.status.limits
                .filter((l) => l.spent > 0n)
                .map((l) => {
                  const token = SUPPORTED_TOKENS.find((t) =>
                    l.coinType.toLowerCase().endsWith(
                      t.coinType.slice(t.coinType.indexOf("::")).toLowerCase(),
                    ),
                  );
                  const dec = token?.decimals ?? 0;
                  return (
                    <div key={l.coinType} className="wallet-menu__item" style={{ cursor: "default" }}>
                      <span className="wallet-menu__addr">
                        {token?.ticker ?? shortHex(l.coinType)}
                      </span>
                      <span className="wallet-menu__label">
                        {formatAmount(l.spent, dec)} / {formatAmount(l.total, dec)} spent
                      </span>
                    </div>
                  );
                })}
            </>
          )}

          <div className="wallet-menu__section-label">Account balance</div>
          {balanceRows.length === 0 && (
            <div className="wallet-menu__item" style={{ cursor: "default" }}>
              <span className="wallet-menu__label">empty</span>
            </div>
          )}
          {balanceRows.map(({ type, raw, token }) => (
            <div key={type} className="wallet-menu__item" style={{ cursor: "default" }}>
              <span className="wallet-menu__addr">{token?.ticker ?? shortHex(type)}</span>
              <span className="wallet-menu__label">
                {formatAmount(raw, token?.decimals ?? 0)}
              </span>
            </div>
          ))}

          {ENV === "testnet" && TEST_TOKENS.length > 0 && !expired && !revoked && (
            <>
              <div className="wallet-menu__section-label">Fund (testnet faucet)</div>
              {TEST_TOKENS.map((t) => (
                <FundRow key={t.symbol} token={t} />
              ))}
            </>
          )}

          {!expired && !revoked && <WithdrawForm balances={session.balances} />}

          {session.error && (
            <div className="wallet-menu__section-label" style={{ color: "var(--danger, #c33)" }}>
              {session.error}
            </div>
          )}

          <div className="wallet-menu__divider" />
          <button
            className="wallet-menu__item wallet-menu__item--danger"
            role="menuitem"
            disabled={session.busy === "revoking"}
            title="Root-signed: kills every session key for this account immediately"
            onClick={() => revokeSession().catch(() => {})}
          >
            {session.busy === "revoking" ? "Revoking…" : "Revoke all sessions"}
          </button>
          <button
            className="wallet-menu__item wallet-menu__item--danger"
            role="menuitem"
            onClick={() => {
              setOpen(false);
              void signOutSession();
            }}
          >
            Sign out
          </button>
        </div>
      )}
    </div>
  );
}
