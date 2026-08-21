import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";

import * as api from "../api/dakota";
import { CopyField, Empty, ErrorBox, Panel, Table } from "../components/ui";
import { SANDBOX_MAX_AMOUNT } from "../config";
import { useAuthed } from "../state/session";

/** Our own non-custodial Dakota wallet.
 *
 *  The private key lives server-side in Secrets Manager; this screen never
 *  touches key material. Sends are signed by dakota-service as endorsed
 *  requests — the browser only names the amount and destination. */
export default function Treasury() {
  const { token } = useAuthed();
  const qc = useQueryClient();
  const treasury = useQuery({ queryKey: ["treasury"], queryFn: () => api.getTreasury(token) });
  const catalog = useQuery({ queryKey: ["catalog"], queryFn: () => api.getCatalog(token) });

  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);

  const wallets = treasury.data?.treasury ?? [];

  const setup = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.setupTreasury(token);
      await qc.invalidateQueries({ queryKey: ["treasury"] });
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <h2>Treasury</h2>
      <ErrorBox error={error ?? treasury.error} />

      {wallets.length === 0 && (
        <Panel
          title="No treasury wallet yet"
          hint="Creates a signer, signer group, policy and wallet in one go. Not idempotent — running it twice creates a second wallet."
        >
          <button disabled={busy} onClick={() => void setup()}>
            {busy ? "Creating…" : "Create treasury wallet"}
          </button>
          <p className="muted" style={{ marginTop: 10 }}>
            Requires <code>dakota.wallet_p256_pem</code> in the service's secrets.
          </p>
        </Panel>
      )}

      {wallets.map((entry: any) => (
        <WalletCard
          key={entry.wallet.dakota_wallet_id}
          token={token}
          entry={entry}
          networks={catalog.data?.assets ?? []}
          onSent={() => void qc.invalidateQueries({ queryKey: ["treasury"] })}
        />
      ))}
    </>
  );
}

function WalletCard({
  token,
  entry,
  networks,
  onSent,
}: {
  token: string;
  entry: any;
  networks: api.Asset[];
  onSent: () => void;
}) {
  const wallet = entry.wallet as {
    dakota_wallet_id: string;
    address: string | null;
    family: string;
    label: string | null;
  };
  const balances = entry.balances as any;

  const [to, setTo] = useState("");
  const [amount, setAmount] = useState("1.00");
  const [assetKey, setAssetKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [ok, setOk] = useState(false);

  const selected = networks.find((a) => `${a.symbol}@${a.network_id}` === assetKey);
  const overCap = Number(amount) > SANDBOX_MAX_AMOUNT;

  const send = async () => {
    if (!selected) return;
    setBusy(true);
    setError(null);
    setOk(false);
    try {
      await api.treasurySend(token, wallet.dakota_wallet_id, {
        to,
        amount,
        asset_id: selected.symbol,
        network_id: selected.network_id,
      });
      setOk(true);
      onSent();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Panel title={wallet.label ?? wallet.dakota_wallet_id}>
      {wallet.address && <CopyField label="Address" value={wallet.address} />}

      {balances?.balances?.length ? (
        <Table
          head={
            <tr>
              <th>Asset</th>
              <th>Network</th>
              <th className="num">Amount</th>
            </tr>
          }
        >
          {balances.balances.map((b: any, i: number) => (
            <tr key={i}>
              <td>{b.asset ?? b.asset_id ?? "—"}</td>
              <td className="mono">{b.network_id ?? "—"}</td>
              <td className="num">{b.amount ?? "—"}</td>
            </tr>
          ))}
        </Table>
      ) : (
        <Empty>
          Empty wallet{balances?.total_amount_usd ? ` (${balances.total_amount_usd} USD)` : ""}.
        </Empty>
      )}

      <h3 style={{ marginTop: 18 }}>Send</h3>
      <ErrorBox error={error} />
      {ok && <div className="success">Submitted.</div>}
      <div className="row">
        <label>
          <span>Asset</span>
          <select value={assetKey} onChange={(e) => setAssetKey(e.target.value)}>
            <option value="">Select…</option>
            {networks.map((a) => (
              <option key={a.id} value={`${a.symbol}@${a.network_id}`}>
                {a.symbol} on {a.network_id}
              </option>
            ))}
          </select>
        </label>
        <label>
          <span>To</span>
          <input className="mono" value={to} onChange={(e) => setTo(e.target.value)} placeholder="0x…" />
        </label>
        <label>
          <span>Amount</span>
          <input value={amount} onChange={(e) => setAmount(e.target.value)} />
        </label>
      </div>
      {overCap && (
        <p className="muted">Sandbox caps transfers at ${SANDBOX_MAX_AMOUNT.toFixed(2)}.</p>
      )}
      <div className="actions">
        <button disabled={busy || !selected || !to || overCap} onClick={() => void send()}>
          {busy ? "Signing…" : "Sign and send"}
        </button>
        <span className="muted">Signed server-side; the key never reaches this browser.</span>
      </div>
    </Panel>
  );
}
