// Covered-call vault page (SO vault system — PRs 137/148/168).
//
// Reads the api-service vault endpoints (`/vaults`, `/vaults/:id/rounds`) and
// the user's on-chain receipts/share balance, and drives every invest action
// (deposit / claim / initiate+complete withdraw / cancel) through
// `useVaultActions`.
//
// All decision-support fields (fees, strike band, round cadence, live phase)
// are served from the vault's on-chain VaultConfig via api-service — no mocks.
// Fields render "—" until the config-carrying events are indexed for a vault.

import { useState, useEffect, useMemo, type CSSProperties } from "react";
import { createPortal } from "react-dom";
import { useCurrentAccount } from "@mysten/dapp-kit";

import type { Vault, VaultRound, VaultApyPoint, VaultRfq } from "../api/vaults";
import { useVault, useVaultRounds, useVaultApyHistory, useOwnedVaultReceipts, useShareBalance, useVaults, useVaultRfqs, useRfqBids } from "../api/useVaults";
import { useVaultActions } from "../state/vault";
import { VaultApyChart } from "../components/VaultApyChart";
import { TokenLogo } from "../components/TokenLogo";
import { findToken } from "../config";
import { Toast } from "../components/Toast";
import { formatPrice } from "../format";

function scaled(raw: string | null | undefined, decimals: number | null): number | null {
  if (raw == null || decimals == null) return null;
  return Number(raw) / 10 ** decimals;
}

function toRaw(amount: number, decimals: number): bigint {
  // Round to the nearest atomic unit. Fine for the amounts a deposit UI takes;
  // BigInt keeps the on-chain value exact once scaled.
  return BigInt(Math.round(amount * 10 ** decimals));
}

function fmtPct(x: number | null | undefined, digits = 2): string {
  if (x == null || !Number.isFinite(x)) return "—";
  return `${(x * 100).toFixed(digits)}%`;
}

function fmtDate(ms: number | null | undefined): string {
  if (ms == null) return "—";
  return new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
}

/** Date + time of day — for within-round timestamps like the selling window. */
function fmtDateTime(ms: number | null | undefined): string {
  if (ms == null || ms <= 0) return "—";
  return new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

/** A scaled raw amount with its unit, or "—" when missing. */
function fmtAmt(
  raw: string | null | undefined,
  decimals: number | null,
  unit: string,
): string {
  const v = scaled(raw, decimals);
  return v != null ? `${formatPrice(v, { grouping: true })} ${unit}` : "—";
}

/** Human cadence from a round length in ms (e.g. 604800000 → "Weekly"). */
function fmtCadence(ms: number | null | undefined): string {
  if (ms == null || ms <= 0) return "—";
  const days = ms / 86_400_000;
  if (Math.abs(days - 7) < 0.1) return "Weekly";
  if (Math.abs(days - 1) < 0.1) return "Daily";
  if (Math.abs(days - 14) < 0.1) return "Biweekly";
  if (Math.abs(days - 30) < 1) return "Monthly";
  return days >= 1 ? `${Math.round(days)}d` : `${Math.round(ms / 3_600_000)}h`;
}

function fmtPctRaw(x: number | null | undefined, digits = 1): string {
  if (x == null || !Number.isFinite(x)) return "—";
  return `${x.toFixed(digits)}%`;
}

/** Abbreviate a 0x id/address for dense tables: 0x1234…cdef. */
function shortHex(s: string): string {
  return s.length > 12 ? `${s.slice(0, 6)}…${s.slice(-4)}` : s;
}

/** The most recent point in a series (by timestamp), or null when empty. */
function latestApyPoint(pts: VaultApyPoint[]): VaultApyPoint | null {
  if (pts.length === 0) return null;
  return pts.reduce((a, b) => (b.t_ms > a.t_ms ? b : a));
}

/** APY of the most recent point in a series (by timestamp), or null when empty. */
function latestApy(pts: VaultApyPoint[]): number | null {
  return latestApyPoint(pts)?.apy ?? null;
}

/** Underlying asset logo, falling back to a glyph when token-info has none. */
function AssetGlyph({ asset }: { asset: string }) {
  const fallback =
    asset === "BTC" ? (
      <span className="asset-glyph asset-glyph--btc">₿</span>
    ) : asset === "SUI" ? (
      <span className="asset-glyph asset-glyph--sui">≈</span>
    ) : (
      <span className="asset-glyph">{asset[0]}</span>
    );
  return <TokenLogo symbol={asset} className="asset-glyph" fallback={fallback} />;
}

const SELECTED_VAULT_KEY = "tideline.selectedVault";
const FOREGROUND_VAULT_KEY = "tideline.foregroundVault";
const SEARCH_QUERY_KEY = "tideline.vaultSearch";

export function VaultScreen() {
  const vaults = useVaults();
  // Hide paused (decommissioned) vaults from the public listing. The admin
  // page intentionally still sees them (to unpause), so filter here, not in
  // the shared `useVaults` hook.
  const visible = useMemo(
    () => (vaults.data ?? []).filter((v) => !v.deposits_paused),
    [vaults.data],
  );
  // Persist the open vault across nav-away/back (the screen unmounts on route
  // change), so returning to Vaults reopens the same detail.
  const [selected, setSelectedState] = useState<string | null>(
    () => sessionStorage.getItem(SELECTED_VAULT_KEY),
  );
  const setSelected = (id: string | null) => {
    setSelectedState(id);
    if (id) sessionStorage.setItem(SELECTED_VAULT_KEY, id);
    else sessionStorage.removeItem(SELECTED_VAULT_KEY);
  };

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <div className="app__wrap">
        {vaults.isLoading && <div className="vault-note">Loading vaults…</div>}
        {vaults.isError && (
          <div className="dash-alert" role="alert">
            Couldn't load vaults: {vaults.error.message}
          </div>
        )}
        {vaults.data && visible.length === 0 && (
          <div className="dash-empty">
            <div className="dash-empty__title">no vaults yet.</div>
            <div className="dash-empty__sub">
              No covered-call vault has been deployed on this network.
            </div>
          </div>
        )}

        {/* Selection screen: a searchable coverflow carousel of vault cards
            (asset logo, APY sparkline, realized & projected APY). Picking one
            drills into its detail; the back link returns here. */}
        {selected ? (
          <VaultDetail vaultId={selected} onBack={() => setSelected(null)} />
        ) : (
          visible.length > 0 && (
            <VaultBrowser vaults={visible} onSelect={setSelected} />
          )
        )}
      </div>
    </div>
  );
}

