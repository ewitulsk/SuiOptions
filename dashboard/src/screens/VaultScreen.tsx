// Vault — LP-facing basics: TVL/NAV/pps, the withdraw queue contents,
// free balances, and the external (hedge venue) account summary.

import { enabledState, useDeskState } from "../api/deskState";
import { fmtAmount, fmtDate, fmtPct, fmtRaw, fromRaw, shortId, timeAgo } from "../api/format";
import { useWithdrawQueue } from "../api/indexer";
import { usePpsHistory, useVaultDetail, vaultTvlRaw, PPS_E12 } from "../api/vault";
import { PpsChart } from "../components/PpsChart";
import { Card, Empty, ErrorNote, Pill, Tile } from "../components/ui";
import { DeskDownBanner } from "../components/DeskDownBanner";

export function VaultScreen() {
  const desk = useDeskState();
  const state = enabledState(desk.data);
  const vaultId = state?.vault.vaultId;
  const vault = useVaultDetail(vaultId);
  const pps = usePpsHistory(vaultId);
  const queue = useWithdrawQueue(vaultId);

  if (desk.isError || (desk.data && !desk.data.enabled) || (desk.isLoading && !state)) {
    return <DeskDownBanner query={desk} />;
  }
  if (!state) return <Empty>Loading desk state…</Empty>;
  const dec = state.vault.settlementDecimals;
  const v = vault.data;
  const sharesScale = dec; // shares mint 1:1 with deposit units at genesis

  return (
    <div className="dash-grid">
      <Card title="Vault" sub={vaultId}>
        {vault.isError && <ErrorNote error={vault.error} what="vault detail" />}
        {v && (
          <>
            <div style={{ display: "flex", gap: 8, flexWrap: "wrap", marginBottom: 12 }}>
              <Pill tone={v.state === "open" ? "ok" : "warn"}>state {v.state}</Pill>
              <Pill tone={v.depositsPaused ? "warn" : "ok"}>
                deposits {v.depositsPaused ? "paused" : "open"}
              </Pill>
              <Pill tone={v.mmReleaseEnabled ? "ok" : "warn"}>
                vault_mm release {v.mmReleaseEnabled ? "on" : "off"}
              </Pill>
              <Pill tone="muted">curator {shortId(v.curator)}</Pill>
              <Pill tone="muted">fee {(v.curatorFeeBps / 100).toFixed(1)}% of profit</Pill>
              <Pill tone="muted">lockup {Math.round(v.lockupMs / 3_600_000)}h</Pill>
            </div>
            <div className="dash-tiles">
              <Tile label="TVL" value={fmtAmount(fromRaw(vaultTvlRaw(v), dec))} />
              <Tile
                label="NAV (appraised)"
                value={fmtRaw(v.latestNavRaw, dec)}
                hint={`as of ${timeAgo(v.navUpdatedAtMs)}`}
              />
              <Tile
                label="Share price"
                value={
                  v.latestPpsE12Raw == null
                    ? "—"
                    : (Number(v.latestPpsE12Raw) / PPS_E12).toFixed(4)
                }
              />
              <Tile label="Total shares" value={fmtRaw(v.totalSharesRaw, sharesScale)} />
              <Tile label="Positions" value={v.positionCount} />
              <Tile label="Pending withdrawals" value={v.pendingWithdrawals} />
            </div>
          </>
        )}
      </Card>

      <Card title="Share price" sub="from on-chain appraisals (deposits + fulfillments)" span="half">
        <PpsChart points={pps.data ?? []} />
        {pps.data && pps.data.length < 2 && <Empty>Not enough history yet.</Empty>}
      </Card>

      <Card
        title="External account"
        sub="funds released to the hedge venue parent address (budgeted on-chain)"
        span="half"
      >
        {v &&
          (v.externalAccount == null ? (
            <Empty>
              No external account registered — nothing can leave the vault toward a hedge
              venue. See the Venues tab once Bluefin is configured.
            </Empty>
          ) : (
            <div className="dash-tiles">
              <Tile label="Account" value={shortId(v.externalAccount)} />
              <Tile
                label="Exposure (released − returned)"
                value={fmtRaw(v.externalExposure, dec)}
              />
              <Tile
                label="Last attested equity"
                value={fmtRaw(v.latestExternalEquity, dec)}
                hint={`as of ${timeAgo(v.externalEquityUpdatedAtMs)}`}
              />
            </div>
          ))}
      </Card>

      <Card
        title="Withdraw queue"
        sub="derived from TvWithdrawRequested − TvWithdrawFulfilled events (FIFO, all-or-nothing)"
        span="half"
      >
        {queue.isError && <ErrorNote error={queue.error} what="withdraw queue" />}
        {queue.data &&
          (queue.data.pending.length === 0 ? (
            <Empty>Queue is empty.</Empty>
          ) : (
            <div className="dash-table-wrap">
              <table className="dash-table">
                <thead>
                  <tr>
                    <th className="num">Seq</th>
                    <th>Recipient</th>
                    <th className="num">Shares</th>
                    <th className="num">Basis</th>
                    <th>Requested</th>
                  </tr>
                </thead>
                <tbody>
                  {queue.data.pending.map((e) => (
                    <tr key={e.seq}>
                      <td className="num">{e.seq}</td>
                      <td className="muted">{shortId(e.recipient)}</td>
                      <td className="num">{fmtAmount(fromRaw(e.sharesRaw, sharesScale))}</td>
                      <td className="num">{fmtAmount(fromRaw(e.basisRaw, dec))}</td>
                      <td>{fmtDate(e.requestedAtMs)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ))}
        {queue.data?.truncated && (
          <div className="dash-note">
            Event scan window truncated — the queue view may be incomplete.
          </div>
        )}
      </Card>

      <Card title="Free balances" sub="un-custodied vault balances (live chain read)" span="half">
        {v &&
          (v.balancesStale ? (
            <Empty>Balance read failed — holdings unknown (not necessarily empty).</Empty>
          ) : v.balances.length === 0 ? (
            <Empty>No free balances.</Empty>
          ) : (
            <div className="dash-table-wrap">
              <table className="dash-table">
                <thead>
                  <tr>
                    <th>Asset</th>
                    <th className="num">Amount</th>
                  </tr>
                </thead>
                <tbody>
                  {v.balances.map((b) => (
                    <tr key={b.coinType}>
                      <td>
                        {b.symbol}
                        <span className="muted"> {shortId(b.coinType, 4)}</span>
                      </td>
                      <td className="num">
                        {b.decimals == null
                          ? `${b.amountRaw} (raw)`
                          : fmtRaw(b.amountRaw, b.decimals)}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ))}
        {state && (
          <div className="dash-note">
            Desk funding input: weighted funding {fmtPct(state.fundingRateAnnual)} · settlement{" "}
            {state.vault.settlementCoinType.split("::").pop()}.
          </div>
        )}
      </Card>
    </div>
  );
}
