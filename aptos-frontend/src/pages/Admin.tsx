import { useState } from "react";
import { api } from "../api";
import { VENUES } from "../config";

// Admin builds venue/fee payloads for the multisig to sign. Nothing here
// submits transactions or holds keys.
export default function Admin() {
  const [token, setToken] = useState("");
  const [config, setConfig] = useState("");
  const [out, setOut] = useState("");
  const [feeBps, setFeeBps] = useState("0");
  const [minFee, setMinFee] = useState("0");

  async function run(fn: () => Promise<unknown>) {
    try {
      setOut(JSON.stringify(await fn(), null, 2));
    } catch (e) {
      setOut(e instanceof Error ? e.message : String(e));
    }
  }

  return (
    <div>
      <h2>Admin (payload builder)</h2>
      <p>
        Admin token:{" "}
        <input
          type="password"
          value={token}
          onChange={(e) => setToken(e.target.value)}
          placeholder="ADMIN_TOKEN"
        />
      </p>
      <p>
        Router config:{" "}
        <input
          value={config}
          onChange={(e) => setConfig(e.target.value)}
          placeholder="0x…"
          size={68}
        />
      </p>
      <h3>Venues</h3>
      {VENUES.map((v) => (
        <p key={v.id}>
          {v.name}{" "}
          <button
            onClick={() =>
              void run(() =>
                api.adminVenues(token, { config, venue: v.id, enable: true }),
              )
            }
          >
            enable
          </button>{" "}
          <button
            onClick={() =>
              void run(() =>
                api.adminVenues(token, { config, venue: v.id, enable: false }),
              )
            }
          >
            disable
          </button>
        </p>
      ))}
      <h3>Fees</h3>
      <p>
        bps <input value={feeBps} onChange={(e) => setFeeBps(e.target.value)} size={6} />{" "}
        min <input value={minFee} onChange={(e) => setMinFee(e.target.value)} size={10} />{" "}
        <button
          onClick={() =>
            void run(() =>
              api.adminFees(token, {
                config,
                fee_bps: feeBps,
                min_fee: minFee,
              }),
            )
          }
        >
          build set_fee
        </button>
      </p>
      <pre>{out}</pre>
    </div>
  );
}