// Search + coverflow carousel. Search filters by asset symbol so the page scales
// to many vaults; the carousel renders the filtered set.
function VaultBrowser({
  vaults,
  onSelect,
}: {
  vaults: Vault[];
  onSelect: (id: string) => void;
}) {
  // Persist the search across nav-away so returning to Vaults keeps the filter.
  const [query, setQueryState] = useState(() => sessionStorage.getItem(SEARCH_QUERY_KEY) ?? "");
  const setQuery = (next: string) => {
    setQueryState(next);
    if (next) sessionStorage.setItem(SEARCH_QUERY_KEY, next);
    else sessionStorage.removeItem(SEARCH_QUERY_KEY);
  };
  // Normalize away spaces so "testbitcoin" matches the catalog's "Test Bitcoin".
  const norm = (s: string) => s.toLowerCase().replace(/\s+/g, "");
  const q = norm(query.trim());
  // Match on ticker (TBTC), settlement ticker, and the catalog's full asset
  // name (e.g. "Test Bitcoin") so users can search by either form.
  const matches = (v: Vault) => {
    const fields = [
      v.underlying_symbol,
      v.settlement_symbol ?? "",
      findToken(v.underlying_symbol)?.name ?? "",
      findToken(v.settlement_symbol)?.name ?? "",
    ];
    return fields.some((f) => norm(f).includes(q));
  };
  const filtered = q ? vaults.filter(matches) : vaults;

  return (
    <>
      {/* Centered heading (the nav tab already says "Vaults"). Search sits at
          the opposite end, below the carousel — also centered. */}
      <div className="vault-browser__head">
        <span className="vault-head__badge">Covered-call vaults</span>
        <span className="vault-head__tag">Deposit once and your premium compounds every round.</span>
      </div>

      {filtered.length === 0 ? (
        <div className="vault-empty">
          <div className="vault-empty__title">No matches</div>
          <div className="vault-empty__sub">No vault matches “{query}”.</div>
        </div>
      ) : (
        <VaultCarousel vaults={filtered} onSelect={onSelect} />
      )}

      {vaults.length > 1 && (
        <div className="vault-browser__search">
          <div className="vault-search">
            <span className="vault-search__icon" aria-hidden>⌕</span>
            <input
              className="vault-search__input"
              type="text"
              placeholder="Search vaults by asset…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
            {query && (
              <button
                className="vault-search__clear"
                onClick={() => setQuery("")}
                aria-label="Clear search"
              >
                ×
              </button>
            )}
          </div>
        </div>
      )}
    </>
  );
}

