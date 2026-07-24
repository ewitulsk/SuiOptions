// Bluefin account setup wizard (SO-305): a resumable stepper that stands up
// a vault's Bluefin parent account, from key ceremony to an authorized
// trading wallet. Steps:
//   a. keygen         — DKG → parent Sui address (+ mandatory share backup)
//   b. register       — admin set_external_account (rendered CLI + polled)
//   c. fund           — release_external (existing wallet flow)
//   d. deposit        — co-signed deposit_to_asset_bank from the parent
//   e. authorize      — co-signed login + authorize_account of the curator
// Progress persists per vault; the wizard resumes at whatever step is next.

import { useEffect, useState } from "react";
import { normalizeSuiAddress } from "@mysten/sui/utils";

import {
  EQUITY_ORACLE_PACKAGE_ID,
  TRADING_VAULT_OBJECTS,
  TRADING_VAULT_PACKAGE_ID,
} from "../../config";
import type { TradingVaultDetail } from "../../api/tradingVaults";
import { fetchTradingVault } from "../../api/tradingVaults";
import {
  WIZARD_STEPS,
  useCuratorBluefin,
  type UnlockedShare,
  type WizardStep,
} from "../../state/curatorBluefin";
import { KeygenCeremony } from "./KeygenCeremony";
import { DepositStep, AuthorizeStep } from "./SetupCeremonies";
import { ShareUnlock } from "./ShareUnlock";
import { curatorFieldStyle } from "./styles";

const STEP_LABELS: Record<WizardStep, string> = {
  keygen: "Key ceremony",
  register: "Registration",
  fund: "Fund",
  deposit: "Bluefin deposit",
  authorize: "Authorize trading",
  complete: "Done",
};

export function SetupWizard({
  vault,
  symbol,
  decimals,
  onFund,
  onComplete,
}: {
  vault: TradingVaultDetail;
  symbol: string;
  decimals: number | null;
  /** Opens the existing release_external flow (step c). */
  onFund: () => void;
  /** Called once the wizard finishes — the parent panel re-renders trading. */
  onComplete: (share: UnlockedShare) => void;
}) {
  const bf = useCuratorBluefin(vault.vaultId);
  const step: WizardStep = bf.progress?.step ?? "keygen";

  return (
    <div className="vault-card__body">
      <Stepper current={step} />
      {step === "keygen" && (
        <KeygenCeremony
          vaultId={vault.vaultId}
          onComplete={(share, parentAddress, publicKeyPackageB64, groupPublicKeyHex) => {
            bf.setUnlocked(share);
            bf.update({
              step: "register",
              parentAddress,
              publicKeyPackageB64,
              groupPublicKeyHex,
            });
          }}
        />
      )}
      {step === "register" && bf.progress?.parentAddress && (
        <RegisterStep
          vault={vault}
          parentAddress={bf.progress.parentAddress}
          onRegistered={() => bf.update({ step: "fund", registered: true })}
        />
      )}
      {step === "fund" && (
        <FundStep symbol={symbol} onFund={onFund} onDone={() => bf.update({ step: "deposit" })} />
      )}
      {step === "deposit" && bf.progress?.parentAddress && (
        <UnlockedGate share={bf.share} vaultId={vault.vaultId} parentAddress={bf.progress.parentAddress} onUnlock={bf.setUnlocked}>
          {(share) => (
            <DepositStep
              vault={vault}
              symbol={symbol}
              decimals={decimals}
              share={share}
              onDeposited={() => bf.update({ step: "authorize", deposited: true })}
            />
          )}
        </UnlockedGate>
      )}
      {step === "authorize" && bf.progress?.parentAddress && (
        <UnlockedGate share={bf.share} vaultId={vault.vaultId} parentAddress={bf.progress.parentAddress} onUnlock={bf.setUnlocked}>
          {(share) => (
            <AuthorizeStep
              vault={vault}
              share={share}
              onAuthorized={() => {
                bf.update({ step: "complete", authorized: true });
                onComplete(share);
              }}
            />
          )}
        </UnlockedGate>
      )}
      {step === "complete" && (
        <div className="status-pill is-success" style={{ display: "block", fontSize: 12, padding: "8px 10px" }}>
          ✓ Bluefin account is set up. Switch to the External venue tab to trade.
        </div>
      )}
    </div>
  );
}

function Stepper({ current }: { current: WizardStep }) {
  const idx = WIZARD_STEPS.indexOf(current);
  return (
    <div style={{ display: "flex", flexWrap: "wrap", gap: 6, marginBottom: 12 }}>
      {WIZARD_STEPS.filter((s) => s !== "complete").map((s, i) => (
        <span
          key={s}
          className={"status-pill " + (i < idx ? "is-success" : i === idx ? "is-info" : "")}
          style={{ fontSize: 11, opacity: i <= idx ? 1 : 0.5 }}
        >
          {i + 1}. {STEP_LABELS[s]}
        </span>
      ))}
    </div>
  );
}

/** Lazy passphrase-unlock gate: the ceremony steps need the in-memory share.
 * On a resumed session it isn't loaded yet, so prompt for it. */
