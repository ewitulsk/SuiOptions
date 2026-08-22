// Exposures — every risk number, what contributes to it, and how close
// each one is to its limit.

import { Fragment, useState } from "react";

import {
  enabledState,
  useDeskState,
  type DeskHolding,
  type DeskState,
  type DeskWritten,
} from "../api/deskState";
import { fmtAmount, fmtExpiry, fmtPct, fmtSigned, fromRaw } from "../api/format";
import { Card, Empty, Meter, Pill, Tile } from "../components/ui";
import { DeskDownBanner } from "../components/DeskDownBanner";

export function Exposures() {
  const desk = useDeskState();
  const state = enabledState(desk.data);
  if (desk.isError || (desk.data && !desk.data.enabled) || (desk.isLoading && !state)) {
    return <DeskDownBanner query={desk} />;
  }
  if (!state) return <Empty>Loading desk state…</Empty>;
  return (
    <div className="dash-grid">
      <LimitsCard state={state} />
      <DeltaCard state={state} />
      <ExpiryCard state={state} />
      <ConcentrationCard state={state} />
    </div>
  );
}

function LimitsCard({ state }: { state: DeskState }) {
  const dec = state.vault.settlementDecimals;
  const l = state.limits;
  return (
    <Card
      title="Limits & utilization"
      sub="continuous utilization vs the SOFT limits — the desk widens quotes as these fill, and hard-declines past the hard caps"
      span="half"
    >
      <Meter
        label={`Premium budget (soft ${fmtPct(l.premium_budget_soft, 0)} / hard ${fmtPct(l.premium_budget_hard, 0)} of NAV)`}
        value={state.utilization.premium}
        detail={`${fmtAmount(fromRaw(state.exposure.premiumDeployed + state.exposure.reserved, dec))} deployed+reserved`}
      />
      <Meter
        label={`Net vega cap (${fmtPct(l.vega_cap_nav_per_volpt, 2)} NAV / vol pt)`}
        value={state.utilization.vega}
        detail={`${fmtSigned(fromRaw(state.exposure.netVegaPerVolpt, dec))} per vol pt`}
      />
      <Meter
        label={`Theta governor (soft ${fmtPct(l.theta_soft_nav_per_day, 2)} / hard ${fmtPct(l.theta_hard_nav_per_day, 2)} NAV/day)`}
        value={state.utilization.theta}
        detail={`${fmtAmount(fromRaw(state.exposure.thetaCostPerDay, dec))} / day`}
      />
      <div className="dash-tiles" style={{ marginTop: 14 }}>
        <Tile
          label="Total greeks"
          value={fmtSigned(state.greeks.total.deltaUnits / 10 ** underlyingDecimals(state), 3)}
          hint="net delta, underlying units"
        />
        <Tile label="Gamma (units)" value={fmtSigned(state.greeks.total.gammaUnits, 4)} />
        <Tile
          label="Theta / day"
          value={fmtSigned(fromRaw(state.greeks.total.thetaPerDay, dec))}
          hint="negative = decay cost"
        />
        <Tile
          label="Naked written"
          value={fmtAmount(state.nakedWrittenUnits)}
          hint="raw units · V2 short budget"
        />
        <Tile
          label="Kill drawdown"
          value={fmtPct(state.limits.kill_drawdown, 0)}
          hint={`over ${state.limits.kill_window_days}d window`}
        />
      </div>
    </Card>
  );
}

/** Decimals of the first market — for coarse delta display only. */
function underlyingDecimals(state: DeskState): number {
  return state.markets[0]?.decimals ?? 0;
}

function DeltaCard({ state }: { state: DeskState }) {
  return (
    <Card
      title="Delta vs hedge, per underlying"
      sub={`rebalances when |net| leaves the band (±${state.hedge.bandPctNav}% NAV; ${state.hedge.bandWidePctNav}% when funding < ${fmtPct(state.hedge.fundingWidenThreshold, 0)})`}
      span="half"
    >
      {state.hedge.bySymbol.length === 0 && <Empty>No markets.</Empty>}
      {state.hedge.bySymbol.map((s) => {
        const m = state.markets.find((mk) => mk.symbol === s.symbol);
        const dec = m?.decimals ?? 0;
        const scale = (v: number) => fmtSigned(v / 10 ** dec, 4);
        const bandUse =
          s.bandUnits != null && s.bandUnits > 0 ? Math.abs(s.netUnits) / s.bandUnits : 0;
        return (
          <div key={s.symbol} className="dash-meter">
            <div className="dash-meter__row">
              <span>
                <b style={{ fontFamily: "inherit" }}>{s.symbol}</b>
                {"  "}book {scale(s.bookDeltaUnits)} · hedge {scale(s.hedgeUnits)} ·
                net {scale(s.netUnits)}
              </span>
              <b>
                {s.bandUnits == null
                  ? "band —"
                  : `${(bandUse * 100).toFixed(0)}% of band ±${fmtAmount(s.bandUnits / 10 ** dec, 4)}`}
              </b>
            </div>
            <div className="dash-meter__track">
              <div
                className={`dash-meter__fill ${bandUse >= 1 ? "dash-meter__fill--bad" : bandUse >= 0.7 ? "dash-meter__fill--warn" : ""}`}
                style={{ width: `${Math.min(100, bandUse * 100)}%` }}
              />
            </div>
          </div>
        );
      })}
      <div className="dash-note">
        The hedge is the primary revenue engine: rebalancing a long-gamma book systematically
        sells high and buys low. Hedge shorts run on the venue roster (Venues tab).
      </div>
    </Card>
  );
}