function VaultCarousel({
  vaults,
  onSelect,
}: {
  vaults: Vault[];
  onSelect: (id: string) => void;
}) {
  // Two-axis coverflow: each column is an asset (horizontal), each row a vault
  // of that asset at a different expiry cadence (vertical). Columns keep their
  // first-seen order; within a column the shortest cadence sits on top.
  const groups = useMemo(() => {
    const by = new Map<string, Vault[]>();
    const order: string[] = [];
    for (const v of vaults) {
      if (!by.has(v.underlying_symbol)) {
        by.set(v.underlying_symbol, []);
        order.push(v.underlying_symbol);
      }
      by.get(v.underlying_symbol)!.push(v);
    }
    const cadence = (v: Vault) => v.round_ms ?? Number.POSITIVE_INFINITY;
    return order.map((k) =>
      by.get(k)!.slice().sort((a, b) => cadence(a) - cadence(b)),
    );
  }, [vaults]);

  // Default to the middle column so the fan is balanced on both sides.
  const middle = Math.max(0, Math.floor((groups.length - 1) / 2));
  const stacked = groups.some((g) => g.length > 1);

  // Resolve the remembered foreground vault (by id, since filtering shifts
  // indices) to its column+row, or the center column / top row when absent.
  const locate = () => {
    const id = sessionStorage.getItem(FOREGROUND_VAULT_KEY);
    if (id) {
      for (let g = 0; g < groups.length; g++) {
        const r = groups[g].findIndex((v) => v.vault_id === id);
        if (r >= 0) return { group: g, row: r };
      }
    }
    return { group: middle, row: 0 };
  };

  const [pos, setPosRaw] = useState(locate);
  const groupsKey = groups.map((g) => g.map((v) => v.vault_id).join("|")).join(",");

  // Remember the foreground vault so it persists across nav-away / filtering.
  const setPos = (next: { group: number; row: number }) => {
    const v = groups[next.group]?.[next.row];
    if (v) sessionStorage.setItem(FOREGROUND_VAULT_KEY, v.vault_id);
    setPosRaw(next);
  };

  // When the (filtered) set changes, re-resolve — preferring the remembered
  // card if it's still in view, so clearing a search restores the selection.
  useEffect(() => {
    setPosRaw(locate());
  }, [groupsKey]);

  const activeGroup = Math.min(pos.group, groups.length - 1);
  const column = groups[activeGroup] ?? [];
  const activeRow = Math.min(pos.row, column.length - 1);

  const goH = (dir: number) => {
    const g = Math.max(0, Math.min(groups.length - 1, activeGroup + dir));
    setPos({ group: g, row: Math.min(activeRow, groups[g].length - 1) });
  };
  const goV = (dir: number) => {
    const r = Math.max(0, Math.min(column.length - 1, activeRow + dir));
    setPos({ group: activeGroup, row: r });
  };

  // ←/→ switch asset, ↑/↓ switch cadence — but not while typing in search.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const tag = (e.target as HTMLElement | null)?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA") return;
      if (e.key === "ArrowLeft") goH(-1);
      else if (e.key === "ArrowRight") goH(1);
      else if (e.key === "ArrowUp") { e.preventDefault(); goV(-1); }
      else if (e.key === "ArrowDown") { e.preventDefault(); goV(1); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [activeGroup, activeRow, groups]);

  const HVISIBLE = 2; // asset columns rendered each side of the active one
  const VVISIBLE = 1; // cadence cards rendered above & below the active one

  // Flatten to renderable cards: every row of the active column (the vertical
  // fan) plus the top card of each other column (the horizontal fan).
  const items = groups.flatMap((group, gi) => {
    const hOffset = gi - activeGroup;
    const isActiveCol = hOffset === 0;
    const rows = isActiveCol ? group.map((_, r) => r) : [0];
    const center = isActiveCol ? activeRow : 0;
    return rows.map((r) => ({
      vault: group[r],
      gi,
      row: r,
      hOffset,
      vOffset: r - center,
      isActiveCol,
    }));
  });

  const multipleAssets = groups.length > 1;

  return (
    <>
      <div className={"vault-coverflow" + (stacked ? " vault-coverflow--stacked" : "")}>
        <div className="vault-coverflow__stage">
          {items.map(({ vault: v, gi, row, hOffset, vOffset, isActiveCol }) => {
            const hAbs = Math.abs(hOffset);
            const vAbs = Math.abs(vOffset);
            const depth = hAbs + vAbs;
            const hidden = hAbs > HVISIBLE || vAbs > VVISIBLE;
            const foreground = isActiveCol && vOffset === 0;
            const style: CSSProperties = {
              transform: `translate(-50%, -50%) translateX(${hOffset * 58}%) translateY(${vOffset * 38}%) scale(${Math.max(
                1 - depth * 0.14,
                0.58,
              )})`,
              opacity: hidden ? 0 : Math.max(1 - depth * 0.36, 0),
              zIndex: 50 - depth,
              pointerEvents: hidden ? "none" : "auto",
            };
            const onClick = () => {
              if (foreground) onSelect(v.vault_id);
              else if (isActiveCol) setPos({ group: activeGroup, row });
              else setPos({ group: gi, row: Math.min(activeRow, groups[gi].length - 1) });
            };
            return (
              <div
                className={"vault-coverflow__item" + (foreground ? " vault-coverflow__item--active" : "")}
                style={style}
                key={v.vault_id}
                aria-hidden={hidden}
              >
                <VaultTile vault={v} active={foreground} onSelect={onClick} />
              </div>
            );
          })}
        </div>
      </div>

      {/* Always rendered (even with one asset) so the row's height is reserved
          and the search bar below keeps a consistent position. */}
      <div className="vault-carousel__nav">
        {multipleAssets && (
          <>
            <button
              className="vault-carousel__arrow vault-carousel__arrow--prev"
              onClick={() => goH(-1)}
              disabled={activeGroup === 0}
              aria-label="Previous asset"
            >
              ‹
            </button>
            <button
              className="vault-carousel__arrow vault-carousel__arrow--next"
              onClick={() => goH(1)}
              disabled={activeGroup === groups.length - 1}
              aria-label="Next asset"
            >
              ›
            </button>
          </>
        )}
      </div>
    </>
  );
}

/** Compact APY sparkline for a vault card: a solid realized line (no fill) and a
 *  dashed projected line with a confidence band, drawn as inline SVG (no
 *  per-card chart instances) on a shared time/value scale. */
