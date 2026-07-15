// Testnet faucet (SO-93). Lets a connected wallet mint the protocol test
// tokens (TBTC/TUSDC/TDEEP/TWAL) so testers can fund themselves to exercise
// the Earn flow. Testnet-only: on mainnet/devnet `TEST_TOKENS` is empty and
// the page shows a "testnet only" notice. Mirrors `Admin.tsx`'s `run()`
// sign+execute idiom and `dash-hero` layout.

import { useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";
import type { Transaction } from "@mysten/sui/transactions";
import { useSubmitTransaction } from "../tx/submit";

import { Toast } from "../components/Toast";
import { ENV, TEST_TOKENS, type TestToken } from "../config";
import { buildMintTx } from "../tx/faucet";
import { posthog } from "../lib/posthog";

/** Display-units → raw smallest-units. Safe well past any faucet amount. */
function toRaw(amount: string, decimals: number): bigint {
  const n = Number(amount);
  if (!Number.isFinite(n) || n <= 0) return 0n;
  return BigInt(Math.round(n * 10 ** decimals));
}

export function Faucet() {
  const account = useCurrentAccount();
  const wallet = account?.address ?? null;
  const submitTx = useSubmitTransaction();

  const [toast, setToast] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  // Per-token amount inputs, defaulting to 1 whole token.
  const [amounts, setAmounts] = useState<Record<string, string>>(() =>
    Object.fromEntries(TEST_TOKENS.map((t) => [t.symbol, "1"])),
  );

  const flash = (msg: string) => {
    setToast(msg);
    setTimeout(() => setToast(null), 5000);
  };

  // Coins are minted straight to the connected wallet's address.
  const run = async (
    key: string,
    token: TestToken,
    ok: string,
    amount: string,
    walletBuild: () => Transaction,
  ) => {
    setBusy(key);
    try {
      await submitTx(walletBuild());
      posthog.capture("faucet_tokens_minted", {
        token_symbol: token.symbol,
        amount: Number(amount),
        wallet_address: wallet,
        auth: "wallet",
      });
      flash(`✓ ${ok}`);
    } catch (err) {
      posthog.captureException(err, { action: "faucet_tokens_minted", token_symbol: token.symbol, wallet_address: wallet });
      const message = err instanceof Error ? err.message : String(err);
      flash(`failed · ${message}`);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <div className="app__wrap">
        <div className="dash-hero">
          <div className="dash-hero__eyebrow">testnet · developer tooling</div>
          <h1 className="dash-hero__title">Faucet</h1>
          <div className="dash-hero__addr">
            mint test tokens to fund the Earn flow
          </div>
        </div>

        {TEST_TOKENS.length === 0 ? (
          <div className="admin-empty">
            Faucet is testnet-only; current environment is <code>{ENV}</code>.
          </div>
        ) : (
          <section className="admin-section">
            <div className="admin-section__head">
              <h2 className="admin-section__title">Mint test tokens</h2>
              <div className="admin-section__sub">
                {!wallet
                  ? "connect a wallet to mint."
                  : "coins are minted straight to your connected wallet."}
              </div>
            </div>

            <div className="admin-table">
              <div className="admin-table__head admin-table__row">
                <span>Token</span>
                <span>Decimals</span>
                <span>Amount</span>
                <span>Action</span>
              </div>
              {TEST_TOKENS.map((t) => {
                const key = `mint-${t.symbol}`;
                const amount = amounts[t.symbol] ?? "1";
                const ready = wallet !== null && toRaw(amount, t.decimals) > 0n;
                return (
                  <div className="admin-table__row" key={t.symbol}>
                    <span className="admin-cell__asset">
                      {t.symbol}
                      <code className="admin-cell__id">
                        {t.faucetId.slice(0, 6)}…{t.faucetId.slice(-4)}
                      </code>
                    </span>
                    <span>{t.decimals}</span>
                    <span>
                      <input
                        className="admin-field__input"
                        type="number"
                        min={0}
                        value={amount}
                        onChange={(e) =>
                          setAmounts((a) => ({ ...a, [t.symbol]: e.target.value }))
                        }
                      />
                    </span>
                    <span className="admin-cell__actions">
                      <button
                        className="admin-btn admin-btn--primary"
                        disabled={!ready || busy !== null}
                        onClick={() =>
                          run(
                            key,
                            t,
                            `minted ${amount} ${t.symbol}`,
                            amount,
                            () =>
                              buildMintTx({
                                testTokenPackageId: t.packageId,
                                module: t.module,
                                faucetId: t.faucetId,
                                amountRaw: toRaw(amount, t.decimals),
                              }),
                          )
                        }
                      >
                        {busy === key ? "minting…" : "Mint"}
                      </button>
                    </span>
                  </div>
                );
              })}
            </div>
          </section>
        )}
      </div>

      {toast && <Toast message={toast} />}
    </div>
  );
}
