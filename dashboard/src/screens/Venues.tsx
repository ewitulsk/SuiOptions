// Venues — every hedge venue the desk runs, plus the Bluefin parent
// account: equity, margin, per-position liquidation distance, funding.
// Renders honest placeholder states: the paper venue is badged simulated
// and the Bluefin panel says exactly what is missing until the account
// exists.

import { enabledState, useDeskState, type DeskState } from "../api/deskState";
import {
  annualizedFunding,
  fromE9,
  liqDistance,
  useBluefinAccount,
  useFrostParent,
  useFundingHistory,
} from "../api/bluefin";
import { fmtAmount, fmtPct, fmtRaw, fmtSigned, fromRaw, shortId, timeAgo } from "../api/format";
import { useVaultDetail } from "../api/vault";
import { Card, Empty, ErrorNote, Tile } from "../components/ui";
import { DeskDownBanner } from "../components/DeskDownBanner";
import { SeriesChart } from "../components/SeriesChart";

export function Venues() {
  const desk = useDeskState();
  const state = enabledState(desk.data);
  if (desk.isError || (desk.data && !desk.data.enabled) || (desk.isLoading && !state)) {
    return <DeskDownBanner query={desk} />;
  }
  if (!state) return <Empty>Loading desk state…</Empty>;
  return (
    <div className="dash-grid">
      <RosterCard state={state} />
      <FundMovementCard state={state} />
      <BluefinCard state={state} />
    </div>
  );
}