function VaultSparkline({
  realized,
  predicted,
}: {
  realized: VaultApyPoint[];
  predicted: VaultApyPoint[];
}) {
  const W = 100;
  const H = 40;

  const rSorted = [...realized].sort((a, b) => a.t_ms - b.t_ms);
  const pSorted = [...predicted].sort((a, b) => a.t_ms - b.t_ms);
  // Anchor the projected segment to the last realized point so it continues the
  // curve rather than floating disconnected.
  const anchor = rSorted.length ? [rSorted[rSorted.length - 1]] : [];
  const pLine = [...anchor, ...pSorted];

  const all = [...rSorted, ...pLine];
  if (all.length < 2) {
    return (
      <div className="vault-tile__spark vault-tile__spark--empty">
        APY history fills in as rounds settle
      </div>
    );
  }

  // Band over the projected segment: pinched at the realized anchor (no band
  // there), opening to [apy_low, apy_high] across the forecast.
  const pBand = pLine.map((p) => ({
    t: p.t_ms,
    blo: p.apy_low ?? p.apy,
    bhi: p.apy_high ?? p.apy,
  }));
  const hasBand = pBand.some((b) => b.bhi - b.blo > 1e-4);

  const ts = all.map((p) => p.t_ms);
  // Include the band extremes in the y-range so a wide band doesn't clip.
  const vs = [
    ...all.map((p) => p.apy),
    ...(hasBand ? pBand.flatMap((b) => [b.blo, b.bhi]) : []),
  ];
  const tMin = Math.min(...ts);
  const tMax = Math.max(...ts);
  const vMin = Math.min(...vs);
  const vMax = Math.max(...vs);
  const pad = (vMax - vMin) * 0.18 || Math.abs(vMax) * 0.18 || 0.01;
  const lo = vMin - pad;
  const hi = vMax + pad;

  const x = (t: number) => (tMax === tMin ? 0 : ((t - tMin) / (tMax - tMin)) * W);
  const y = (v: number) => H - ((v - lo) / (hi - lo)) * H;
  const pts = (arr: VaultApyPoint[]) =>
    arr.map((p) => `${x(p.t_ms).toFixed(2)},${y(p.apy).toFixed(2)}`);

  const rPts = pts(rSorted);
  const pPts = pts(pLine);
  const rPath = rPts.length ? "M" + rPts.join(" L") : "";
  const pPath = pPts.length ? "M" + pPts.join(" L") : "";
  // Top edge (highs L→R) then bottom edge (lows R→L), closed into a ribbon.
  const bandHi = pBand.map((b) => `${x(b.t).toFixed(2)},${y(b.bhi).toFixed(2)}`);
  const bandLo = pBand.map((b) => `${x(b.t).toFixed(2)},${y(b.blo).toFixed(2)}`);
  const bandPath =
    hasBand && bandHi.length >= 2
      ? `M${bandHi.join(" L")} L${bandLo.reverse().join(" L")} Z`
      : "";

  return (
    <svg
      className="vault-tile__spark"
      viewBox={`0 0 ${W} ${H}`}
      preserveAspectRatio="none"
      aria-hidden
    >
      {bandPath && <path className="vault-tile__spark-band" d={bandPath} />}
      {rPath && (
        <path className="vault-tile__spark-line vault-tile__spark-line--realized" d={rPath} vectorEffect="non-scaling-stroke" />
      )}
      {pPath && (
        <path className="vault-tile__spark-line vault-tile__spark-line--predicted" d={pPath} vectorEffect="non-scaling-stroke" />
      )}
    </svg>
  );
}

function VaultTile({
  vault,
  onSelect,
  active = true,
}: {
  vault: Vault;
  onSelect: () => void;
  active?: boolean;
}) {
  const apyQ = useVaultApyHistory(vault.vault_id);
  const realizedSeries = apyQ.data?.realized ?? [];
  const predictedSeries = apyQ.data?.predicted ?? [];
  const realized = latestApy(realizedSeries) ?? vault.apy;
  const projectedPt = latestApyPoint(predictedSeries);
  const projected = projectedPt?.apy ?? null;
  const lo = projectedPt?.apy_low;
  const hi = projectedPt?.apy_high;
  // Show a range when the band is present and non-degenerate; the projection is
  // a model estimate, so the range — not a single number — is the honest read.
  const hasBand = lo != null && hi != null && hi - lo > 1e-4;

  return (
    <button className="vault-tile" onClick={onSelect} tabIndex={active ? 0 : -1}>
      <div className="vault-tile__head">
        <AssetGlyph asset={vault.underlying_symbol} />
        <div className="vault-tile__title">
          <div className="vault-tile__sym">{vault.underlying_symbol}</div>
          <div className="vault-tile__sub">
            covered call{vault.round_ms ? ` · ${fmtCadence(vault.round_ms)}` : ""}
          </div>
        </div>
        {vault.tvl != null && (
          <div className="vault-tile__tvl">
            <div className="vault-tile__tvl-val">{formatPrice(vault.tvl, { grouping: true })}</div>
            <div className="vault-tile__tvl-label">TVL · {vault.underlying_symbol}</div>
          </div>
        )}
      </div>

      <VaultSparkline realized={realizedSeries} predicted={predictedSeries} />

      <div className="vault-tile__apys">
        <div className="vault-tile__apy">
          <div className="vault-tile__apy-label">Realized APY</div>
          <div className="vault-tile__apy-val is-pos">{fmtPct(realized)}</div>
        </div>
        <div className="vault-tile__apy">
          <div className="vault-tile__apy-label">Projected APY</div>
          <div className={`vault-tile__apy-val${hasBand ? " vault-tile__apy-val--band" : ""}`}>
            {hasBand ? `${fmtPct(lo)} – ${fmtPct(hi)}` : fmtPct(projected)}
          </div>
          {hasBand && <div className="vault-tile__apy-band">mid {fmtPct(projected)}</div>}
        </div>
      </div>

      <span className="vault-tile__cta">View vault →</span>
    </button>
  );
}

