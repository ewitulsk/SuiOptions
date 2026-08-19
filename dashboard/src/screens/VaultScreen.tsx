// Vault — LP-facing basics: TVL/NAV/pps, the withdraw queue contents,
// free balances, and the external (hedge venue) account summary.

import { enabledState, useDeskState } from "../api/deskState";
import { fmtAmount, fmtDate, fmtPct, fmtRaw, fromRaw, shortId, timeAgo } from "../api/format";
import { useWithdrawQueue, type WithdrawQueueEntry } from "../api/indexer";
import {
  isTranched,
  usePpsHistory,
  useVaultDetail,
  vaultTvlRaw,
  PPS_E12,
  type RiskStateLabel,
  type TradingVault,
} from "../api/vault";
import { PpsChart } from "../components/PpsChart";
import { Card, Empty, ErrorNote, Pill, Tile, type PillTone } from "../components/ui";
import { DeskDownBanner } from "../components/DeskDownBanner";

export const RISK_TONE: Record<RiskStateLabel, PillTone> = {
  healthy: "ok",
  coverage_breach: "warn",
  impaired: "bad",
  reset_pending: "warn",
};

/** Why a queued request can't be paid right now; null when payable.
 * Mirrors api-service `request_payability`: a wiped junior generation is
 * dead, and the junior lane freezes whenever capital is risk-off (a
 * commitment breach alone does NOT block the lane). */