function UnlockedGate({
  share,
  vaultId,
  parentAddress,
  onUnlock,
  children,
}: {
  share: UnlockedShare | null;
  vaultId: string;
  parentAddress: string;
  onUnlock: (s: UnlockedShare) => void;
  children: (share: UnlockedShare) => React.ReactNode;
}) {
  if (share && share.vaultId === vaultId) return <>{children(share)}</>;
  return (
    <ShareUnlock vaultId={vaultId} expectedParentAddress={parentAddress} onUnlocked={onUnlock} />
  );
}

/** Step b: set_external_account is AdminCap-gated, so render the exact admin
 * invocation and poll the vault until its external account is the parent. */
function RegisterStep({
  vault,
  parentAddress,
  onRegistered,
}: {
  vault: TradingVaultDetail;
  parentAddress: string;
  onRegistered: () => void;
}) {
  const [polling, setPolling] = useState(false);
  const [matched, setMatched] = useState(false);
  const [budgetBps, setBudgetBps] = useState("2000");
  const [dailyBps, setDailyBps] = useState("1000");

  const witnessType = EQUITY_ORACLE_PACKAGE_ID
    ? `${EQUITY_ORACLE_PACKAGE_ID}::equity_oracle::EquityOracle`
    : "<equity-oracle package>::equity_oracle::EquityOracle";
  const oracleRegistry = TRADING_VAULT_OBJECTS?.oracleRegistryId ?? "<oracle registry>";
  const pkg = TRADING_VAULT_PACKAGE_ID ?? "<trading-vault package>";

  const recipe =
    `# Admin (AdminCap holder) registers the parent account as the vault's\n` +
    `# external account, pinning the attested EquityOracle witness.\n` +
    `sui client call \\\n` +
    `  --package ${pkg} \\\n` +
    `  --module vault --function set_external_account \\\n` +
    `  --type-args '${vault.depositAsset}' \\\n` +
    `  --args <ADMIN_CAP> ${vault.vaultId} ${oracleRegistry} \\\n` +
    `         ${parentAddress} '${witnessType}' ${budgetBps} ${dailyBps}`;

  useEffect(() => {
    if (!polling) return;
    let alive = true;
    const tick = async () => {
      try {
        const fresh = await fetchTradingVault(vault.vaultId);
        if (
          alive &&
          fresh.externalAccount &&
          normalizeSuiAddress(fresh.externalAccount) === normalizeSuiAddress(parentAddress)
        ) {
          setMatched(true);
          setPolling(false);
        }
      } catch {
        /* keep polling */
      }
    };
    const h = setInterval(tick, 4000);
    void tick();
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, [polling, vault.vaultId, parentAddress]);

  return (
    <div>
      <div className="vault-prose__muted" style={{ fontSize: 12, marginBottom: 8 }}>
        Registration binds the parent address on-chain. Like allowlisting an
        adapter, it is an admin act — hand this invocation to an AdminCap
        holder, then poll until it lands.
      </div>
      <div style={{ display: "flex", gap: 8, marginBottom: 8 }}>
        <label style={{ fontSize: 11, opacity: 0.8, flex: 1 }}>
          Budget (bps of NAV)
          <input style={curatorFieldStyle} value={budgetBps} onChange={(e) => setBudgetBps(e.target.value)} />
        </label>
        <label style={{ fontSize: 11, opacity: 0.8, flex: 1 }}>
          Daily release (bps)
          <input style={curatorFieldStyle} value={dailyBps} onChange={(e) => setDailyBps(e.target.value)} />
        </label>
      </div>
      <pre
        style={{
          margin: "0 0 8px", padding: 8, fontSize: 11, lineHeight: 1.5, borderRadius: 6,
          border: "1px solid var(--aqua-line, rgba(92,107,122,0.25))", overflowX: "auto",
        }}
      >
        {recipe}
      </pre>
      <button className="vault-invest__tab" style={{ marginBottom: 8 }} onClick={() => void navigator.clipboard.writeText(recipe)}>
        Copy invocation
      </button>
      {matched ? (
        <>
          <div className="status-pill is-success" style={{ display: "block", fontSize: 12, marginBottom: 8 }}>
            ✓ External account registered as the parent address.
          </div>
          <button className="vault-invest__cta" onClick={onRegistered}>
            Continue to funding
          </button>
        </>
      ) : (
        <button className="vault-invest__cta" disabled={polling} onClick={() => setPolling(true)}>
          {polling ? "Waiting for admin registration…" : "Poll for registration"}
        </button>
      )}
    </div>
  );
}

/** Step c: reuse the existing release_external flow, then advance. */
function FundStep({
  symbol,
  onFund,
  onDone,
}: {
  symbol: string;
  onFund: () => void;
  onDone: () => void;
}) {
  return (
    <div>
      <div className="vault-prose__muted" style={{ fontSize: 12, marginBottom: 8 }}>
        Release {symbol} from the vault to the parent address (budgeted
        on-chain). Use the Release control below, then continue — the released
        coins fund the Bluefin deposit in the next step.
      </div>
      <button className="vault-invest__tab" style={{ marginBottom: 8 }} onClick={onFund}>
        Open release control
      </button>
      <button className="vault-invest__cta" onClick={onDone}>
        I've funded the parent — continue
      </button>
    </div>
  );
}