function VaultDetail({ vaultId, onBack }: { vaultId: string; onBack: () => void }) {
  const vaultQ = useVault(vaultId);
  const roundsQ = useVaultRounds(vaultId);
  const apyQ = useVaultApyHistory(vaultId);

  if (vaultQ.isLoading || !vaultQ.data) {
    return (
      <>
        <button className="vault-back" onClick={onBack}>← All vaults</button>
        <div className="vault-note">Loading vault…</div>
      </>
    );
  }
  const vault = vaultQ.data;
  const rounds = roundsQ.data ?? [];

  return (
    <>
      <div className="vault-detail__bar">
        <button className="vault-back" onClick={onBack}>← All vaults</button>
        <StrategyInfo vault={vault} />
      </div>
      <VaultStats vault={vault} rounds={rounds} />
      <div className="vault-grid">
        <div className="vault-grid__main">
          <VaultApyChart
            realized={apyQ.data?.realized ?? []}
            predicted={apyQ.data?.predicted ?? []}
            loading={apyQ.isLoading}
          />
          <CurrentRoundCard vault={vault} rounds={rounds} />
          <TrackRecord vault={vault} rounds={rounds} />
        </div>
        <div className="vault-grid__side">
          <InvestPanel vault={vault} />
          <ParamsCard vault={vault} />
        </div>
      </div>
    </>
  );
}

function VaultStats({ vault, rounds }: { vault: Vault; rounds: VaultRound[] }) {
  const sharePositionValue = useMyPositionValue(vault);
  const finalized = rounds.filter((r) => r.pps != null).length;
  return (
    <div className="vault-stats">
      <div className="vault-stat vault-stat--hero">
        <div className="vault-stat__label">Net APY</div>
        <div className="vault-stat__val is-pos">{vault.apy != null ? fmtPct(vault.apy) : "—"}</div>
        <div className="vault-stat__sub">
          {finalized >= 2 ? "annualized, last round" : "needs 2 finalized rounds"}
        </div>
      </div>
      <div className="vault-stat">
        <div className="vault-stat__label">TVL</div>
        <div className="vault-stat__val">
          {vault.tvl != null ? formatPrice(vault.tvl, { grouping: true }) : "—"}
          <span className="vault-stat__unit"> {vault.underlying_symbol}</span>
        </div>
        <div className="vault-stat__sub">total value locked</div>
      </div>
      <div className="vault-stat">
        <div className="vault-stat__label">Price / share</div>
        <div className="vault-stat__val">
          {vault.pps != null ? vault.pps.toFixed(6) : "1.000000"}
          <span className="vault-stat__unit"> {vault.underlying_symbol}</span>
        </div>
        <div className="vault-stat__sub">round {vault.round}</div>
      </div>
      <div className="vault-stat">
        <div className="vault-stat__label">Your position</div>
        <div className="vault-stat__val">
          {sharePositionValue != null ? formatPrice(sharePositionValue, { grouping: true }) : "—"}
          <span className="vault-stat__unit"> {vault.underlying_symbol}</span>
        </div>
        <div className="vault-stat__sub">value of held shares</div>
      </div>
    </div>
  );
}

