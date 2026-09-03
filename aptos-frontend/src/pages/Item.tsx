import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useParams } from "react-router-dom";
import { useWallet } from "@aptos-labs/wallet-adapter-react";
import { api, octasToApt, venueName } from "../api";
import { VENUES } from "../config";
import { useCart } from "../cart";

export default function Item() {
  const { id } = useParams();
  const { account, signAndSubmitTransaction } = useWallet();
  const { add } = useCart();
  const [txHash, setTxHash] = useState("");
  const [err, setErr] = useState("");
  const { data } = useQuery({
    queryKey: ["item", id],
    queryFn: () => api.item(id ?? ""),
  });
  const l = data?.listing;

  async function buy() {
    setErr("");
    setTxHash("");
    try {
      if (!account || !l) throw new Error("connect a wallet first");
      const venue = VENUES.find((v) => v.slug === l.marketplace)?.id;
      if (!venue) throw new Error(`unknown venue ${l.marketplace}`);
      const entry = await api.txBuy({
        venue,
        standard: "v2",
        args: [l.listing_id, String(l.price)],
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
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  }

  if (!data) return <p>Loading…</p>;
  return (
    <div>
      <h2>{l?.token_name ?? id}</h2>
      {l ? (
        <>
          <p>
            {octasToApt(l.price)} APT · {venueName(l.marketplace)} · seller{" "}
            <code>{l.seller.slice(0, 12)}…</code>
          </p>
          <button onClick={() => void buy()}>Buy now</button>{" "}
          <button
            onClick={() =>
              add({
                ...l,
                venue: VENUES.find((v) => v.slug === l.marketplace)?.id ?? 0,
                standard: "v2",
              })
            }
          >
            Add to cart
          </button>
        </>
      ) : (
        <p>No live listing — see history below.</p>
      )}
      {txHash && (
        <p>
          Submitted: <code>{txHash}</code>
        </p>
      )}
      {err && <p style={{ color: "red" }}>{err}</p>}
      <h3>History</h3>
      <ul>
        {data.activities.map((a: unknown, i: number) => (
          <li key={i}>
            <code>{JSON.stringify(a)}</code>
          </li>
        ))}
      </ul>
    </div>
  );
}
