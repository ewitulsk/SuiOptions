// Venue-neutral "External venue" panel (SO-305), replacing the old Hedge
// tab. Future venues slot in beside Bluefin behind the same `venue`
// selector. Holds the release_external control (the shared funding
// primitive), the Bluefin setup wizard, the trading panel, and the in-app
// sweep-back. The copy-paste sweep recipe is gone.

import { useEffect, useState } from "react";

import type { TradingVaultDetail } from "../../api/tradingVaults";
import { fetchFrostPubkey } from "../../api/hedgeSigner";
import type { AppraisalPlan } from "../../tx/appraisal";
import type { useTradingVaultActions } from "../../state/tradingVault";
import { useCuratorBluefin, type UnlockedShare } from "../../state/curatorBluefin";
import { BluefinPanel } from "./BluefinPanel";
import { SetupWizard } from "./SetupWizard";
import { ShareUnlock } from "./ShareUnlock";
import { SweepBack } from "./SweepBack";

type Actions = ReturnType<typeof useTradingVaultActions>;

export function ExternalVenuePanel({
  vault,
  symbol,
  decimals,
  actions,
  cfgId,
  plan,
  planError,
}: {
  vault: TradingVaultDetail;
  symbol: string;
  decimals: number | null;
  actions: Actions;
  cfgId: string | null;
  plan: AppraisalPlan | null;
  planError: string | null;
}) {
  const bf = useCuratorBluefin(vault.vaultId);
  // Only Bluefin today; future venues slot in beside it behind this tab row.
  const [sub, setSub] = useState<"setup" | "trade" | "sweep">("setup");

  // Resolve the parent address: wizard progress, else a live signer lookup
  // (a vault whose account was set up in a previous session/browser).
  const [parentAddress, setParentAddress] = useState<string | null>(
    bf.progress?.parentAddress ?? null,
  );
  useEffect(() => {
    if (parentAddress) return;
    let alive = true;
    fetchFrostPubkey(vault.vaultId)
      .then((r) => alive && r && setParentAddress(r.suiAddress))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [vault.vaultId, parentAddress]);

  const setupComplete =
    bf.progress?.step === "complete" ||
    (vault.externalAccount != null && parentAddress != null);

  // Default to the trade tab once set up.
  useEffect(() => {
    if (setupComplete && sub === "setup") setSub("trade");
  }, [setupComplete, sub]);

  return (
    <>
      <div className="vault-invest__tabs" style={{ marginBottom: 10 }}>
        <button className="vault-invest__tab is-active">Bluefin</button>
      </div>

      <ReleaseControl
        vault={vault}
        symbol={symbol}
        decimals={decimals}
        actions={actions}
        cfgId={cfgId}
        plan={plan}
        planError={planError}
      />

      <div className="vault-invest__tabs" style={{ margin: "12px 0" }}>
        <button
          className={"vault-invest__tab" + (sub === "setup" ? " is-active" : "")}
          onClick={() => setSub("setup")}
        >
          Setup
        </button>
        <button
          className={"vault-invest__tab" + (sub === "trade" ? " is-active" : "")}
          onClick={() => setSub("trade")}
          disabled={!setupComplete}
        >
          Trade
        </button>
        <button
          className={"vault-invest__tab" + (sub === "sweep" ? " is-active" : "")}
          onClick={() => setSub("sweep")}
          disabled={!setupComplete}
        >
          Sweep
        </button>
      </div>

      {sub === "setup" &&
        (setupComplete ? (
          <div className="status-pill status-pill--note is-success">
            ✓ Bluefin account is set up (parent {parentAddress?.slice(0, 10)}…). Use the Trade
            and Sweep tabs.
          </div>
        ) : (
          <SetupWizard
            vault={vault}
            symbol={symbol}
            decimals={decimals}
            onFund={() => {
              document.getElementById("release-control")?.scrollIntoView({ behavior: "smooth" });
            }}
            onComplete={(share) => {
              bf.setUnlocked(share);
              setParentAddress(share.parentAddress);
              setSub("trade");
            }}
          />
        ))}

      {sub === "trade" && parentAddress && <BluefinPanel parentAddress={parentAddress} />}

      {sub === "sweep" && parentAddress && (
        <SweepGate vault={vault} symbol={symbol} decimals={decimals} share={bf.share} parentAddress={parentAddress} onUnlock={bf.setUnlocked} />
      )}
    </>
  );
}

function SweepGate({
  vault,
  symbol,
  decimals,
  share,
  parentAddress,
  onUnlock,
}: {
  vault: TradingVaultDetail;
  symbol: string;
  decimals: number | null;
  share: UnlockedShare | null;
  parentAddress: string;
  onUnlock: (s: UnlockedShare) => void;
}) {
  if (!share || share.vaultId !== vault.vaultId) {
    return (
      <ShareUnlock vaultId={vault.vaultId} expectedParentAddress={parentAddress} onUnlocked={onUnlock} />
    );
  }
  return <SweepBack vault={vault} symbol={symbol} decimals={decimals} share={share} />;
}

/** The budgeted release_external control (unchanged behavior from the old
 * HedgePanel): budget + daily window are enforced on-chain against the NAV
 * this appraisal snapshots, so no client-side headroom preview. */
function ReleaseControl({
  vault,
  symbol,
  decimals,
  actions,
  cfgId,
  plan,
  planError,
}: {
  vault: TradingVaultDetail;
  symbol: string;
  decimals: number | null;
  actions: Actions;
  cfgId: string | null;
  plan: AppraisalPlan | null;
  planError: string | null;
}) {
  const [amount, setAmount] = useState("");

  if (vault.externalAccount == null) {
    return (
      <div className="vault-card__body vault-prose__muted" id="release-control">
        No external account is registered yet. Complete the setup wizard below —
        the register step submits it from your wallet.
      </div>
    );
  }

  const amountNum = Number(amount) || 0;
  const disabled =
    !!actions.busy || amountNum <= 0 || decimals == null || plan == null || !cfgId || vault.state !== "open";
  const title =
    vault.state !== "open"
      ? "The vault is no longer open"
      : !cfgId
        ? "Protocol config unavailable"
        : plan == null
          ? planError
            ? `Release unavailable: ${planError}`
            : "Analyzing vault holdings…"
          : undefined;

  const onRelease = () => {
    if (decimals == null || amountNum <= 0 || plan == null || !cfgId) return;
    actions.releaseExternal({
      plan,
      protocolConfigId: cfgId,
      curatorCapId: vault.curatorCapId,
      amountRaw: BigInt(Math.round(amountNum * 10 ** decimals)),
    });
    setAmount("");
  };

  return (
    <div id="release-control">
      <div className="vault-card__head" style={{ fontSize: 13 }}>
        Release to external account
      </div>
      <div className="vault-invest__field" style={{ marginTop: 8 }}>
        <input
          className="amount__input"
          type="number"
          min="0"
          placeholder="0.0"
          value={amount}
          onChange={(e) => setAmount(e.target.value)}
        />
        <span className="vault-invest__unit">{symbol}</span>
      </div>
      <div className="vault-invest__bal">
        budget and daily release window are enforced on-chain at release time
      </div>
      <button className="vault-invest__cta" disabled={disabled} onClick={onRelease} title={title}>
        {actions.busy ? `${actions.busy}…` : `Release ${symbol} to external account`}
      </button>
      {title && <div className="vault-card__foot vault-prose__muted">{title}</div>}
    </div>
  );
}
