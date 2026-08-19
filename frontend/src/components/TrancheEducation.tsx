// §3.8 tranche education flow (SO-418): a "How tranches work" explainer
// modal — four panels driving the SAME waterfall stack bar through a canned
// scenario (deposit → gain → loss → junior wipe), linked from senior/junior
// badges. Shown automatically ONCE per browser (localStorage) where
// `autoOpen` is set; always reopenable from its trigger link.
//
// Copy is sourced from docs/trading-vault-v2/disclosures.md so product copy
// and legal copy can't drift apart — edit there first.

import { useEffect, useState } from "react";
import { createPortal } from "react-dom";

import { WaterfallStackBar, type StackInputs } from "./WaterfallStackBar";

const SEEN_KEY = "tv-tranche-explainer-seen";

const DISCLOSURES_URL =
  "https://github.com/ewitulsk/SuiOptions/blob/staging/docs/trading-vault-v2/disclosures.md";

/** Canned preferred-only scenario: senior 400k principal, junior 600k. */
const P = 400_000n;
type Panel = { title: string; body: string; inputs: StackInputs };
const PANELS: Panel[] = [
  {
    title: "Deposits fund two tranches",
    body:
      "Senior deposits 400,000 and junior deposits 600,000. Senior holds a " +
      "priority claim: the first 400,000 of vault assets — plus a hurdle " +
      "that accrues on it over time — belongs to senior before junior " +
      "receives anything. Junior owns everything left over.",
    inputs: { mode: "preferred_only", nav: 1_000_000n, claim: 400_000n, principal: P, participationBps: 0, capBps: 0 },
  },
  {
    title: "The vault gains",
    body:
      "NAV grew to 1,200,000 while the hurdle accrued senior's claim to " +
      "410,000. Senior hurdle returns are a priority claim, not guaranteed " +
      "yield. In this vault's preferred-only mode senior's upside stops at " +
      "its claim — junior owns the entire residual gain (participating " +
      "modes give senior a slice of the residual too, reducing junior's).",
    inputs: { mode: "preferred_only", nav: 1_200_000n, claim: 410_000n, principal: P, participationBps: 0, capBps: 0 },
  },
  {
    title: "The vault loses",
    body:
      "NAV fell to 700,000. Junior absorbs first loss: senior's slice is " +
      "untouched at its full 410,000 claim while junior's residual shrank " +
      "from 790,000 to 290,000. If the junior buffer falls below the " +
      "vault's maintenance threshold, junior withdrawals pause (they stay " +
      "queued in order) while senior withdrawals keep flowing.",
    inputs: { mode: "preferred_only", nav: 700_000n, claim: 410_000n, principal: P, participationBps: 0, capBps: 0 },
  },
  {
    title: "Junior is wiped",
    body:
      "NAV fell to 350,000 — below the senior claim. Junior is wiped to " +
      "zero, and senior loses money too: all 350,000 of assets stand " +
      "against a 410,000 claim. The hatched slice is arrears — the claim " +
      "keeps accruing during impairment and absorbs later recovery first. " +
      "Recapitalization happens through a junior reset, which permanently " +
      "wipes the old junior generation: old junior positions stay " +
      "zero-value forever, even if NAV later recovers.",
    inputs: { mode: "preferred_only", nav: 350_000n, claim: 410_000n, principal: P, participationBps: 0, capBps: 0 },
  },
];

function seenBefore(): boolean {
  try {
    return localStorage.getItem(SEEN_KEY) != null;
  } catch {
    return true; // storage unavailable — never auto-open repeatedly
  }
}

function markSeen(): void {
  try {
    localStorage.setItem(SEEN_KEY, "1");
  } catch {
    /* private mode — the explainer stays reachable from its links */
  }
}

/**
 * The "How tranches work" trigger + modal, self-contained so it can hang off
 * any senior/junior badge. `autoOpen` opens it once per browser (intended on
 * the tranched vault detail page only, not on every badge).
 */
export function HowTranchesWork({
  autoOpen = false,
  termsVersion = 1,
  compact = false,
}: {
  autoOpen?: boolean;
  termsVersion?: number;
  /** Bare link text sized for badge rows (no icon). */
  compact?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [step, setStep] = useState(0);

  useEffect(() => {
    if (autoOpen && !seenBefore()) {
      markSeen();
      setOpen(true);
    }
  }, [autoOpen]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    window.addEventListener("keydown", onKey);
    const prevOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      window.removeEventListener("keydown", onKey);
      document.body.style.overflow = prevOverflow;
    };
  }, [open]);

  const panel = PANELS[step];

  return (
    <>
      <button
        className="vault-howto__trigger"
        style={compact ? { fontSize: 10, padding: 0 } : undefined}
        onClick={() => {
          markSeen();
          setStep(0);
          setOpen(true);
        }}
      >
        {!compact && (
          <span className="vault-howto__icon" aria-hidden>
            i
          </span>
        )}
        How tranches work
      </button>
      {open &&
        createPortal(
          // display:contents → carries the aqua theme variables (incl.
          // dark-mode swaps) without generating a box or stacking context.
          <div data-theme="aqua" style={{ display: "contents" }}>
            <div className="vault-modal__scrim" onClick={() => setOpen(false)} />
            <div className="vault-modal" role="dialog" aria-modal="true" aria-label="How tranches work">
              <div className="vault-modal__head">
                <span>
                  How tranches work · {step + 1}/{PANELS.length}
                </span>
                <button className="vault-modal__close" onClick={() => setOpen(false)} aria-label="Close">
                  ×
                </button>
              </div>
              <div className="vault-modal__body vault-prose">
                <p style={{ marginTop: 0 }}>
                  <b>{panel.title}.</b> {panel.body}
                </p>
                {/* The same stack-bar component as the live waterfall
                    explorer, on the canned scenario. Whole units, no asset. */}
                <WaterfallStackBar inputs={panel.inputs} symbol="" decimals={0} />
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 8,
                    marginTop: 14,
                  }}
                >
                  <button
                    className="vault-invest__tab"
                    disabled={step === 0}
                    onClick={() => setStep((s) => Math.max(0, s - 1))}
                  >
                    ← Back
                  </button>
                  <span
                    className="vault-prose__muted"
                    style={{ fontSize: 11, letterSpacing: 2, margin: "0 auto" }}
                    aria-hidden
                  >
                    {PANELS.map((_, i) => (i === step ? "●" : "○")).join(" ")}
                  </span>
                  {step < PANELS.length - 1 ? (
                    <button className="vault-invest__tab" onClick={() => setStep((s) => s + 1)}>
                      Next →
                    </button>
                  ) : (
                    <button className="vault-invest__tab" onClick={() => setOpen(false)}>
                      Done
                    </button>
                  )}
                </div>
                <p className="vault-prose__muted" style={{ fontSize: 11, marginBottom: 0 }}>
                  Sourced from the{" "}
                  <a href={DISCLOSURES_URL} target="_blank" rel="noreferrer">
                    Terms &amp; Risk Disclosures
                  </a>{" "}
                  (terms v{termsVersion}) — read them before depositing into a
                  tranched vault.
                </p>
              </div>
            </div>
          </div>,
          document.body,
        )}
    </>
  );
}
