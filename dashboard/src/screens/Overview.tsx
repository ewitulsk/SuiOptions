// Overview — "is the bot doing its job": health strip, vault basics,
// desk exposure snapshot, P&L attribution.

import { enabledState, useDeskState } from "../api/deskState";
import { fmtAmount, fmtPct, fmtRaw, fmtSigned, fromRaw, timeAgo } from "../api/format";
import { useVaultFlows } from "../api/indexer";
import { useVaultDetail, vaultTvlRaw } from "../api/vault";
import { Card, Empty, ErrorNote, Meter, Pill, Tile, type PillTone } from "../components/ui";
import { DeskDownBanner } from "../components/DeskDownBanner";

export function Overview() {
  const desk = useDeskState();
  const state = enabledState(desk.data);
  const vault = useVaultDetail(state?.vault.vaultId);
  const flows = useVaultFlows(state?.vault.vaultId);

  if (desk.isError || (desk.data && !desk.data.enabled) || (desk.isLoading && !state)) {
    return <DeskDownBanner query={desk} />;
  }
  if (!state) return <Empty>Loading desk state…</Empty>;

  const dec = state.vault.settlementDecimals;
  const settle = (raw: number | null | undefined) =>
    raw == null ? "—" : fmtAmount(fromRaw(raw, dec));

  const income = state.pnl.spread + state.pnl.scalp;
  const cost = Math.max(0, -state.pnl.theta) + Math.max(0, -state.pnl.funding);
  const netDeposits =
    flows.data != null ? flows.data.depositedRaw - flows.data.withdrawnRaw : null;
  const lpPnlRaw =
    vault.data?.latestNavRaw != null && netDeposits != null
      ? Number(vault.data.latestNavRaw) - netDeposits
      : null;

  return (
    <div className="dash-grid">
      <Card title="Health" sub={`booted ${timeAgo(state.bootedAtMs)} · ${state.network}`}>
        <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
          <Pill tone="ok">desk enabled</Pill>
          <Pill tone={state.exposure.killSwitch ? "bad" : "ok"}>
            kill switch {state.exposure.killSwitch ? "LATCHED" : "clear"}
          </Pill>
          <Pill tone={state.exposure.stressBlocked ? "bad" : "ok"}>
            stress gate {state.exposure.stressBlocked ? "BLOCKING" : "clear"}
          </Pill>
          <Pill
            tone={state.vault.curatorSessionFlowsEnabled ? "ok" : "warn"}
            title="CuratorCap + IntegrationRegistry resolved — gates vault-funded bids and vault-custody exits"
          >
            curator flows {state.vault.curatorSessionFlowsEnabled ? "on" : "DEGRADED"}
          </Pill>
          <Pill tone={state.vault.mmReleaseEnabled ? "ok" : "warn"}>
            vault_mm release {state.vault.mmReleaseEnabled ? "on" : "off"}
          </Pill>
          {state.markets.map((m) => {
            const age = m.spotAtMs ? Date.now() - m.spotAtMs : null;
            const tone: PillTone = age == null ? "bad" : age > 5 * 60_000 ? "warn" : "ok";
            return (
              <Pill key={m.symbol} tone={tone}>
                {m.symbol} spot {age == null ? "missing" : timeAgo(m.spotAtMs)}
                {m.surfaceIsFallback ? " · fallback vol" : ""}
              </Pill>
            );
          })}
          <Pill tone={state.config.auctionsEnabled ? "ok" : "warn"}>
            auctions {state.config.auctionsEnabled ? "on" : "off"}
          </Pill>
          <Pill tone={state.config.exitsEnabled ? "ok" : "warn"}>
            exits {state.config.exitsEnabled ? "on" : "off"}
          </Pill>
        </div>
      </Card>

      <Card title="Vault" sub={state.vault.vaultId} span="half">
        {vault.isError && <ErrorNote error={vault.error} what="vault detail" />}
        {vault.data && (
          <div className="dash-tiles">
            <Tile
              label="TVL"
              value={fmtAmount(fromRaw(vaultTvlRaw(vault.data), dec))}
              hint="shares × pps"
            />
            <Tile
              label="NAV (appraised)"
              value={fmtRaw(vault.data.latestNavRaw, dec)}
              hint={`as of ${timeAgo(vault.data.navUpdatedAtMs)}`}
            />
            <Tile
              label="Share price"
              value={
                vault.data.latestPpsE12Raw == null
                  ? "—"
                  : (Number(vault.data.latestPpsE12Raw) / 1e12).toFixed(4)
              }
            />
            <Tile
              label="LP P&L"
              value={
                lpPnlRaw == null ? (
                  "—"
                ) : (
                  <span className={lpPnlRaw >= 0 ? "pos" : "neg"}>
                    {fmtSigned(fromRaw(lpPnlRaw, dec))}
                  </span>
                )
              }
              hint={
                flows.data?.truncated
                  ? "NAV − net deposits (event window truncated)"
                  : "NAV − net deposits"
              }
            />
            <Tile label="Pending withdrawals" value={vault.data.pendingWithdrawals} />
            <Tile
              label="State"
              value={vault.data.state}
              hint={vault.data.depositsPaused ? "deposits paused" : undefined}
            />
          </div>
        )}
      </Card>

      <Card title="Desk exposure" sub="mark-to-model, settlement units" span="half">
        <div className="dash-tiles" style={{ marginBottom: 12 }}>
          <Tile label="NAV (desk)" value={settle(state.exposure.nav)} />
          <Tile label="Premium deployed" value={settle(state.exposure.premiumDeployed)} />
          <Tile
            label="Reserved"
            value={settle(state.exposure.reserved)}
            hint={`${state.reservations.count} live quotes`}
          />
          <Tile
            label="Net vega / vol pt"
            value={fmtSigned(fromRaw(state.exposure.netVegaPerVolpt, dec))}
          />
          <Tile label="Theta cost / day" value={settle(state.exposure.thetaCostPerDay)} />
          <Tile
            label="Funding (weighted)"
            value={fmtPct(state.fundingRateAnnual)}
            hint="annualized; positive = shorts earn"
          />
        </div>
        <Meter label="Premium budget" value={state.utilization.premium} detail="vs soft 30%" />
        <Meter label="Vega cap" value={state.utilization.vega} />
        <Meter label="Theta governor" value={state.utilization.theta} detail="vs soft" />
      </Card>

      <Card
        title="P&L attribution"
        sub="cumulative since boot, settlement units — durable history on the History tab"
        span="half"
      >
        <div className="dash-tiles">
          {(
            [
              ["Spread", state.pnl.spread],
              ["Scalp", state.pnl.scalp],
              ["Theta", state.pnl.theta],
              ["Funding", state.pnl.funding],
              ["Total", state.pnl.total],
            ] as const
          ).map(([label, v]) => (
            <Tile
              key={label}
              label={label}
              value={
                <span className={v >= 0 ? "pos" : "neg"}>{fmtSigned(fromRaw(v, dec))}</span>
              }
            />
          ))}
        </div>
        <div className="dash-note">
          Bleed check: income (spread + scalp) {settle(income)} vs cost (theta + funding paid){" "}
          {settle(cost)} —{" "}
          {income >= cost ? (
            <span className="pos">covering the bleed</span>
          ) : (
            <span className="neg">bleeding (bids too high or bands wrong)</span>
          )}
          . The strategy target is (scalping + spread) ≥ (theta + funding) over a rolling
          window.
        </div>
      </Card>

      <Card title="Nightly stress" sub="model revaluation of the live book" span="half">
        {state.stress == null ? (
          <Empty>No stress run yet this boot.</Empty>
        ) : (
          <div className="dash-tiles">
            <Tile label="Gap −60%" value={fmtPct(state.stress.gapDown60)} hint="NAV drawdown" />
            <Tile label="Gap +80%" value={fmtPct(state.stress.gapUp80)} />
            <Tile label="Flat 6 months" value={fmtPct(state.stress.flat6mo)} hint="theta only" />
            <Tile label="Funding −50%" value={fmtPct(state.stress.fundingMinus50)} />
            <Tile
              label="Worst"
              value={
                <span className={state.stress.blocked ? "neg" : "pos"}>
                  {fmtPct(state.stress.worstDrawdown)}
                </span>
              }
              hint={`ran ${timeAgo(state.stress.atMs)}${state.stress.blocked ? " · gate BLOCKING" : ""}`}
            />
          </div>
        )}
      </Card>
    </div>
  );
}