function RosterCard({ state }: { state: DeskState }) {
  const dec = state.vault.settlementDecimals;
  return (
    <Card
      title="Hedge venue roster"
      sub="per venue × underlying — the first venue executes, the rest are monitored"
    >
      {state.hedge.venues.length === 0 ? (
        <Empty>No hedge venues configured.</Empty>
      ) : (
        <div className="dash-table-wrap">
          <table className="dash-table">
            <thead>
              <tr>
                <th>Venue</th>
                <th>Symbol</th>
                <th className="num">Position (units)</th>
                <th className="num">Notional</th>
                <th className="num">Funding (ann.)</th>
                <th className="num">Margin headroom</th>
                <th className="num">Realized P&L</th>
              </tr>
            </thead>
            <tbody>
              {state.hedge.venues.map((v) => {
                const m = state.markets.find((mk) => mk.symbol === v.symbol);
                const udec = m?.decimals ?? 0;
                return (
                  <tr key={`${v.name}-${v.symbol}`}>
                    <td>
                      {v.name}{" "}
                      {v.simulated && (
                        <span
                          className="dash-badge dash-badge--sim"
                          title="Paper venue: fills simulated at oracle spot ± slippage; funding is a config constant and margin headroom a hard-coded 1.0"
                        >
                          simulated
                        </span>
                      )}
                      {!v.readOk && <span className="neg"> read failed</span>}
                    </td>
                    <td>{v.symbol}</td>
                    <td className="num">{fmtAmount(v.positionUnits / 10 ** udec, 4)}</td>
                    <td className="num">{fmtAmount(fromRaw(v.notional, dec))}</td>
                    <td className="num">{fmtPct(v.fundingRateAnnual)}</td>
                    <td className="num">{v.simulated ? "n/a (paper)" : fmtPct(v.marginHeadroom)}</td>
                    <td className="num">
                      <span className={v.realizedPnl >= 0 ? "pos" : "neg"}>
                        {fmtSigned(fromRaw(v.realizedPnl, dec))}
                      </span>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      <div className="dash-note">
        The paper venue is the only executable venue until <code>HedgeVenue::bluefin</code>{" "}
        lands — delta hedging (the primary revenue engine) is simulated, so the strategy is
        economically unvalidated until then.
      </div>
    </Card>
  );
}

function FundMovementCard({ state }: { state: DeskState }) {
  const vault = useVaultDetail(state.vault.vaultId);
  const dec = state.vault.settlementDecimals;
  const v = vault.data;
  const exposure = v ? Number(v.externalExposure) : null;
  const equity = v?.latestExternalEquity != null ? Number(v.latestExternalEquity) : null;
  const divergence = exposure != null && equity != null ? equity - exposure : null;
  return (
    <Card
      title="Vault ↔ venue fund movement"
      sub="release_external is curator-gated and budgeted on-chain (% of NAV + daily rate limit); returns only from the registered account"
      span="half"
    >
      {vault.isError && <ErrorNote error={vault.error} what="vault detail" />}
      {v &&
        (v.externalAccount == null ? (
          <Empty>
            No external account registered on the vault — no funds can move to a perps venue.
            The registration ceremony (FROST keygen → set_external_account → seed equity) runs
            from the curator panel.
          </Empty>
        ) : (
          <>
            <div className="dash-tiles">
              <Tile label="Registered account" value={shortId(v.externalAccount)} />
              <Tile
                label="Exposure (released − returned)"
                value={fmtRaw(v.externalExposure, dec)}
              />
              <Tile
                label="Attested equity"
                value={fmtRaw(v.latestExternalEquity, dec)}
                hint={`posted ${timeAgo(v.externalEquityUpdatedAtMs)}`}
              />
              <Tile
                label="Equity − exposure"
                value={
                  divergence == null ? (
                    "—"
                  ) : (
                    <span className={divergence >= 0 ? "pos" : "neg"}>
                      {fmtSigned(fromRaw(divergence, dec))}
                    </span>
                  )
                }
                hint="venue P&L above/below released capital"
              />
            </div>
            <div className="dash-note">
              The on-chain budget (max % of NAV, daily release limit) binds at each release
              against a fresh appraisal; surfacing the configured bps here needs an
              api-service field (follow-up). Reconciliation alerts fire as{" "}
              <code>hedge-reconciliation</code> when exposure, releases−sweeps, and venue
              equity diverge.
            </div>
          </>
        ))}
    </Card>
  );
}

function BluefinCard({ state }: { state: DeskState }) {
  const parent = useFrostParent(state.vault.vaultId);
  const account = useBluefinAccount(parent.data?.suiAddress);
  const firstSymbol = account.data?.positions?.[0]?.symbol;
  const funding = useFundingHistory(firstSymbol ?? "SUI-PERP");

  return (
    <Card
      title="Bluefin Pro (parent account)"
      sub="via the hedge-signer relay — read-only"
      span="half"
    >
      {parent.isError && <ErrorNote error={parent.error} what="FROST parent key" />}
      {parent.data === null && (
        <Empty>
          No FROST parent key exists for this vault yet — the Bluefin account setup wizard
          hasn't run. Once it does, equity, margin, and liquidation distances appear here.
        </Empty>
      )}
      {parent.data && account.data === null && (
        <Empty>
          Parent address {shortId(parent.data.suiAddress)} exists but Bluefin has never seen
          it (accounts materialize on first deposit).
        </Empty>
      )}
      {account.isError && <ErrorNote error={account.error} what="Bluefin account" />}
      {account.data && (
        <>
          <div className="dash-tiles" style={{ marginBottom: 12 }}>
            <Tile
              label="Account value"
              value={fmtAmount(fromE9(account.data.totalAccountValueE9))}
              hint="USDC"
            />
            <Tile
              label="Margin available"
              value={fmtAmount(fromE9(account.data.marginAvailableE9 ?? null))}
            />
            <Tile
              label="Margin required"
              value={fmtAmount(fromE9(account.data.crossMarginRequiredE9 ?? null))}
            />
            <Tile
              label="Unrealized P&L"
              value={fmtSigned(fromE9(account.data.totalUnrealizedPnlE9 ?? null))}
            />
            <Tile
              label="Cross leverage"
              value={`${fmtAmount(fromE9(account.data.crossLeverageE9 ?? null))}×`}
            />
          </div>
          {(account.data.positions ?? []).length === 0 ? (
            <Empty>No open perp positions.</Empty>
          ) : (
            <div className="dash-table-wrap">
              <table className="dash-table">
                <thead>
                  <tr>
                    <th>Market</th>
                    <th>Side</th>
                    <th className="num">Size</th>
                    <th className="num">Entry</th>
                    <th className="num">Mark</th>
                    <th className="num">Liq. price</th>
                    <th className="num">Liq. distance</th>
                    <th className="num">uP&L</th>
                  </tr>
                </thead>
                <tbody>
                  {(account.data.positions ?? []).map((p) => {
                    const d = liqDistance(p);
                    return (
                      <tr key={p.symbol}>
                        <td>{p.symbol}</td>
                        <td>{p.side}</td>
                        <td className="num">{fmtAmount(fromE9(p.sizeE9), 4)}</td>
                        <td className="num">{fmtAmount(fromE9(p.avgEntryPriceE9), 4)}</td>
                        <td className="num">{fmtAmount(fromE9(p.markPriceE9), 4)}</td>
                        <td className="num">{fmtAmount(fromE9(p.liquidationPriceE9), 4)}</td>
                        <td className="num">
                          {d == null ? (
                            "—"
                          ) : (
                            <span className={d < 0.15 ? "neg" : d < 0.3 ? "" : "pos"}>
                              {fmtPct(d)}
                            </span>
                          )}
                        </td>
                        <td className="num">
                          <span className={Number(p.unrealizedPnlE9) >= 0 ? "pos" : "neg"}>
                            {fmtSigned(fromE9(p.unrealizedPnlE9))}
                          </span>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </>
      )}
      <div style={{ marginTop: 14 }}>
        <div className="dash-card__sub" style={{ marginBottom: 6 }}>
          Hourly funding, annualized — {firstSymbol ?? "SUI-PERP"} (positive = shorts earn)
        </div>
        {funding.isError ? (
          <ErrorNote error={funding.error} what="funding history" />
        ) : (
          <SeriesChart
            height={160}
            digits={3}
            lines={[
              {
                name: "funding (ann.)",
                points: (funding.data ?? []).map((p) => ({
                  timeMs: p.fundingTimeAtMillis,
                  value: annualizedFunding(p.fundingRateE9),
                })),
              },
            ]}
          />
        )}
      </div>
    </Card>
  );
}