function blockedReason(e: WithdrawQueueEntry, v: TradingVault): string | null {
  if (e.tranche === "junior" && e.capitalGeneration < v.activeJuniorGeneration)
    return "wiped generation";
  if (e.lane === "junior" && v.riskStateCode !== 0) return "lane blocked";
  return null;
}

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
              {/* Risk state + commitment first — the incident questions. */}
              <Pill tone={RISK_TONE[v.riskState]}>
                risk {v.riskState === "healthy" ? "healthy" : v.riskState.replace("_", " ").toUpperCase()}
              </Pill>
              <Pill tone={v.curatorCommitmentBreached ? "bad" : "ok"}>
                commitment {v.curatorCommitmentBreached ? "BREACHED" : "ok"}
              </Pill>
              <Pill tone={v.state === "open" ? "ok" : "warn"}>state {v.state}</Pill>
              {v.settled && <Pill tone="warn">settled — claims via settlement pool</Pill>}
              <Pill tone={v.depositsPaused ? "warn" : "ok"}>
                deposits {v.depositsPaused ? "paused" : "open"}
              </Pill>
              <Pill tone={v.mmReleaseEnabled ? "ok" : "warn"}>
                vault_mm release {v.mmReleaseEnabled ? "on" : "off"}
              </Pill>
              <Pill tone="muted">
                {isTranched(v) ? `tranched · junior gen ${v.activeJuniorGeneration}` : "untranched"}
              </Pill>
              <Pill tone="muted">curator {shortId(v.curator)}</Pill>
              <Pill tone="muted">fee {(v.curatorFeeBps / 100).toFixed(1)}% of profit</Pill>
              <Pill tone="muted">lockup {Math.round(v.lockupMs / 3_600_000)}h</Pill>
            </div>
            {v.impairedSinceMs != null && (
              <div className="dash-note" style={{ marginBottom: 12 }}>
                Impaired since {fmtDate(v.impairedSinceMs)} ({timeAgo(v.impairedSinceMs)}).
              </div>
            )}
            {v.resetProposal != null && (
              <div className="dash-note" style={{ marginBottom: 12 }}>
                Junior reset proposed {fmtDate(v.resetProposal.proposedAtMs)} (wipes generation{" "}
                {v.resetProposal.oldGeneration}) — executable {fmtDate(v.resetProposal.executableAtMs)};
                requires a {fmtRaw(v.resetProposal.recordedRequiredDepositRaw, dec)} recapitalization
                deposit.
              </div>
            )}
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
            {isTranched(v) && (
              <div className="dash-tiles" style={{ marginTop: 12 }}>
                <Tile
                  label="Senior NAV"
                  value={fmtRaw(v.seniorNavRaw, dec)}
                  hint={`claim ${fmtRaw(v.seniorClaimRaw, dec)} · shares ${fmtRaw(v.seniorSharesRaw, sharesScale)}`}
                />
                <Tile
                  label="Junior NAV"
                  value={fmtRaw(v.juniorNavRaw, dec)}
                  hint={`shares ${fmtRaw(v.juniorSharesRaw, sharesScale)}`}
                />
                <Tile
                  label="Senior pps"
                  value={
                    v.seniorPpsRaw == null
                      ? "—"
                      : (Number(v.seniorPpsRaw) / PPS_E12).toFixed(4)
                  }
                />
                <Tile
                  label="Junior pps"
                  value={
                    v.juniorPpsRaw == null
                      ? "—"
                      : (Number(v.juniorPpsRaw) / PPS_E12).toFixed(4)
                  }
                />
                <Tile
                  label="Junior buffer"
                  value={v.juniorBufferBps == null ? "—" : fmtPct(v.juniorBufferBps / 10_000)}
                  hint={
                    v.capitalStructure == null
                      ? undefined
                      : `maintenance ${fmtPct(v.capitalStructure.maintenanceJuniorBps / 10_000)} · target ${fmtPct(v.capitalStructure.targetJuniorBps / 10_000)}`
                  }
                />
                <Tile
                  label="Senior hurdle"
                  value={
                    v.capitalStructure == null
                      ? "—"
                      : fmtPct(v.capitalStructure.seniorHurdleBpsAnnual / 10_000)
                  }
                  hint={v.capitalStructure?.upside.replace(/_/g, " ")}
                />
              </div>
            )}
          </>
        )}
      </Card>

      <Card
        title="Share price"
        sub={
          v != null && isTranched(v)
            ? "per tranche, from deposits, fulfillments, and capital syncs"
            : "from on-chain appraisals (deposits, fulfillments, capital syncs)"
        }
        span="half"
      >
        <PpsChart points={pps.data ?? []} tranched={v != null && isTranched(v)} />
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
        sub="requested − fulfilled − settlement-drained events (per-lane FIFO on the global sequence)"
        span="half"
      >
        {queue.isError && <ErrorNote error={queue.error} what="withdraw queue" />}
        {v != null && v.riskStateCode !== 0 && (
          <div style={{ display: "flex", gap: 8, flexWrap: "wrap", marginBottom: 8 }}>
            <Pill tone="bad">junior lane BLOCKED — risk state {v.riskState.replace("_", " ")}</Pill>
          </div>
        )}
        {queue.data &&
          (queue.data.pending.length === 0 ? (
            <Empty>Queue is empty.</Empty>
          ) : (
            <div className="dash-table-wrap">
              <table className="dash-table">
                <thead>
                  <tr>
                    <th className="num">Seq</th>
                    <th>Lane</th>
                    <th>Recipient</th>
                    <th className="num">Shares</th>
                    <th className="num">Basis</th>
                    <th>Requested</th>
                    <th>Status</th>
                  </tr>
                </thead>
                <tbody>
                  {queue.data.pending.map((e) => {
                    const blocked = v == null ? null : blockedReason(e, v);
                    return (
                      <tr key={e.globalSeq}>
                        <td className="num">{e.globalSeq}</td>
                        <td>
                          {e.lane}
                          {e.tranche !== e.lane && (
                            <span className="muted"> ({e.tranche})</span>
                          )}
                        </td>
                        <td className="muted">{shortId(e.recipient)}</td>
                        <td className="num">{fmtAmount(fromRaw(e.sharesRaw, sharesScale))}</td>
                        <td className="num">{fmtAmount(fromRaw(e.basisRaw, dec))}</td>
                        <td>{fmtDate(e.requestedAtMs)}</td>
                        <td>
                          {blocked == null ? (
                            <span className="muted">payable</span>
                          ) : (
                            <span className="neg">{blocked}</span>
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          ))}
        {v != null && (
          <div className="dash-note">
            Lane cursors: senior head {v.laneHeads.senior.head} / tail {v.laneHeads.senior.tail} ·
            junior head {v.laneHeads.junior.head} / tail {v.laneHeads.junior.tail}
            {queue.data != null && queue.data.settledCount > 0
              ? ` · ${queue.data.settledCount} drained via settlement`
              : ""}
            .
          </div>
        )}
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
