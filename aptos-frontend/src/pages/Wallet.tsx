import { useQuery } from "@tanstack/react-query";
import { useWallet } from "@aptos-labs/wallet-adapter-react";
import { Link } from "react-router-dom";
import { api, octasToApt } from "../api";

export default function WalletPage() {
  const { account, connected } = useWallet();
  const addr = account?.address.toString() ?? "";
  const { data } = useQuery({
    queryKey: ["wallet", addr],
    queryFn: () => api.listings({ seller: addr, limit: "100" }),
    enabled: connected && !!addr,
  });
  if (!connected) return <p>Connect a wallet to see your listings.</p>;
  return (
    <div>
      <h2>Your listings</h2>
      <code>{addr}</code>
      <ul>
        {(data ?? []).map((l) => (
          <li key={l.listing_id}>
            <Link to={`/items/${l.token_data_id || l.listing_id}`}>
              {l.token_name}
            </Link>{" "}
            — {octasToApt(l.price)} APT · {l.marketplace}
          </li>
        ))}
      </ul>
    </div>
  );
}
