// Positions — everything the bot holds or has written, plus vault
// custody objects and the live reservation ledger.

import { enabledState, useDeskState, type DeskState } from "../api/deskState";
import { fmtAmount, fmtDate, fmtExpiry, fromRaw, shortId, timeAgo } from "../api/format";
import { useDeskFills } from "../api/indexer";
import { useVaultDetail } from "../api/vault";
import { Card, Empty, ErrorNote, Pill } from "../components/ui";
import { DeskDownBanner } from "../components/DeskDownBanner";

export function Positions() {
  const desk = useDeskState();
  const state = enabledState(desk.data);
  const vault = useVaultDetail(state?.vault.vaultId);
  const fills = useDeskFills(state?.vault.vaultId);

  if (desk.isError || (desk.data && !desk.data.enabled) || (desk.isLoading && !state)) {
    return <DeskDownBanner query={desk} />;
  }
  if (!state) return <Empty>Loading desk state…</Empty>;
  const dec = state.vault.settlementDecimals;

  return (
    <div className="dash-grid">
      <Card
        title="Held options (long)"
        sub="vault custody + wallet float + coin-custody positions; marks are model fair"
      >
        {state.holdings.length === 0 ? (
          <Empty>No held options.</Empty>
        ) : (
          <div className="dash-table-wrap">
            <table className="dash-table">
              <thead>
                <tr>
                  <th>Series</th>
                  <th>Expiry</th>
                  <th className="num">Amount</th>
                  <th className="num">vault / wallet / positions</th>
                  <th className="num">Mark / unit</th>
                  <th className="num">Value</th>
                  <th className="num">σ</th>
                  <th className="num">Δ / unit</th>
                  <th>Resale pool</th>
                </tr>
              </thead>
              <tbody>
                {state.holdings.map((h) => (
                  <tr key={h.bucketId}>
                    <td>
                      <b>{h.symbol ?? "?"}</b> {h.isPut ? "PUT" : "CALL"} @{" "}
                      {fmtAmount(h.strikeScaled, 4)}
                      <span className="muted"> · {shortId(h.bucketId)}</span>
                    </td>
                    <td>{fmtExpiry(h.expiryMs)}</td>
                    <td className="num">{fmtAmount(h.amount)}</td>
                    <td className="num muted">
                      {fmtAmount(h.amountVault)} / {fmtAmount(h.amountWallet)} /{" "}
                      {fmtAmount(h.amountCoinPositions)}
                    </td>
                    <td className="num">{h.mark ? h.mark.markPerUnit.toFixed(6) : "—"}</td>
                    <td className="num">
                      {h.mark ? fmtAmount(fromRaw(h.mark.value, dec)) : "—"}
                    </td>
                    <td className="num">{h.mark ? (h.mark.sigma * 100).toFixed(1) : "—"}</td>
                    <td className="num">
                      {h.mark ? h.mark.greeksPerUnit.delta.toFixed(3) : "—"}
                    </td>
                    <td>{h.poolId ? shortId(h.poolId) : <span className="muted">none</span>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      <Card title="Written options (short)" sub="vault-custodied Position objects; naked = uncovered by a same-series long">
        {state.written.length === 0 ? (
          <Empty>No written positions.</Empty>
        ) : (
          <div className="dash-table-wrap">
            <table className="dash-table">
              <thead>
                <tr>
                  <th>Series</th>
                  <th>Expiry</th>
                  <th className="num">Amount</th>
                  <th className="num">Covered</th>
                  <th className="num">Naked</th>
                  <th className="num">Mark / unit</th>
                  <th className="num">Liability</th>
                  <th>Position</th>
                </tr>
              </thead>
              <tbody>
                {state.written.map((w) => (
                  <tr key={w.positionId}>
                    <td>
                      <b>{w.symbol ?? "?"}</b> {w.isPut ? "PUT" : "CALL"} @{" "}
                      {fmtAmount(w.strikeScaled, 4)}
                    </td>
                    <td>{fmtExpiry(w.expiryMs)}</td>
                    <td className="num">{fmtAmount(w.amount)}</td>
                    <td className="num">{fmtAmount(w.covered)}</td>
                    <td className="num">
                      {w.naked > 0 ? <span className="neg">{fmtAmount(w.naked)}</span> : 0}
                    </td>
                    <td className="num">{w.mark ? w.mark.markPerUnit.toFixed(6) : "—"}</td>
                    <td className="num">
                      {w.mark ? <span className="neg">{fmtAmount(fromRaw(w.mark.value, dec))}</span> : "—"}
                    </td>
                    <td className="muted">{shortId(w.positionId)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      <Card
        title="Vault custody positions"
        sub="from the api-service view: every custodied object with its last appraisal mark"
        span="half"
      >
        {vault.isError && <ErrorNote error={vault.error} what="vault positions" />}
        {vault.data &&
          (vault.data.positions.filter((p) => p.active).length === 0 ? (
            <Empty>No custodied positions.</Empty>
          ) : (
            <div className="dash-table-wrap">
              <table className="dash-table">
                <thead>
                  <tr>
                    <th>Position</th>
                    <th>Adapter</th>
                    <th className="num">Last mark</th>
                    <th>Appraised</th>
                    <th className="num">Bot model</th>
                  </tr>
                </thead>
                <tbody>
                  {vault.data.positions
                    .filter((p) => p.active)
                    .map((p) => {
                      const botMark = botMarkFor(state, p.positionId);
                      return (
                        <tr key={p.positionId}>
                          <td className="muted">{shortId(p.positionId)}</td>
                          <td>{adapterLabel(p.adapter)}</td>
                          <td className="num">{fmtAmount(fromRaw(p.lastValueRaw, dec))}</td>
                          <td className="muted">{timeAgo(p.lastAppraisedAtMs)}</td>
                          <td className="num">
                            {botMark == null ? (
                              <span className="muted">—</span>
                            ) : (
                              fmtAmount(fromRaw(botMark, dec))
                            )}
                          </td>
                        </tr>
                      );
                    })}
                </tbody>
              </table>
            </div>
          ))}
        <div className="dash-note">
          "Bot model" joins the desk's mark-to-model where the custody object is a written
          option Position the desk tracks; the vault's own marks are the conservative on-chain
          appraisals.
        </div>
      </Card>

      <Card
        title="Reservations"
        sub="premium reserved for live signed quotes (in-memory; resets on bot restart)"
        span="half"
      >
        <div style={{ display: "flex", gap: 8, flexWrap: "wrap", marginBottom: 10 }}>
          <Pill tone="muted">count {state.reservations.count}</Pill>
          <Pill tone="muted">total {fmtAmount(fromRaw(state.reservations.total, dec))}</Pill>
        </div>
        {state.reservations.entries.length === 0 ? (
          <Empty>No live reservations.</Empty>
        ) : (
          <div className="dash-table-wrap">
            <table className="dash-table">
              <thead>
                <tr>
                  <th className="num">Amount</th>
                  <th>Expires</th>
                </tr>
              </thead>
              <tbody>
                {state.reservations.entries.map((r, i) => (
                  <tr key={i}>
                    <td className="num">{fmtAmount(fromRaw(r.amount, dec))}</td>
                    <td>{fmtDate(r.expires_ms)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      <Card title="Recent fills" sub="on-chain writes whose collateral released from the vault (newest first)">
        {fills.isError && <ErrorNote error={fills.error} what="fills" />}
        {fills.data &&
          (fills.data.length === 0 ? (
            <Empty>No fills observed.</Empty>
          ) : (
            <div className="dash-table-wrap">
              <table className="dash-table">
                <thead>
                  <tr>
                    <th>When</th>
                    <th>Side</th>
                    <th>Kind</th>
                    <th>Bucket</th>
                    <th className="num">Amount</th>
                    <th className="num">Premium</th>
                  </tr>
                </thead>
                <tbody>
                  {fills.data.map((f) => (
                    <tr key={f.sequence}>
                      <td>{fmtDate(f.timestampMs)}</td>
                      <td>
                        {f.side === "bought" ? (
                          <span className="pos">bought</span>
                        ) : (
                          <span className="neg">wrote</span>
                        )}
                      </td>
                      <td>{f.kind.toUpperCase()}</td>
                      <td className="muted">{shortId(f.bucketId)}</td>
                      <td className="num">{fmtAmount(f.amount)}</td>
                      <td className="num">{fmtAmount(fromRaw(f.premium, dec))}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ))}
      </Card>
    </div>
  );
}

function botMarkFor(state: DeskState, positionId: string): number | null {
  const w = state.written.find((w) => w.positionId === positionId);
  return w?.mark ? w.mark.value : null;
}

function adapterLabel(adapter: string): string {
  const m = adapter.match(/::([a-z_]+)::([A-Za-z]+)$/);
  return m ? `${m[1]}::${m[2]}` : adapter;
}