function ExpiryCard({ state }: { state: DeskState }) {
  const [open, setOpen] = useState<number | null>(null);
  const dec = state.vault.settlementDecimals;
  const nav = state.exposure.nav;
  const perExpiryCap = state.limits.per_expiry_max * nav;
  const rows = state.greeks.byExpiry;
  return (
    <Card
      title="Per-expiry exposure"
      sub={`net greeks and premium concentration (cap ${fmtPct(state.limits.per_expiry_max, 0)} of NAV per expiry) — click a row for the contributing positions`}
    >
      {rows.length === 0 ? (
        <Empty>No positions on the book.</Empty>
      ) : (
        <div className="dash-table-wrap">
          <table className="dash-table">
            <thead>
              <tr>
                <th>Expiry</th>
                <th className="num">Premium</th>
                <th className="num">of cap</th>
                <th className="num">Delta (units)</th>
                <th className="num">Gamma</th>
                <th className="num">Vega</th>
                <th className="num">Theta/day</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => {
                const premium = state.exposure.premiumByExpiry[String(r.expiryMs)] ?? 0;
                const contributors = [
                  ...state.holdings.filter((h) => h.expiryMs === r.expiryMs),
                  ...state.written.filter((w) => w.expiryMs === r.expiryMs),
                ];
                return (
                  <Fragment key={r.expiryMs}>
                    <tr
                      className="expandable"
                      onClick={() => setOpen(open === r.expiryMs ? null : r.expiryMs)}
                    >
                      <td>{fmtExpiry(r.expiryMs)}</td>
                      <td className="num">{fmtAmount(fromRaw(premium, dec))}</td>
                      <td className="num">
                        {perExpiryCap > 0 ? fmtPct(premium / perExpiryCap, 0) : "—"}
                      </td>
                      <td className="num">{fmtSigned(r.deltaUnits, 2)}</td>
                      <td className="num">{fmtSigned(r.gammaUnits, 4)}</td>
                      <td className="num">{fmtSigned(fromRaw(r.vega, dec))}</td>
                      <td className="num">{fmtSigned(fromRaw(r.thetaPerDay, dec))}</td>
                    </tr>
                    {open === r.expiryMs &&
                      contributors.map((c) => <ContributorRow key={rowKey(c)} c={c} state={state} />)}
                  </Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </Card>
  );
}

function rowKey(c: DeskHolding | DeskWritten): string {
  return "positionId" in c ? `w-${c.positionId}` : `h-${c.bucketId}`;
}

function ContributorRow({ c, state }: { c: DeskHolding | DeskWritten; state: DeskState }) {
  const dec = state.vault.settlementDecimals;
  const written = "positionId" in c;
  const g = c.mark?.greeksPerUnit;
  const sign = written ? -1 : 1;
  return (
    <tr className="subrow">
      <td>
        {written ? "short (written)" : "long (held)"} {c.symbol ?? "?"}{" "}
        {c.isPut ? "PUT" : "CALL"} @ {fmtAmount(c.strikeScaled, 4)}
      </td>
      <td className="num">
        {c.mark ? fmtAmount(fromRaw(sign * c.mark.value, dec)) : "—"}
      </td>
      <td className="num muted">amt {fmtAmount(c.amount)}</td>
      <td className="num">{g ? fmtSigned(sign * g.delta * c.amount, 2) : "—"}</td>
      <td className="num">{g ? fmtSigned(sign * g.gamma * c.amount, 4) : "—"}</td>
      <td className="num">{g ? fmtSigned(fromRaw(sign * g.vega * c.amount, dec)) : "—"}</td>
      <td className="num">{g ? fmtSigned(fromRaw(sign * g.theta * c.amount, dec)) : "—"}</td>
    </tr>
  );
}

function ConcentrationCard({ state }: { state: DeskState }) {
  const dec = state.vault.settlementDecimals;
  const nav = state.exposure.nav;
  const cap = state.limits.per_strike_bucket_max * nav;
  const labels = ["< 90% moneyness", "90–110%", "> 110%"];
  return (
    <Card
      title="Strike concentration"
      sub={`premium per moneyness bucket, cap ${fmtPct(state.limits.per_strike_bucket_max, 0)} of NAV each`}
      span="half"
    >
      {state.exposure.premiumByStrikeBucket.map((v, i) => (
        <Meter
          key={labels[i]}
          label={labels[i]}
          value={cap > 0 ? v / cap : 0}
          detail={fmtAmount(fromRaw(v, dec))}
        />
      ))}
      <div style={{ display: "flex", gap: 8, flexWrap: "wrap", marginTop: 8 }}>
        <Pill tone={state.exposure.killSwitch ? "bad" : "ok"}>
          kill switch {state.exposure.killSwitch ? "LATCHED" : "clear"}
        </Pill>
        <Pill tone={state.exposure.stressBlocked ? "bad" : "ok"}>
          stress gate {state.exposure.stressBlocked ? "BLOCKING new short risk" : "clear"}
        </Pill>
      </div>
    </Card>
  );
}
