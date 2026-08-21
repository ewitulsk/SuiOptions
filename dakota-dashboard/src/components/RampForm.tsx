import { useMemo, useState } from "react";

import * as api from "../api/dakota";
import type { Asset, Catalog, Customer } from "../api/dakota";
import { SANDBOX_MAX_AMOUNT } from "../config";
import { CopyField, ErrorBox, Panel } from "./ui";

type Flow = "onramp" | "offramp" | "swap";

const BLURB: Record<Flow, string> = {
  onramp:
    "USD in, stablecoin out. Dakota returns real ACH and Fedwire details; wire USD there and the stablecoin lands at your destination address.",
  offramp:
    "Stablecoin in, USD out. Dakota returns a deposit address; send the stablecoin there and Dakota wires the dollars to the bank account.",
  swap: "Stablecoin in, stablecoin out — across chains. Fully on-chain in both directions.",
};

/** The ramp UI, shared by all three roles.
 *
 *  It walks the whole prerequisite chain — recipient, destination, account —
 *  because the Dakota API will not tell you what is missing until it rejects
 *  you, and each step's id feeds the next. */
export default function RampForm({
  token,
  catalog,
  customers,
  isAdmin,
}: {
  token: string;
  catalog: Catalog;
  customers: Customer[];
  isAdmin: boolean;
}) {
  const [flow, setFlow] = useState<Flow>("onramp");
  const [customerId, setCustomerId] = useState(customers[0]?.dakota_customer_id ?? "");
  const [assetKey, setAssetKey] = useState("");
  const [cryptoAddress, setCryptoAddress] = useState("");
  const [recipientName, setRecipientName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [result, setResult] = useState<Record<string, any> | null>(null);

  const enabled = useMemo(
    () =>
      catalog.assets.filter(
        (a) =>
          (flow === "onramp" && a.onramp_enabled) ||
          (flow === "offramp" && a.offramp_enabled) ||
          (flow === "swap" && a.swap_enabled),
      ),
    [catalog.assets, flow],
  );

  const selected: Asset | undefined = enabled.find(
    (a) => `${a.symbol}@${a.network_id}` === assetKey,
  );
  const customer = customers.find((c) => c.dakota_customer_id === customerId);
  const approved = customer?.kyb_status === "active";

  const submit = async () => {
    if (!selected || !customerId) return;
    setBusy(true);
    setError(null);
    setResult(null);
    try {
      // 1. Recipient. Crypto-only recipients need no address; a fiat
      //    destination would, which is why offramp asks for more below.
      const recipient = await api.createRecipient(token, customerId, {
        name: recipientName || "Console recipient",
      });

      // 2. Destination.
      const destination = await api.createDestination(token, recipient.id, {
        customer_id: customerId,
        destination_type: "crypto",
        name: `${selected.symbol} on ${selected.network_id}`,
        crypto_address: cryptoAddress,
        network_id: selected.network_id,
      });

      // 3. Account. `capabilities` is filled in server-side for onramps —
      //    Dakota requires it and does not document that.
      const body: api.CreateAccountBody =
        flow === "onramp"
          ? {
              customer_id: customerId,
              account_type: "onramp",
              crypto_destination_id: destination.id,
              destination_network_id: selected.network_id,
              source_asset: "USD",
              destination_asset: selected.symbol,
            }
          : flow === "swap"
            ? {
                customer_id: customerId,
                account_type: "swap",
                crypto_destination_id: destination.id,
                destination_network_id: selected.network_id,
                source_network_id: selected.network_id,
                source_asset: selected.symbol,
                destination_asset: selected.symbol,
              }
            : {
                customer_id: customerId,
                account_type: "offramp",
                crypto_destination_id: destination.id,
                source_network_id: selected.network_id,
                source_asset: selected.symbol,
                destination_asset: "USD",
              };

      setResult(await api.createAccount(token, body));
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <div className="tabs">
        {(["onramp", "offramp", "swap"] as Flow[]).map((f) => (
          <button key={f} className={flow === f ? "active" : ""} onClick={() => setFlow(f)}>
            {f}
          </button>
        ))}
      </div>

      <Panel title={flow} hint={BLURB[flow]}>
        <ErrorBox error={error} />

        {customers.length === 0 ? (
          <p className="muted">No customers yet. Create one first.</p>
        ) : (
          <>
            <div className="row">
              <label>
                <span>Customer</span>
                <select value={customerId} onChange={(e) => setCustomerId(e.target.value)}>
                  {customers.map((c) => (
                    <option key={c.dakota_customer_id} value={c.dakota_customer_id}>
                      {c.external_ref || c.dakota_customer_id} ({c.customer_type})
                    </option>
                  ))}
                </select>
              </label>
              <label>
                <span>Asset and network</span>
                <select value={assetKey} onChange={(e) => setAssetKey(e.target.value)}>
                  <option value="">Select…</option>
                  {enabled.map((a) => (
                    <option key={a.id} value={`${a.symbol}@${a.network_id}`}>
                      {a.symbol} on {a.network_id}
                    </option>
                  ))}
                </select>
              </label>
            </div>

            {enabled.length === 0 && (
              <p className="muted">
                No assets are enabled for {flow}.{" "}
                {isAdmin ? "Enable one under Assets." : "Ask an admin to enable one."}
              </p>
            )}

            <div className="row">
              <label>
                <span>Recipient name</span>
                <input
                  value={recipientName}
                  onChange={(e) => setRecipientName(e.target.value)}
                  placeholder="Console recipient"
                />
              </label>
              <label>
                <span>
                  {flow === "onramp" ? "Deliver stablecoin to" : "Deliver proceeds to"} (address)
                </span>
                <input
                  className="mono"
                  value={cryptoAddress}
                  onChange={(e) => setCryptoAddress(e.target.value)}
                  placeholder="0x…"
                />
              </label>
            </div>

            {!approved && customer && (
              <p className="muted">
                This customer is not approved yet (kyb_status ={" "}
                {customer.kyb_status ?? "unknown"}). Dakota will refuse the account until it
                is. {isAdmin ? "Use Approve on the Customers screen." : ""}
              </p>
            )}

            <div className="actions">
              <button
                disabled={busy || !selected || !cryptoAddress || !approved}
                onClick={() => void submit()}
              >
                {busy ? "Working…" : `Create ${flow}`}
              </button>
              <span className="muted">Sandbox caps each transfer at ${SANDBOX_MAX_AMOUNT.toFixed(2)}.</span>
            </div>
          </>
        )}
      </Panel>

      {result && <DepositInstructions result={result} flow={flow} />}
    </>
  );
}

/** Where the money actually has to go.
 *
 *  These values come straight from Dakota and are never stored by us — the
 *  bank block in particular is pure PII. */
function DepositInstructions({ result, flow }: { result: Record<string, any>; flow: Flow }) {
  const bank = result.bank_account as Record<string, any> | undefined;
  return (
    <Panel
      title="Deposit instructions"
      hint="Relayed live from Dakota and not stored anywhere by this console."
    >
      <CopyField label="Account id" value={String(result.id ?? "")} />

      {flow === "onramp" && bank ? (
        <>
          <div className="row">
            <CopyField label="Routing (ABA)" value={String(bank.aba_routing_number ?? "")} />
            <CopyField label="Account number" value={String(bank.account_number ?? "")} />
          </div>
          <div className="row">
            <CopyField label="Bank" value={String(bank.bank_name ?? "")} />
            <CopyField label="Account holder" value={String(bank.account_holder_name ?? "")} />
          </div>
          <p className="muted">
            Wire USD to these details. Dakota converts and delivers the stablecoin on-chain.
          </p>
        </>
      ) : result.source_crypto_address ? (
        <>
          <CopyField label="Send stablecoin to" value={String(result.source_crypto_address)} />
          <p className="muted">
            On <code>{String(result.source_network_id ?? "")}</code>. Sending on any other
            chain loses the funds.
          </p>
        </>
      ) : (
        <p className="muted">Dakota returned no deposit details for this account.</p>
      )}
    </Panel>
  );
}
