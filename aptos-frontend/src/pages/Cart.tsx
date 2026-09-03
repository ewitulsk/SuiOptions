import { useState } from "react";
import { useWallet } from "@aptos-labs/wallet-adapter-react";
import { api, octasToApt } from "../api";
import { useCart } from "../cart";

export default function Cart() {
  const { items, remove, clear } = useCart();
  const { account, signAndSubmitTransaction } = useWallet();
  const [txHash, setTxHash] = useState("");
  const [err, setErr] = useState("");

  async function sweep() {
    setErr("");
    setTxHash("");
    try {
      if (!account) throw new Error("connect a wallet first");
      const entry = await api.txSweep({
        venues: items.map((i) => i.venue),
        listings: items.map((i) => i.listing_id),
        prices: items.map((i) => String(i.price)),
        v1_creators: items.map(() => ""),
        v1_collections: items.map(() => ""),
        v1_names: items.map(() => ""),
        v1_property_versions: items.map(() => ""),
        sender: account.address.toString(),
      });
      const res = await signAndSubmitTransaction({
        data: {
          function: entry.function as `${string}::${string}::${string}`,
          typeArguments: entry.type_arguments,
          functionArguments: entry.arguments as never[],
        },
      });
      setTxHash(res.hash);
      clear();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  }

  const total = items.reduce((s, i) => s + i.price, 0);
  return (
    <div>
      <h2>Cart — one atomic sweep</h2>
      <ul>
        {items.map((i) => (
          <li key={i.listing_id}>
            {i.token_name} — {octasToApt(i.price)} APT ({i.marketplace}){" "}
            <button onClick={() => remove(i.listing_id)}>remove</button>
          </li>
        ))}
      </ul>
      <p>Total: {octasToApt(total)} APT</p>
      <button disabled={items.length === 0} onClick={() => void sweep()}>
        Sweep via router
      </button>
      {txHash && (
        <p>
          Submitted: <code>{txHash}</code>
        </p>
      )}
      {err && <p style={{ color: "red" }}>{err}</p>}
    </div>
  );
}
