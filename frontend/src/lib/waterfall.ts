// Client-side mirror of the trading-vault v2 waterfall (SO-418, spec §3.4a —
// docs/trading-vault-v2/spec.md §3). Pure bigint math with floor division so
// the UI can never disagree with the chain: the same formula the contract
// runs on every consumed appraisal, re-runnable here at hypothetical NAVs
// for the §3.1 what-if explorer and the §3.6 deposit preview.
//
//   preferred     = min(NAV, C)
//   residual      = NAV − preferred
//   participation =
//       PreferredOnly         → 0
//       CappedParticipating   → min(residual × participation_bps / 10⁴,
//                                   max(0, P × total_return_cap_bps / 10⁴ − preferred))
//       UncappedParticipating → residual × participation_bps / 10⁴
//   senior_nav    = preferred + participation
//   junior_nav    = NAV − senior_nav

import type { UpsideMode } from "../api/tradingVaults";

const BPS = 10_000n;

export type WaterfallSplit = {
  /** min(NAV, C) — the funded slice of the senior claim. */
  preferred: bigint;
  /** Senior's share of the residual (participating modes only). */
  participation: bigint;
  seniorNav: bigint;
  juniorNav: bigint;
};

/**
 * The §3.4a waterfall. All monetary inputs in accounting-asset smallest
 * units; `principalBasis` is P (0n when unknown — only the capped mode reads
 * it, and a zero basis floors its cap headroom at zero).
 */
export function waterfall(
  mode: UpsideMode,
  totalNav: bigint,
  accruedClaim: bigint,
  principalBasis: bigint,
  participationBps: number,
  capBps: number,
): WaterfallSplit {
  const preferred = totalNav < accruedClaim ? totalNav : accruedClaim;
  const residual = totalNav - preferred;
  let participation = 0n;
  if (mode === "uncapped_participating") {
    participation = (residual * BigInt(participationBps)) / BPS;
  } else if (mode === "capped_participating") {
    const share = (residual * BigInt(participationBps)) / BPS;
    const capTotal = (principalBasis * BigInt(capBps)) / BPS;
    const headroom = capTotal > preferred ? capTotal - preferred : 0n;
    participation = share < headroom ? share : headroom;
  }
  const seniorNav = preferred + participation;
  return { preferred, participation, seniorNav, juniorNav: totalNav - seniorNav };
}

/**
 * The two slider break points for the §3.1 explorer, as NAV levels.
 *
 * Mathematically the waterfall has a single kink at NAV = C: exactly there
 * junior's residual hits zero AND senior first falls short of its full
 * claim (arrears begin) — the protocol's Impaired predicate. The second
 * economically distinct level is NAV = P (< C, since C = principal +
 * accrual): below it senior loses actual principal, not just accrued
 * hurdle. `seniorPrincipal` is null when P is unknown or not below C.
 */
export function waterfallBreakpoints(
  accruedClaim: bigint,
  principalBasis: bigint | null,
): { juniorWiped: bigint; seniorPrincipal: bigint | null } {
  return {
    juniorWiped: accruedClaim,
    seniorPrincipal:
      principalBasis != null && principalBasis > 0n && principalBasis < accruedClaim
        ? principalBasis
        : null,
  };
}

// ── dev-only self-check ─────────────────────────────────────────────────────
// The repo has no frontend test runner, so the spec §3 worked examples and
// invariants run as assertions on module load in dev builds only (compiled
// out of production by Vite's dead-code elimination on import.meta.env.DEV).

function assertEq(label: string, got: WaterfallSplit, want: [bigint, bigint]) {
  if (got.seniorNav !== want[0] || got.juniorNav !== want[1]) {
    throw new Error(
      `waterfall self-check "${label}": got (${got.seniorNav}/${got.juniorNav}), want (${want[0]}/${want[1]})`,
    );
  }
}

function assertInvariants(label: string, nav: bigint, s: WaterfallSplit) {
  const residual = nav - s.preferred;
  if (s.seniorNav + s.juniorNav !== nav) throw new Error(`${label}: split != NAV`);
  if (s.participation > residual) throw new Error(`${label}: participation > residual`);
  if (s.juniorNav < 0n || s.seniorNav < 0n) throw new Error(`${label}: negative slice`);
}

if (import.meta.env.DEV) {
  const NAV = 1_000_000n;
  const C = 400_000n;
  const P = 400_000n;
  // Spec §3 worked examples.
  assertEq("preferred-only", waterfall("preferred_only", NAV, C, P, 0, 0), [400_000n, 600_000n]);
  assertEq(
    "uncapped 30%",
    waterfall("uncapped_participating", NAV, C, P, 3000, 0),
    [580_000n, 420_000n],
  );
  assertEq(
    "capped 50% @ 120% (C accrued to 410k)",
    waterfall("capped_participating", NAV, 410_000n, P, 5000, 12_000),
    [480_000n, 520_000n],
  );
  // Spec §3 boundaries: NAV 0 ⇒ (0/0); NAV < C ⇒ (NAV/0) in every mode.
  for (const mode of ["preferred_only", "capped_participating", "uncapped_participating"] as const) {
    assertEq(`${mode} NAV=0`, waterfall(mode, 0n, C, P, 5000, 12_000), [0n, 0n]);
    assertEq(`${mode} NAV<C`, waterfall(mode, 300_000n, C, P, 5000, 12_000), [300_000n, 0n]);
  }
  // Invariants over a NAV sweep in every mode.
  for (const mode of ["preferred_only", "capped_participating", "uncapped_participating"] as const) {
    for (let nav = 0n; nav <= 2_000_000n; nav += 50_000n) {
      assertInvariants(`${mode} @ ${nav}`, nav, waterfall(mode, nav, 410_000n, P, 3000, 12_000));
    }
  }
  // Uncapped: junior retains exactly (10⁴ − participation_bps) of residual
  // (floor division may leave dust with senior — spec floors participation,
  // so junior's floor-complement check uses the exact identity instead).
  {
    const s = waterfall("uncapped_participating", NAV, C, P, 3000, 0);
    const residual = NAV - s.preferred;
    if (s.juniorNav !== residual - (residual * 3000n) / BPS) {
      throw new Error("uncapped residual identity failed");
    }
  }
}
