// CCTP USDC bridge (Sui ↔ Solana, Circle CCTP v1).
//
// Burn-side entry points are our contracts on each chain (tx/bridge.ts PTB
// on Sui; solana/bridge.ts Anchor ix on Solana). After the burn tx lands the
// hash is registered with cctp-relay, which polls Circle's attestation API
// and auto-submits the destination-chain mint — the list below tracks each
// transfer's status and end-to-end duration live.

import { useEffect, useMemo, useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";
import { PublicKey } from "@solana/web3.js";

import { registerBridgeTransfer, type BridgeTransfer } from "../api/bridge";
import { useBridgeTransfers } from "../api/useBridgeTransfers";
import { Toast, type ToastState } from "../components/Toast";
import { CCTP_BRIDGE_PACKAGE_ID, ENV } from "../config";
import {
  connectPhantomWallet,
  deriveUsdcAta,
  hasPhantom,
  sendSolanaDepositForBurn,
} from "../solana/bridge";
import { buildSuiDepositForBurnTx } from "../tx/bridge";
import { useSubmitTransaction } from "../tx/submit";
import { posthog } from "../lib/posthog";

type Direction = "sui->solana" | "solana->sui";

const USDC_DECIMALS = 6;

function toRaw(amount: string): bigint {
  const n = Number(amount);
  if (!Number.isFinite(n) || n <= 0) return 0n;
  return BigInt(Math.round(n * 10 ** USDC_DECIMALS));
}

function fmtUsdc(baseUnits: number | null): string {
  if (baseUnits == null) return "—";
  return (baseUnits / 10 ** USDC_DECIMALS).toLocaleString("en-US", {
    maximumFractionDigits: USDC_DECIMALS,
  });
}

function fmtDuration(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  const m = Math.floor(s / 60);
  const h = Math.floor(m / 60);
  if (h > 0) return `${h}h ${m % 60}m ${s % 60}s`;
  if (m > 0) return `${m}m ${s % 60}s`;
  return `${s}s`;
}

function shortHash(h: string): string {
  return h.length > 14 ? `${h.slice(0, 8)}…${h.slice(-6)}` : h;
}

const STATUS_LABEL: Record<BridgeTransfer["status"], string> = {
  pending_attestation: "awaiting attestation",
  attested: "attested",
  minting: "minting",
  complete: "complete",
  failed: "failed",
};

export function Bridge() {
  const account = useCurrentAccount();
  const submitTx = useSubmitTransaction();

  const [direction, setDirection] = useState<Direction>("sui->solana");
  const [amount, setAmount] = useState("");
  const [destination, setDestination] = useState("");
  const [solanaWallet, setSolanaWallet] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [toast, setToast] = useState<ToastState | null>(null);

  const flash = (message: string, variant: ToastState["variant"] = "success") => {
    setToast({ message, variant });
    setTimeout(() => setToast(null), 6000);
  };

  // Prefill the destination from the connected wallet on the other chain.
  useEffect(() => {
    if (direction === "sui->solana" && solanaWallet) setDestination(solanaWallet);
    if (direction === "solana->sui" && account?.address) setDestination(account.address);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [direction, solanaWallet, account?.address]);

  const wallets = useMemo(() => {
    const w: string[] = [];
    if (account?.address) w.push(account.address);
    if (solanaWallet) w.push(solanaWallet);
    return w;
  }, [account?.address, solanaWallet]);

  const transfers = useBridgeTransfers(wallets);

  // 1s tick so in-flight rows show a live elapsed timer.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);

  const connectSolana = async () => {
    try {
      const address = await connectPhantomWallet();
      setSolanaWallet(address);
    } catch (err) {
      flash(err instanceof Error ? err.message : String(err), "error");
    }
  };

  const bridge = async () => {
    const amountRaw = toRaw(amount);
    if (amountRaw <= 0n) {
      flash("Enter a positive USDC amount", "error");
      return;
    }
    setBusy(true);
    try {
      if (direction === "sui->solana") {
        if (!account) throw new Error("Connect a Sui wallet first");
        if (!destination) throw new Error("Enter the destination Solana wallet address");
        const owner = new PublicKey(destination);
        const ata = deriveUsdcAta(owner);
        const mintRecipientHex =
          "0x" +
          [...ata.toBytes()].map((b) => b.toString(16).padStart(2, "0")).join("");
        const digest = await submitTx(
          buildSuiDepositForBurnTx({ amountRaw, mintRecipientHex }),
        );
        await registerBridgeTransfer({
          txHash: digest,
          originChain: "sui",
          wallet: account.address,
          destinationWallet: destination,
        });
      } else {
        if (!/^0x[0-9a-fA-F]{1,64}$/.test(destination.trim())) {
          throw new Error("Enter the destination Sui address (0x…)");
        }
        const { signature, wallet } = await sendSolanaDepositForBurn({
          amountRaw,
          suiRecipientHex: destination.trim(),
        });
        setSolanaWallet(wallet);
        await registerBridgeTransfer({
          txHash: signature,
          originChain: "solana",
          wallet,
          destinationWallet: destination.trim(),
        });
      }
      posthog.capture("bridge_transfer_started", {
        direction,
        amount_usdc: Number(amount),
      });
      flash("Bridge transfer started — the relay will mint on the destination chain.");
      setAmount("");
      void transfers.refetch();
    } catch (err) {
      posthog.captureException(err, { source: "bridge_transfer" });
      flash(err instanceof Error ? err.message : String(err), "error");
    } finally {
      setBusy(false);
    }
  };

  const suiSideReady = Boolean(CCTP_BRIDGE_PACKAGE_ID);
  const fromLabel = direction === "sui->solana" ? "Sui" : "Solana";
  const toLabel = direction === "sui->solana" ? "Solana" : "Sui";

  return (
    <div className="app__wrap">
      <div className="dash-hero">
        <div className="dash-hero__eyebrow">circle cctp · usdc</div>
        <h1 className="dash-hero__title">Bridge</h1>
        <div className="dash-hero__addr">
          {ENV === "mainnet" ? "Sui ↔ Solana" : "Sui Testnet ↔ Solana Devnet"}
        </div>
      </div>

      <section className="admin-section">
        <div className="admin-section__head">
          <h2 className="admin-section__title">Transfer USDC</h2>
          <div className="admin-section__sub">
            Burns USDC on {fromLabel} through our bridge contract; the relay
            service mints it on {toLabel} automatically.
          </div>
        </div>

        <div className="bridge-form">
          <div className="bridge-form__direction">
            <button
              className={direction === "sui->solana" ? "is-active" : ""}
              onClick={() => setDirection("sui->solana")}
              disabled={busy}
            >
              Sui → Solana
            </button>
            <button
              className={direction === "solana->sui" ? "is-active" : ""}
              onClick={() => setDirection("solana->sui")}
              disabled={busy}
            >
              Solana → Sui
            </button>
          </div>

          {direction === "sui->solana" && !suiSideReady ? (
            <div className="admin-empty">
              The cctp_bridge package isn&apos;t deployed on this network yet.
            </div>
          ) : (
            <>
              <label className="bridge-form__field">
                <span>Amount (USDC)</span>
                <input
                  type="text"
                  inputMode="decimal"
                  placeholder="0.00"
                  value={amount}
                  onChange={(e) => setAmount(e.target.value)}
                  disabled={busy}
                />
              </label>

              <label className="bridge-form__field">
                <span>
                  Destination {toLabel} {direction === "sui->solana" ? "wallet" : "address"}
                </span>
                <input
                  type="text"
                  placeholder={direction === "sui->solana" ? "Solana address (base58)" : "0x…"}
                  value={destination}
                  onChange={(e) => setDestination(e.target.value)}
                  disabled={busy}
                />
              </label>

              <div className="bridge-form__actions">
                {direction === "sui->solana" && !account && (
                  <span className="bridge-form__hint">Connect a Sui wallet to bridge.</span>
                )}
                {!solanaWallet && hasPhantom() && (
                  <button className="bridge-form__secondary" onClick={connectSolana} disabled={busy}>
                    Connect Phantom
                  </button>
                )}
                <button
                  className="bridge-form__submit"
                  onClick={bridge}
                  disabled={busy || (direction === "sui->solana" && !account)}
                >
                  {busy ? "Bridging…" : `Bridge to ${toLabel}`}
                </button>
              </div>
            </>
          )}
        </div>
      </section>

      <section className="admin-section">
        <div className="admin-section__head">
          <h2 className="admin-section__title">Your transfers</h2>
          <div className="admin-section__sub">
            Duration is measured from the on-chain burn to the on-chain mint.
          </div>
        </div>

        {wallets.length === 0 ? (
          <div className="admin-empty">Connect a wallet to see your transfers.</div>
        ) : (transfers.data ?? []).length === 0 ? (
          <div className="admin-empty">No bridge transfers yet.</div>
        ) : (
          <div className="bridge-list">
            {(transfers.data ?? []).map((t) => (
              <TransferRow key={t.id} t={t} now={now} />
            ))}
          </div>
        )}
      </section>

      {toast && <Toast message={toast.message} variant={toast.variant} />}
    </div>
  );
}

function TransferRow({ t, now }: { t: BridgeTransfer; now: number }) {
  const open = t.status !== "complete" && t.status !== "failed";
  const started = t.burned_at_ms ?? t.created_at_ms;
  return (
    <div className={`bridge-row bridge-row--${t.status}`}>
      <div className="bridge-row__main">
        <span className="bridge-row__route">
          {t.origin_chain === "sui" ? "Sui → Solana" : "Solana → Sui"}
        </span>
        <span className="bridge-row__amount">{fmtUsdc(t.amount)} USDC</span>
        <span className={`bridge-row__status bridge-row__status--${t.status}`}>
          {STATUS_LABEL[t.status]}
        </span>
        <span className="bridge-row__timer">
          {t.status === "complete" && t.duration_ms != null
            ? `✓ completed in ${fmtDuration(t.duration_ms)}`
            : open
              ? `⏱ ${fmtDuration(now - started)} elapsed`
              : null}
        </span>
      </div>
      <div className="bridge-row__detail">
        <span>
          burn <code>{shortHash(t.origin_tx_hash)}</code>
        </span>
        {t.mint_tx_hash && (
          <span>
            mint <code>{shortHash(t.mint_tx_hash)}</code>
          </span>
        )}
        {t.status === "failed" && t.error && (
          <span className="bridge-row__error">{t.error}</span>
        )}
      </div>
    </div>
  );
}
