import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";

import * as api from "../api/dakota";
import { Empty, ErrorBox, Panel, Table } from "../components/ui";
import { useAuthed } from "../state/session";

/** The supported-asset catalog and the rate card.
 *
 *  Dakota has no assets endpoint and no fee endpoint available to our client
 *  tier, so both of these are ours: the catalog drives every dropdown in the
 *  app and doubles as the server-side allow-list, and the schedule is what we
 *  *expect* to be charged. What we were *actually* charged comes from
 *  transaction receipts and is shown beside it. */
export default function Assets() {
  const { token } = useAuthed();
  const qc = useQueryClient();
  const catalog = useQuery({ queryKey: ["catalog"], queryFn: () => api.getCatalog(token) });
  const rates = useQuery({ queryKey: ["rates"], queryFn: () => api.getRates(token) });

  const [symbol, setSymbol] = useState("USDC");
  const [network, setNetwork] = useState("");
  const [flows, setFlows] = useState({ onramp: true, offramp: true, swap: true });
  const [error, setError] = useState<unknown>(null);

  const save = async () => {
    setError(null);
    try {
      await api.upsertAsset(token, {
        symbol: symbol.trim().toUpperCase(),
        network_id: network,
        onramp_enabled: flows.onramp,
        offramp_enabled: flows.offramp,
        swap_enabled: flows.swap,
        sort_order: 0,
      });
      await qc.invalidateQueries({ queryKey: ["catalog"] });
    } catch (e) {
      setError(e);
    }
  };

  const remove = async (id: number) => {
    setError(null);
    try {
      await api.deleteAsset(token, id);
      await qc.invalidateQueries({ queryKey: ["catalog"] });
    } catch (e) {
      setError(e);
    }
  };

  return (
    <>
      <h2>Assets &amp; rates</h2>
      <ErrorBox error={error} />

      <Panel
        title="Supported assets"
        hint="Dakota has no assets API — this list is ours. It fills every ramp dropdown and is enforced server-side, so an asset that is not here cannot be used even by a hand-written request."
      >
        <div className="row">
          <label>
            <span>Symbol</span>
            <input value={symbol} onChange={(e) => setSymbol(e.target.value)} />
          </label>
          <label>
            <span>Network</span>
            <select value={network} onChange={(e) => setNetwork(e.target.value)}>
              <option value="">Select…</option>
              {catalog.data?.networks.map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </select>
          </label>
        </div>
        <div className="actions">
          {(["onramp", "offramp", "swap"] as const).map((f) => (
            <label key={f} style={{ display: "flex", gap: 6, alignItems: "center", margin: 0 }}>
              <input
                type="checkbox"
                style={{ width: "auto" }}
                checked={flows[f]}
                onChange={(e) => setFlows({ ...flows, [f]: e.target.checked })}
              />
              <span style={{ margin: 0 }}>{f}</span>
            </label>
          ))}
          <button disabled={!symbol || !network} onClick={() => void save()}>
            Save
          </button>
        </div>
        <p className="muted" style={{ marginTop: 10 }}>
          Only testnets are offered: the sandbox lists mainnet ids and then refuses them.
        </p>
      </Panel>

      <Panel title="Catalog">
        {catalog.data?.assets.length ? (
          <Table
            head={
              <tr>
                <th>Asset</th>
                <th>Network</th>
                <th>Onramp</th>
                <th>Offramp</th>
                <th>Swap</th>
                <th></th>
              </tr>
            }
          >
            {catalog.data.assets.map((a) => (
              <tr key={a.id}>
                <td>{a.symbol}</td>
                <td className="mono">{a.network_id}</td>
                <td>{a.onramp_enabled ? "yes" : "—"}</td>
                <td>{a.offramp_enabled ? "yes" : "—"}</td>
                <td>{a.swap_enabled ? "yes" : "—"}</td>
                <td>
                  <button className="secondary" onClick={() => void remove(a.id)}>
                    Remove
                  </button>
                </td>
              </tr>
            ))}
          </Table>
        ) : (
          <Empty>No assets yet. Add one above — ramps cannot run without it.</Empty>
        )}
      </Panel>

      <Panel
        title="Rates"
        hint="Dakota's pricing endpoint returns 403 for our client tier, so the expected schedule is hand-entered. Realised rates below come from actual transaction receipts."
      >
        {rates.data?.schedule ? (
          <p>
            Transfer {rates.data.schedule.transfer_fee_bps ?? "—"} bps · ACH{" "}
            {rates.data.schedule.ach_fee_cents ?? "—"}¢ · Wire{" "}
            {rates.data.schedule.wire_fee_cents ?? "—"}¢{" "}
            <span className="pill warn">source: {rates.data.schedule.source}</span>
          </p>
        ) : (
          <p className="muted">No expected schedule recorded.</p>
        )}

        {rates.data?.realised.length ? (
          <Table
            head={
              <tr>
                <th>Asset</th>
                <th>Rate</th>
                <th className="num">Amount</th>
                <th className="num">Dakota fee</th>
              </tr>
            }
          >
            {rates.data.realised.slice(0, 20).map((r, i) => (
              <tr key={i}>
                <td>{r.asset ?? "—"}</td>
                <td className="mono">{r.exchange_rate ?? "—"}</td>
                <td className="num">{api.formatMinor(r.amount_minor)}</td>
                <td className="num">{api.formatMinor(r.fee_minor)}</td>
              </tr>
            ))}
          </Table>
        ) : (
          <p className="muted">No settled transactions yet, so no realised rates.</p>
        )}
      </Panel>
    </>
  );
}
