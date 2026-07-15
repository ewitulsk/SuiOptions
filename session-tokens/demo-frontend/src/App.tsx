import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useSuiClient } from "@mysten/dapp-kit";
import type { SuiJsonRpcClient } from "@mysten/sui/jsonRpc";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";
import {
  createSession,
  createSessionEth,
  ethAddressToBytes,
  readAccountBalance,
  restoreSession,
  type SessionHandle,
  type SessionStatus,
} from "@yourorg/sui-siws-session";

import {
  COIN_TYPE,
  CONFIGURED,
  NETWORK,
  PACKAGE_ID,
  PER_TX_CAP,
  REGISTRY_ID,
  SPEND_CAP,
  TTL_MS,
  WITHDRAW_SELECTOR,
} from "./config";
import { connectPhantom, type PhantomAdapter } from "./phantom";
import { connectMetaMask, type MetaMaskAdapter } from "./metamask";
import {
  fundAccount,
  makeSponsorClient,
  sponsorAddress,
  strayCoins,
  sweepAccount,
} from "./sponsor";

type RootConn =
  | { scheme: "solana"; label: string; adapter: PhantomAdapter; identity: Uint8Array }
  | {
      scheme: "ethereum";
      label: string;
      adapter: MetaMaskAdapter;
      identity: Uint8Array;
      chainId: number;
    };

const MIST = 1_000_000_000n;
const toSui = (mist: bigint) => (Number(mist) / Number(MIST)).toFixed(4);
const toHex = (b: Uint8Array) =>
  "0x" + Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
const short = (s: string) => (s.length > 16 ? `${s.slice(0, 8)}…${s.slice(-6)}` : s);

