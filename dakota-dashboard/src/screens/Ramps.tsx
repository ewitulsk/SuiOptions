import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";

import * as api from "../api/dakota";
import RampForm from "../components/RampForm";
import { Empty, ErrorBox, Panel, Table, fmtTime, shortId } from "../components/ui";
import { SANDBOX_MAX_AMOUNT } from "../config";
import { useAuthed } from "../state/session";

export default function Ramps() {
  const { token, role } = useAuthed();
  const qc = useQueryClient();
  const catalog = useQuery({ queryKey: ["catalog"], queryFn: () => api.getCatalog(token) });
  const customers = useQuery({ queryKey: ["customers"], queryFn: () => api.listCustomers(token) });
  const accounts = useQuery({ queryKey: ["accounts"], queryFn: () => api.listAccounts(token) });

  return (
    <>
      <h2>Ramps</h2>
      <ErrorBox error={catalog.error ?? customers.error ?? accounts.error} />

      {catalog.data && customers.data && (
        <RampForm
          token={token}
          catalog={catalog.data}
          customers={customers.data}
          isAdmin={role === "admin"}
        />
      )}

      <Panel title="Accounts">
        {accounts.data?.length ? (
          <Table
            head={
              <tr>
                <th>Id</th>
                <th>Type</th>
                <th>Customer</th>
                <th>Source</th>
                <th>Destination</th>
                <th>Rail</th>
                <th>Created</th>
              </tr>
            }
          >
            {accounts.data.map((a) => (
              <tr key={a.dakota_account_id}>
                <td className="mono">{shortId(a.dakota_account_id)}</td>
                <td>{a.account_type}</td>
                <td className="mono">{shortId(a.dakota_customer_id)}</td>
                <td>
                  {a.source_asset ?? "—"}
                  {a.source_network_id ? ` / ${a.source_network_id}` : ""}
                </td>
                <td>
                  {a.destination_asset ?? "—"}
                  {a.destination_network_id ? ` / ${a.destination_network_id}` : ""}
                </td>
                <td>{a.rail ?? "—"}</td>
                <td>{fmtTime(a.created_at)}</td>
              </tr>
            ))}
          </Table>
        ) : (
          <Empty>No ramp accounts yet.</Empty>
        )}
      </Panel>

      {role === "admin" && (
        <SimulateDeposit
          token={token}
          accounts={accounts.data ?? []}
          onDone={() => {
            void qc.invalidateQueries({ queryKey: ["feed"] });
            void qc.invalidateQueries({ queryKey: ["flows"] });
          }}
        />
      )}
    </>
  );
}

/** Sandbox funding.
 *
 *  In sandbox the banking rails are mocked, so an onramp is funded by
 *  simulating the inbound wire rather than actually sending one. Crypto legs
 *  settle for real on testnets — `crypto_inbound` simulates those too, which
 *  saves needing a funded testnet wallet just to exercise an offramp. */
function SimulateDeposit({
  token,
  accounts,
  onDone,
}: {
  token: string;
  accounts: api.Account[];
  onDone: () => void;
}) {
  const [accountId, setAccountId] = useState("");
  const [amount, setAmount] = useState("2.00");
  const [type, setType] = useState("ach_inbound");
  const [walletAddress, setWalletAddress] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [ok, setOk] = useState(false);

  const isCrypto = type === "crypto_inbound";
  const overCap = Number(amount) > SANDBOX_MAX_AMOUNT;

  const run = async () => {
    setBusy(true);
    setError(null);
    setOk(false);
    try {
      await api.simulateInbound(token, {
        type,
        amount,
        currency: "USD",
        account_id: isCrypto ? undefined : accountId,
        wallet_address: isCrypto ? walletAddress : undefined,
      });
      setOk(true);
      onDone();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Panel
      title="Simulate a deposit (sandbox)"
      hint="Banking is mocked in sandbox, so this is how an onramp gets funded. Crypto legs settle for real on testnets."
    >
      <ErrorBox error={error} />
      {ok && <div className="success">Accepted. Webhooks will land shortly.</div>}

      <div className="row">
        <label>
          <span>Rail</span>
          <select value={type} onChange={(e) => setType(e.target.value)}>
            <option value="ach_inbound">ACH inbound</option>
            <option value="fedwire_inbound">Fedwire inbound</option>
            <option value="fednow_inbound">FedNow inbound</option>
            <option value="crypto_inbound">Crypto inbound</option>
          </select>
        </label>
        {isCrypto ? (
          <label>
            <span>Wallet address</span>
            <input
              className="mono"
              value={walletAddress}
              onChange={(e) => setWalletAddress(e.target.value)}
              placeholder="the account's source_crypto_address"
            />
          </label>
        ) : (
          <label>
            <span>Account</span>
            <select value={accountId} onChange={(e) => setAccountId(e.target.value)}>
              <option value="">Select…</option>
              {accounts.map((a) => (
                <option key={a.dakota_account_id} value={a.dakota_account_id}>
                  {a.account_type} — {shortId(a.dakota_account_id)}
                </option>
              ))}
            </select>
          </label>
        )}
        <label>
          <span>Amount (USD)</span>
          <input value={amount} onChange={(e) => setAmount(e.target.value)} />
        </label>
      </div>

      {overCap && (
        <p className="muted">
          Dakota's sandbox rejects anything above ${SANDBOX_MAX_AMOUNT.toFixed(2)}.
        </p>
      )}

      <div className="actions">
        <button
          disabled={busy || overCap || (isCrypto ? !walletAddress : !accountId)}
          onClick={() => void run()}
        >
          {busy ? "Sending…" : "Simulate"}
        </button>
      </div>
    </Panel>
  );
}