// Compact "How this vault works" affordance: a pill in the detail toolbar that
// opens the strategy explainer in a centered modal, so the long-form copy is one
// tap away without permanently occupying a column slot. The modal is portaled to
// <body> so its backdrop dims the whole app (header included) rather than sitting
// inside the page's themed/stacked container.
function StrategyInfo({ vault }: { vault: Vault }) {
  const [open, setOpen] = useState(false);

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

  return (
    <div className="vault-howto">
      <button
        className="vault-howto__trigger"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
      >
        <span className="vault-howto__icon" aria-hidden>i</span>
        How this vault works
      </button>
      {open &&
        createPortal(
          // display:contents → carries the aqua theme variables (incl. dark-mode
          // swaps) to the children without generating a box or stacking context.
          <div data-theme="aqua" style={{ display: "contents" }}>
            <div className="vault-modal__scrim" onClick={() => setOpen(false)} />
            <div className="vault-modal" role="dialog" aria-modal="true" aria-label="How this vault works">
              <div className="vault-modal__head">
                <span>How this vault works</span>
                <button
                  className="vault-modal__close"
                  onClick={() => setOpen(false)}
                  aria-label="Close"
                >
                  ×
                </button>
              </div>
              <div className="vault-modal__body vault-prose">
                <p>
                  Each round, the vault writes covered calls on its {vault.underlying_symbol} against{" "}
                  {vault.settlement_symbol}. It sells the calls to market makers via on-chain RFQ auctions
                  and collects the premium up front.
                </p>
                <ol>
                  <li><b>Deposit</b> {vault.underlying_symbol} — it's queued for the next round (never exposed to the round already running).</li>
                  <li>When the round finalizes, your deposit mints <b>shares</b> at that round's price-per-share. Claim them anytime.</li>
                  <li>The vault's price-per-share rises by the net premium each successful round; that's your yield.</li>
                  <li>To exit, <b>initiate a withdrawal</b> (escrows shares for the current round's P&L), then <b>complete</b> it once the round finalizes.</li>
                </ol>
                <p className="vault-prose__muted">
                  Covered calls cap upside above the strike: in a sharp rally the vault may be assigned and
                  forgo gains beyond the strike, keeping the premium. Principal is exposed to{" "}
                  {vault.underlying_symbol} price like any spot holding.
                </p>
              </div>
            </div>
          </div>,
          document.body,
        )}
    </div>
  );
}

function CurrentRoundCard({ vault, rounds }: { vault: Vault; rounds: VaultRound[] }) {
  const current = rounds.find((r) => r.round === vault.round);
  // Phase is ground-truth from the live on-chain read (heuristic fallback).
  const phase =
    vault.phase === "active" ? "Active — selling / holding" : "Settling — between rounds";
  const uDec = vault.underlying_decimals;
  const sDec = vault.settlement_decimals;
  // Live round state arrives from the detail endpoint's sui_getObject read;
  // it's absent on a degraded read, so each row falls back to "—".
  const hasLive = vault.open_rfqs != null || vault.deployable_raw != null;
  return (
    <div className="vault-card">
      <div className="vault-card__head">
        
        Current round · #{vault.round}
      </div>
      <div className="vault-kv">
        <div className="vault-kv__row">
          <span>Status</span>
          <span>{phase}</span>
        </div>
        <div className="vault-kv__row">
          <span>Strike</span>
          <span>
            {current?.strike != null ? `$${formatPrice(current.strike, { grouping: true })}` : "—"}
          </span>
        </div>
        <div className="vault-kv__row">
          <span>Round ends</span>
          <span>{current?.expiry_ms != null ? `~${fmtDate(current.expiry_ms)}` : "—"}</span>
        </div>
        {vault.phase === "active" && (
          <div className="vault-kv__row">
            <span>Selling window ends</span>
            <span>{fmtDateTime(vault.selling_ends_ms)}</span>
          </div>
        )}
        <div className="vault-kv__row">
          <span>Open RFQs</span>
          <span>{vault.open_rfqs != null ? vault.open_rfqs : "—"}</span>
        </div>
        <div className="vault-kv__row">
          <span>Deposits</span>
          <span>{vault.deposits_paused ? "Paused" : "Open"}</span>
        </div>
        {hasLive && (
          <>
            <div className="vault-kv__row">
              <span>Deployable</span>
              <span>{fmtAmt(vault.deployable_raw, uDec, vault.underlying_symbol)}</span>
            </div>
            <div className="vault-kv__row">
              <span>Proceeds awaiting swap</span>
              <span>{fmtAmt(vault.proceeds_settlement_raw, sDec, vault.settlement_symbol)}</span>
            </div>
            <div className="vault-kv__row">
              <span>Withdrawal pool</span>
              <span>{fmtAmt(vault.withdrawal_pool_raw, uDec, vault.underlying_symbol)}</span>
            </div>
            <div className="vault-kv__row">
              <span>Claimable shares</span>
              <span>{fmtAmt(vault.claimable_shares_raw, uDec, "shares")}</span>
            </div>
            <div className="vault-kv__row">
              <span>Queued withdrawals</span>
              <span>{fmtAmt(vault.queued_withdraw_shares_raw, uDec, "shares")}</span>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function TrackRecord({ vault, rounds }: { vault: Vault; rounds: VaultRound[] }) {
  const finalized = rounds.filter((r) => r.pps != null).sort((a, b) => b.round - a.round);
  const rfqsQ = useVaultRfqs(vault.vault_id);
  // bucket_id → its settled RFQs; a round may slice inventory into several.
  const byBucket = new Map<string, VaultRfq[]>();
  for (const r of rfqsQ.data ?? []) {
    if (r.status !== "settled") continue;
    const list = byBucket.get(r.bucket_id) ?? [];
    list.push(r);
    byBucket.set(r.bucket_id, list);
  }
  return (
    <div className="vault-card">
      <div className="vault-card__head">
        
        Track record
      </div>
      {finalized.length === 0 ? (
        <div className="vault-card__body vault-prose__muted">
          No finalized rounds yet — the first round's results will appear here.
        </div>
      ) : (
        <div className="vault-table">
          <div className="vault-table__scroll">
            <div className="vault-table__head">
              <span>Round</span>
              <span>Strike</span>
              <span>Expiry</span>
              <span>PPS</span>
              <span>Premium (net)</span>
            </div>
            {finalized.map((r) => (
              <RoundRow
                key={r.round}
                round={r}
                vault={vault}
                rfqs={r.bucket_id ? byBucket.get(r.bucket_id) ?? [] : []}
              />
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

/** One track-record row; expands to the round's RFQ gross/fee + bid history. */
function RoundRow({ round, vault, rfqs }: { round: VaultRound; vault: Vault; rfqs: VaultRfq[] }) {
  const [open, setOpen] = useState(false);
  const sDec = vault.settlement_decimals;
  const net = scaled(round.premium_collected_raw, sDec);
  const sumRaw = (pick: (r: VaultRfq) => string | null) =>
    rfqs.reduce((a, r) => a + BigInt(pick(r) ?? "0"), 0n);
  const hasRfqs = rfqs.length > 0;
  const gross = hasRfqs && sDec != null ? Number(sumRaw((r) => r.gross_premium_raw)) / 10 ** sDec : null;
  const fee = hasRfqs && sDec != null ? Number(sumRaw((r) => r.fee_raw)) / 10 ** sDec : null;
  return (
    <>
      <div
        className="vault-table__row"
        onClick={hasRfqs ? () => setOpen((o) => !o) : undefined}
        style={hasRfqs ? { cursor: "pointer" } : undefined}
      >
        <span>{hasRfqs ? (open ? "▾ " : "▸ ") : ""}#{round.round}</span>
        <span>{round.strike != null ? `$${formatPrice(round.strike)}` : "—"}</span>
        <span>{fmtDate(round.expiry_ms)}</span>
        <span>{round.pps != null ? round.pps.toFixed(6) : "—"}</span>
        <span className="is-pos">
          {net != null ? `+${formatPrice(net)} ${vault.settlement_symbol}` : "—"}
          {gross != null && (
            <span className="vault-bids__sub">
              {" "}
              gross {formatPrice(gross)} · fee {formatPrice(fee ?? 0)}
            </span>
          )}
        </span>
      </div>
      {open && (
        <div className="vault-bids">
          {rfqs.map((r) => (
            <RfqBidList key={r.rfq_id} rfq={r} vault={vault} />
          ))}
        </div>
      )}
    </>
  );
}

/** Bid ladder for one settled RFQ slice (lazy — fetched on expand). */
function RfqBidList({ rfq, vault }: { rfq: VaultRfq; vault: Vault }) {
  const sDec = vault.settlement_decimals;
  const bidsQ = useRfqBids(rfq.rfq_id, true);
  const bids = bidsQ.data ?? [];
  return (
    <div className="vault-bids__group">
      <div className="vault-bids__title">
        Auction {shortHex(rfq.rfq_id)} · {bids.length} bid{bids.length === 1 ? "" : "s"}
      </div>
      {bidsQ.isLoading && <div className="vault-bids__bid vault-prose__muted">loading bids…</div>}
      {!bidsQ.isLoading && bids.length === 0 && (
        <div className="vault-bids__bid vault-prose__muted">no bids recorded</div>
      )}
      {bids.map((b) => {
        const premium = scaled(b.premium_raw, sDec);
        return (
          <div className="vault-bids__bid" key={b.sequence}>
            <span>{shortHex(b.bidder)}</span>
            <span className="is-pos">
              {premium != null ? `${formatPrice(premium)} ${vault.settlement_symbol}` : "—"}
            </span>
          </div>
        );
      })}
    </div>
  );
}

function ParamsCard({ vault }: { vault: Vault }) {
  const band =
    vault.min_strike_over_spot_pct != null && vault.max_strike_over_spot_pct != null
      ? `+${fmtPctRaw(vault.min_strike_over_spot_pct)} to +${fmtPctRaw(vault.max_strike_over_spot_pct)} over spot`
      : "—";
  return (
    <div className="vault-card">
      <div className="vault-card__head">
        
        Parameters & fees
      </div>
      <div className="vault-kv">
        <div className="vault-kv__row">
          <span>Management fee</span>
          <span>{vault.mgmt_fee_pct != null ? `${fmtPctRaw(vault.mgmt_fee_pct)} / yr` : "—"}</span>
        </div>
        <div className="vault-kv__row">
          <span>Performance fee</span>
          <span>
            {vault.perf_fee_pct != null ? `${fmtPctRaw(vault.perf_fee_pct, 0)} of premium` : "—"}
          </span>
        </div>
        <div className="vault-kv__row">
          <span>Strike band</span>
          <span>{band}</span>
        </div>
        <div className="vault-kv__row">
          <span>Round cadence</span>
          <span>{fmtCadence(vault.round_ms)}</span>
        </div>
        <div className="vault-kv__row">
          <span>Max RFQ slice</span>
          <span>{fmtAmt(vault.max_slice_amount_raw, vault.underlying_decimals, vault.underlying_symbol)}</span>
        </div>
        <div className="vault-kv__row">
          <span>Max open RFQs</span>
          <span>{vault.max_open_rfqs != null ? vault.max_open_rfqs : "—"}</span>
        </div>
        <div className="vault-kv__row">
          <span>Fees to date</span>
          <span>
            {vault.total_fees != null
              ? `${formatPrice(vault.total_fees, { grouping: true })} ${vault.underlying_symbol}`
              : "—"}
          </span>
        </div>
      </div>
      <div className="vault-card__foot vault-prose__muted">
        From the vault's on-chain <code>VaultConfig</code>. This vault is uncapped.
      </div>
    </div>
  );
}

// Position value (held shares × pps), in display underlying units. Shares are
// denominated in the underlying's decimals (1 share == 1 underlying at pps 1.0).
function useMyPositionValue(vault: Vault): number | null {
  const account = useCurrentAccount();
  const balQ = useShareBalance(account?.address ?? null, vault.share_type);
  if (balQ.data == null || vault.underlying_decimals == null || vault.pps == null) return null;
  const shares = Number(balQ.data) / 10 ** vault.underlying_decimals;
  return shares * vault.pps;
}

function InvestPanel({ vault }: { vault: Vault }) {
  const account = useCurrentAccount();
  const address = account?.address ?? null;
  const actions = useVaultActions();
  const [tab, setTab] = useState<"deposit" | "withdraw">("deposit");
  const [amount, setAmount] = useState("");

  const receiptsQ = useOwnedVaultReceipts(address, vault.vault_id);
  const shareBalQ = useShareBalance(address, vault.share_type);

  const uDec = vault.underlying_decimals;
  const amountNum = Number(amount) || 0;
  const shareDisplay = uDec != null && shareBalQ.data != null ? Number(shareBalQ.data) / 10 ** uDec : 0;

  if (!address) {
    return (
      <div className="vault-card vault-invest">
        <div className="vault-card__head">Invest</div>
        <div className="vault-card__body vault-prose__muted">
          Connect a wallet to deposit into this vault.
        </div>
      </div>
    );
  }

  const onSubmit = () => {
    if (uDec == null) return;
    if (tab === "deposit") {
      if (amountNum <= 0) return;
      actions.deposit(vault, toRaw(amountNum, uDec));
    } else {
      if (amountNum <= 0) return;
      actions.initiateWithdraw(vault, toRaw(amountNum, uDec));
    }
    setAmount("");
  };

  const deposits = receiptsQ.data?.deposits ?? [];
  const withdraws = receiptsQ.data?.withdraws ?? [];

  return (
    <div className="vault-card vault-invest">
      <div className="vault-invest__tabs">
        <button className={"vault-invest__tab" + (tab === "deposit" ? " is-active" : "")} onClick={() => setTab("deposit")}>Deposit</button>
        <button className={"vault-invest__tab" + (tab === "withdraw" ? " is-active" : "")} onClick={() => setTab("withdraw")}>Withdraw</button>
      </div>

      <div className="vault-invest__field">
        <input
          className="amount__input"
          type="number"
          min="0"
          placeholder="0.0"
          value={amount}
          onChange={(e) => setAmount(e.target.value)}
        />
        <span className="vault-invest__unit">
          {tab === "deposit" ? vault.underlying_symbol : "shares"}
        </span>
      </div>
      <div className="vault-invest__bal">
        {tab === "withdraw"
          ? `${shareDisplay.toFixed(4)} shares available`
          : vault.deposits_paused
            ? "deposits are paused"
            : "deposited funds enter next round"}
      </div>

      <button
        className="vault-invest__cta"
        disabled={!!actions.busy || amountNum <= 0 || (tab === "deposit" && vault.deposits_paused) || uDec == null}
        onClick={onSubmit}
      >
        {actions.busy
          ? `${actions.busy}…`
          : tab === "deposit"
            ? `Deposit ${vault.underlying_symbol}`
            : "Initiate withdrawal"}
      </button>

      {/* Outstanding receipts: claim shares / complete withdrawal / cancel. */}
      {(deposits.length > 0 || withdraws.length > 0) && (
        <div className="vault-invest__receipts">
          <div className="vault-invest__receipts-head">Your receipts</div>
          {deposits.map((r) => {
            const claimable = vault.round >= r.round; // pps[round-1] exists
            const cancellable = r.round > vault.round; // round hasn't started
            const amt = scaled(r.amount_raw, uDec);
            return (
              <div className="vault-receipt" key={r.object_id}>
                <span>Deposit · round {r.round} · {amt != null ? amt.toFixed(4) : "?"} {vault.underlying_symbol}</span>
                {claimable ? (
                  <button disabled={!!actions.busy} onClick={() => actions.claim(vault, r.object_id)}>Claim shares</button>
                ) : cancellable ? (
                  <button disabled={!!actions.busy} onClick={() => actions.cancelDeposit(vault, r.object_id)}>Cancel</button>
                ) : (
                  <span className="vault-receipt__pending">pending</span>
                )}
              </div>
            );
          })}
          {withdraws.map((r) => {
            const payable = vault.round > r.round; // round r finalized
            const amt = scaled(r.amount_raw, uDec);
            return (
              <div className="vault-receipt" key={r.object_id}>
                <span>Withdraw · round {r.round} · {amt != null ? amt.toFixed(4) : "?"} shares</span>
                {payable ? (
                  <button disabled={!!actions.busy} onClick={() => actions.completeWithdraw(vault, r.object_id)}>Complete</button>
                ) : (
                  <span className="vault-receipt__pending">finalizing</span>
                )}
              </div>
            );
          })}
        </div>
      )}

      {actions.toast && <Toast message={actions.toast.message} variant={actions.toast.variant} />}
    </div>
  );
}