export function App() {
  const client = useSuiClient() as unknown as SuiJsonRpcClient;
  const sponsor = useMemo(() => makeSponsorClient(client), [client]);

  const [root, setRoot] = useState<RootConn | null>(null);
  const [session, setSession] = useState<SessionHandle | null>(null);
  const [status, setStatus] = useState<SessionStatus | null>(null);
  const [sponsorBal, setSponsorBal] = useState<bigint>(0n);
  const [accountBal, setAccountBal] = useState<bigint>(0n);
  const [stray, setStray] = useState<{ count: number; total: bigint }>({
    count: 0,
    total: 0n,
  });
  const [recipient, setRecipient] = useState(sponsorAddress());
  const [amount, setAmount] = useState("0.05");
  const [busy, setBusy] = useState<string | null>(null);
  const [log, setLog] = useState<string[]>([]);
  const [toast, setToast] = useState<{ label: string; digest?: string } | null>(null);

  const append = useCallback((msg: string) => {
    setLog((l) => [`${new Date().toLocaleTimeString()}  ${msg}`, ...l].slice(0, 40));
  }, []);

  const toastTimer = useRef<number | undefined>(undefined);
  const notify = useCallback((label: string, digest?: string) => {
    setToast({ label, digest });
    window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToast(null), 9000);
  }, []);

  const run = useCallback(
    async (label: string, fn: () => Promise<void>) => {
      setBusy(label);
      try {
        await fn();
      } catch (e) {
        append(`✗ ${label}: ${e instanceof Error ? e.message : String(e)}`);
      } finally {
        setBusy(null);
      }
    },
    [append],
  );

  // --- balances ---
  const refreshBalances = useCallback(
    async (accountId?: string) => {
      const sp = await client.getBalance({ owner: sponsorAddress() });
      setSponsorBal(BigInt(sp.totalBalance));
      if (accountId) {
        // Funds live as a Balance<T> inside the shared Account object, not as
        // owned coins — read the object field, not getBalance(owner).
        setAccountBal(await readAccountBalance(client, accountId, COIN_TYPE));
        // Coins wallet-transferred to the id sit "at" the address, awaiting a sweep.
        setStray(await strayCoins(client, accountId));
      }
    },
    [client],
  );

  // --- try restoring a persisted (non-extractable) session on load ---
  const restoredRef = useRef(false);
  useEffect(() => {
    if (restoredRef.current || !CONFIGURED) return;
    restoredRef.current = true;
    void (async () => {
      const handle = await restoreSession({
        client,
        network: NETWORK,
        packageId: PACKAGE_ID,
        registryId: REGISTRY_ID,
        sponsor,
      });
      if (handle) {
        setSession(handle);
        append(`↻ restored ${handle.scheme} session ${short(handle.capId)} (reconnect wallet to revoke)`);
        setStatus(await handle.status());
      }
      await refreshBalances();
    })();
  }, [client, sponsor, append, refreshBalances]);

  // --- actions ---
  // Attach a freshly-connected wallet to a restored session so revoke works,
  // but only if it's the same root identity that owns the session.
  const maybeAttach = (conn: RootConn) => {
    if (session && !session.canRevoke) {
      if (toHex(conn.identity) === toHex(session.rootIdentity)) {
        session.attachRootWallet(conn.adapter);
        append("✓ root wallet attached — revoke enabled");
      } else {
        append("⚠ different account than this session's owner — revoke would fail on-chain");
      }
    }
  };

  const connectSolana = () =>
    run("connect Phantom", async () => {
      const p = await connectPhantom();
      const conn: RootConn = {
        scheme: "solana",
        label: "Solana",
        adapter: p.adapter,
        identity: p.publicKey,
      };
      setRoot(conn);
      append(`✓ connected Solana ${short(toHex(p.publicKey))}`);
      maybeAttach(conn);
    });

  const connectEthereum = () =>
    run("connect MetaMask", async () => {
      const m = await connectMetaMask();
      const conn: RootConn = {
        scheme: "ethereum",
        label: `Ethereum (chain ${m.chainId})`,
        adapter: m.adapter,
        identity: ethAddressToBytes(m.address),
        chainId: m.chainId,
      };
      setRoot(conn);
      append(`✓ connected Ethereum ${short(m.address)} (chain ${m.chainId})`);
      maybeAttach(conn);
    });

  const openSession = () =>
    run("open session", async () => {
      if (!root) throw new Error("connect a wallet first");
      const cfg = {
        client,
        network: NETWORK,
        packageId: PACKAGE_ID,
        registryId: REGISTRY_ID,
        sponsor,
        // Per-coin-type spend limits, covered by the root signature (v2).
        limits: [{ coinType: COIN_TYPE, perTx: PER_TX_CAP, total: SPEND_CAP }],
        ttlMs: TTL_MS,
        allowed: [WITHDRAW_SELECTOR],
        persist: true,
      };
      append(`→ requesting ONE ${root.label} signature…`);
      const handle =
        root.scheme === "solana"
          ? await createSession({ ...cfg, solanaWallet: root.adapter })
          : await createSessionEth({ ...cfg, ethereumWallet: root.adapter, chainId: root.chainId });
      setSession(handle);
      setStatus(await handle.status());
      append(`✓ session open — cap ${short(handle.capId)}, account ${short(handle.accountId)}`);
      notify("Session opened", handle.openDigest);
      await refreshBalances(handle.accountId);
    });

  const refreshStatus = () =>
    run("refresh status", async () => {
      if (!session) return;
      const s = await session.status();
      setStatus(s);
      await refreshBalances(session.accountId);
      const sui = s.limits[0];
      append(`✓ status — spent ${toSui(sui?.spent ?? 0n)} / remaining ${toSui(sui?.remaining ?? 0n)} SUI`);
    });

  const fund = () =>
    run("fund account", async () => {
      if (!session) throw new Error("open a session first");
      const digest = await fundAccount(client, session.accountId, 200_000_000n);
      append(`✓ funded account 0.2 SUI (${short(digest)})`);
      notify("Account funded · 0.2 SUI", digest);
      await refreshBalances(session.accountId);
    });

  const sweep = () =>
    run("sweep stray coins", async () => {
      if (!session) throw new Error("open a session first");
      append("→ sweeping coins sent directly to the Account id into funds…");
      const { count, digest } = await sweepAccount(client, session.accountId);
      if (count === 0) {
        append("nothing to sweep");
        return;
      }
      append(`✓ swept ${count} coin(s) into the Account (${short(digest ?? "")})`);
      notify(`Swept ${count} coin(s) into funds`, digest ?? undefined);
      await refreshBalances(session.accountId);
    });

  const withdraw = () =>
    run("withdraw (auto-signed)", async () => {
      if (!session) throw new Error("open a session first");
      const mist = BigInt(Math.round(parseFloat(amount) * Number(MIST)));
      append(`→ auto-signed withdraw ${amount} SUI → ${short(recipient)} (no wallet prompt)`);
      const res = await session.execute((tx, { capId, accountId }) => {
        tx.moveCall({
          target: `${PACKAGE_ID}::app_example::withdraw`,
          typeArguments: [COIN_TYPE],
          arguments: [
            tx.object(capId),
            tx.object(accountId),
            tx.object(SUI_CLOCK_OBJECT_ID),
            tx.pure.u64(mist),
            tx.pure.address(recipient),
          ],
        });
      });
      append(`✓ withdraw executed (${short(res.digest)})`);
      notify(`Withdraw · ${amount} SUI`, res.digest);
      setStatus(await session.status());
      await refreshBalances(session.accountId);
    });

  const revoke = () =>
    run("revoke", async () => {
      if (!session) throw new Error("no session");
      append("→ requesting ONE Solana signature to revoke…");
      const res = await session.revoke();
      append(`✓ revoked — generation bumped (${short(res.digest)}). All caps dead.`);
      notify("Revoked · all caps killed", res.digest);
      setStatus(await session.status());
      setSession(null);
    });

  // --- countdown ---
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);
  const remainingMs = session ? session.expiresAtMs - now : 0;

  return (
    <div className="wrap">
      <header>
        <h1>SIWS / SIWE Session Keys</h1>
        <p className="sub">
          Sign in with a Solana (SIWS) or Ethereum (SIWE / EIP-4361) wallet →
          mint a scoped, expiring on-chain SessionCap → act on Sui without
          signing every transaction.
        </p>
      </header>

      {!CONFIGURED && (
        <div className="card warn">
          <b>Not configured.</b> Publish <code>../contracts</code> and set{" "}
          <code>VITE_PACKAGE_ID</code> and <code>VITE_REGISTRY_ID</code> (see{" "}
          <code>src/config.ts</code>).
        </div>
      )}

      <div className="grid">
        <section className="card">
          <h2>1 · Sponsor (gas payer)</h2>
          <p className="muted">
            Demo-only in-browser relayer. Pays gas + seeds the Account; cannot
            move user funds. Fund it from the {NETWORK} faucet.
          </p>
          <Row k="address" v={sponsorAddress()} mono copy />
          <Row k="balance" v={`${toSui(sponsorBal)} SUI`} />
          <button onClick={() => run("refresh balances", () => refreshBalances(session?.accountId))}>
            Refresh balance
          </button>
        </section>

        <section className="card">
          <h2>2 · Root identity</h2>
          {root ? (
            <>
              <Row k="scheme" v={root.label} />
              <Row k={root.scheme === "solana" ? "solana pubkey" : "eth address"} v={toHex(root.identity)} mono />
            </>
          ) : (
            <p className="muted">
              Your Solana or Ethereum wallet is the root of identity (used only
              to sign in, renew, and revoke).
            </p>
          )}
          <div className="btnrow">
            <button disabled={root?.scheme === "solana" || !!busy} onClick={connectSolana}>
              {root?.scheme === "solana" ? "Phantom connected" : "Connect Phantom"}
            </button>
            <button disabled={root?.scheme === "ethereum" || !!busy} onClick={connectEthereum}>
              {root?.scheme === "ethereum" ? "MetaMask connected" : "Connect MetaMask"}
            </button>
          </div>
        </section>
      </div>

      <section className="card">
        <h2>3 · Session</h2>
        {session ? (
          <>
            <Row k="session key (Sui)" v={session.sessionKey} mono />
            <Row k="cap id" v={session.capId} mono />
            <Row k="account id" v={session.accountId} mono />
            <Row
              k="expires in"
              v={remainingMs > 0 ? `${Math.floor(remainingMs / 1000)}s` : "expired"}
            />
            <Row k="account balance" v={`${toSui(accountBal)} SUI`} />
            <Row
              k="stray at object id"
              v={
                stray.count > 0
                  ? `${toSui(stray.total)} SUI · ${stray.count} coin(s) — sweep to claim`
                  : "none"
              }
            />
            {status && (
              <>
                <Row
                  k="spent / cap"
                  v={`${toSui(status.limits[0]?.spent ?? 0n)} / ${toSui(SPEND_CAP)} SUI`}
                />
                <Row k="remaining" v={`${toSui(status.limits[0]?.remaining ?? 0n)} SUI`} />
                <Row
                  k="generation"
                  v={`cap ${status.generation} · account ${status.accountGeneration}`}
                />
                <Row k="active" v={status.active ? "yes" : "no (expired/revoked)"} />
              </>
            )}
            <div className="btnrow">
              <button disabled={!!busy} onClick={fund}>
                Fund Account 0.2 SUI
              </button>
              <button disabled={!!busy || stray.count === 0} onClick={sweep}>
                Sweep stray SUI{stray.count > 0 ? ` (${stray.count})` : ""}
              </button>
              <button disabled={!!busy} onClick={refreshStatus}>
                Refresh status
              </button>
              <button className="danger" disabled={!!busy} onClick={revoke}>
                Revoke all
              </button>
            </div>
          </>
        ) : (
          <button disabled={!root || !!busy || !CONFIGURED} onClick={openSession}>
            Open session (1 signature)
          </button>
        )}
      </section>

      {session && (
        <section className="card">
          <h2>4 · Auto-signed action</h2>
          <p className="muted">
            Within the cap's per-tx ({toSui(PER_TX_CAP)}) and total ({toSui(SPEND_CAP)}) limits
            — no wallet prompt, gas paid by the sponsor.
          </p>
          <label>
            recipient
            <input value={recipient} onChange={(e) => setRecipient(e.target.value)} />
          </label>
          <label>
            amount (SUI)
            <input value={amount} onChange={(e) => setAmount(e.target.value)} />
          </label>
          <button disabled={!!busy} onClick={withdraw}>
            Withdraw (auto-signed)
          </button>
          <p className="muted small">
            account balance: {toSui(accountBal)} SUI · cap budget remaining:{" "}
            {status ? toSui(status.limits[0]?.remaining ?? 0n) : "—"} SUI
          </p>
        </section>
      )}

      <section className="card">
        <h2>Activity</h2>
        <pre className="log">{log.join("\n") || "—"}</pre>
        {busy && <p className="muted">⏳ {busy}…</p>}
      </section>

      {toast && (
        <Toast
          label={toast.label}
          digest={toast.digest}
          network={NETWORK}
          onClose={() => setToast(null)}
        />
      )}
    </div>
  );
}

function Toast({
  label,
  digest,
  network,
  onClose,
}: {
  label: string;
  digest?: string;
  network: string;
  onClose: () => void;
}) {
  return (
    <div className="toast" role="status">
      <button className="toast-close" onClick={onClose} aria-label="dismiss">
        ×
      </button>
      <div className="toast-label">✓ {label}</div>
      {digest ? (
        <a
          className="toast-link"
          href={`https://suiscan.xyz/${network}/tx/${digest}`}
          target="_blank"
          rel="noreferrer"
        >
          {short(digest)} ↗
        </a>
      ) : (
        <span className="toast-sub">submitted</span>
      )}
    </div>
  );
}

function Row({
  k,
  v,
  mono,
  copy,
}: {
  k: string;
  v: string;
  mono?: boolean;
  copy?: boolean;
}) {
  return (
    <div className="row">
      <span className="rk">{k}</span>
      <span className={mono ? "rv mono" : "rv"}>
        {v}
        {copy && (
          <button className="copy" onClick={() => navigator.clipboard.writeText(v)}>
            copy
          </button>
        )}
      </span>
    </div>
  );
}
