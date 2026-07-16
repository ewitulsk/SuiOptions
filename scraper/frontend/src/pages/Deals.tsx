import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { FormEvent, useState } from "react";

import { ApiError, Deal, DealStats, get, post } from "../api";

function money(value: string | null): string {
  return value == null ? "–" : `$${Number(value).toFixed(2)}`;
}

function StatsHeader() {
  const stats = useQuery<DealStats>({
    queryKey: ["deals", "stats"],
    queryFn: () => get<DealStats>("/api/deals/stats"),
  });
  if (!stats.data) return null;
  const s = stats.data;
  return (
    <>
      <div className="stat-grid">
        <div className="stat">
          <div className="label">Realized P&L (all time)</div>
          <div className={`value ${Number(s.realized_profit_all_time) >= 0 ? "green" : "red"}`}>
            {money(s.realized_profit_all_time)}
          </div>
        </div>
        <div className="stat">
          <div className="label">Realized P&L (30d)</div>
          <div className="value">{money(s.realized_profit_30d)}</div>
        </div>
        <div className="stat">
          <div className="label">Capital tied up</div>
          <div className="value">{money(s.capital_tied_up)}</div>
        </div>
        <div className="stat">
          <div className="label">Win rate</div>
          <div className="value">
            {s.deals_sold ? `${(s.win_rate * 100).toFixed(0)}%` : "–"}
          </div>
        </div>
        <div className="stat">
          <div className="label">Avg days to sell</div>
          <div className="value">
            {s.avg_days_to_sell != null ? s.avg_days_to_sell.toFixed(1) : "–"}
          </div>
        </div>
      </div>
      {s.per_user.length > 0 && (
        <p className="muted">
          {s.per_user
            .map((u) => `${u.username}: ${u.deals_bought} deals, ${money(u.realized_profit)}`)
            .join(" · ")}
        </p>
      )}
    </>
  );
}

function BoughtForm({ deal, onDone }: { deal: Deal; onDone: () => void }) {
  const [buyPrice, setBuyPrice] = useState("");
  const [extraCosts, setExtraCosts] = useState("0");
  const mutation = useMutation({
    mutationFn: () =>
      post(`/api/deals/${deal.id}/bought`, {
        buy_price: buyPrice,
        buy_extra_costs: extraCosts || "0",
      }),
    onSuccess: onDone,
  });
  const submit = (e: FormEvent) => {
    e.preventDefault();
    mutation.mutate();
  };
  return (
    <form className="inline-form" onSubmit={submit}>
      <label>
        Actual buy price $
        <input type="number" step="0.01" required value={buyPrice}
          onChange={(e) => setBuyPrice(e.target.value)} style={{ width: 110 }} />
      </label>
      <label>
        Extra costs $ (gas, parts…)
        <input type="number" step="0.01" value={extraCosts}
          onChange={(e) => setExtraCosts(e.target.value)} style={{ width: 110 }} />
      </label>
      <button disabled={mutation.isPending}>Save buy</button>
      {mutation.isError && (
        <span className="error">
          {mutation.error instanceof ApiError ? mutation.error.message : "Failed"}
        </span>
      )}
    </form>
  );
}

function SoldForm({ deal, onDone }: { deal: Deal; onDone: () => void }) {
  const [salePrice, setSalePrice] = useState("");
  const [fees, setFees] = useState("0");
  const [channel, setChannel] = useState("");
  const mutation = useMutation({
    mutationFn: () =>
      post(`/api/deals/${deal.id}/sold`, {
        sale_price: salePrice,
        sale_fees: fees || "0",
        sale_channel: channel || null,
      }),
    onSuccess: onDone,
  });
  const submit = (e: FormEvent) => {
    e.preventDefault();
    mutation.mutate();
  };
  return (
    <form className="inline-form" onSubmit={submit}>
      <label>
        Actual sale price $
        <input type="number" step="0.01" required value={salePrice}
          onChange={(e) => setSalePrice(e.target.value)} style={{ width: 110 }} />
      </label>
      <label>
        Fees + shipping $
        <input type="number" step="0.01" value={fees}
          onChange={(e) => setFees(e.target.value)} style={{ width: 110 }} />
      </label>
      <label>
        Channel
        <input placeholder="eBay, FB, local…" value={channel}
          onChange={(e) => setChannel(e.target.value)} style={{ width: 130 }} />
      </label>
      <button disabled={mutation.isPending}>Save sale</button>
      {mutation.isError && (
        <span className="error">
          {mutation.error instanceof ApiError ? mutation.error.message : "Failed"}
        </span>
      )}
    </form>
  );
}

function DealRow({ deal }: { deal: Deal }) {
  const queryClient = useQueryClient();
  const [open, setOpen] = useState<"bought" | "sold" | null>(null);
  const refresh = () => {
    setOpen(null);
    queryClient.invalidateQueries({ queryKey: ["deals"] });
  };
  const profit = deal.net_profit != null ? Number(deal.net_profit) : null;

  return (
    <div className="card">
      <div className="row">
        <strong>{deal.title}</strong>
        <span className={`badge ${deal.status}`}>{deal.status}</span>
        <span className="spacer" style={{ flex: 1 }} />
        {profit != null && (
          <strong className={profit >= 0 ? "green" : "red"}>
            {profit >= 0 ? "+" : ""}${profit.toFixed(2)}
          </strong>
        )}
      </div>
      <div className="row muted" style={{ marginTop: 6 }}>
        <span>Bought: {money(deal.buy_price)}{Number(deal.buy_extra_costs) > 0 && ` (+${money(deal.buy_extra_costs)} costs)`}</span>
        <span>
          Sold: {money(deal.sale_price)}
          {Number(deal.sale_fees) > 0 && ` (−${money(deal.sale_fees)} fees)`}
          {deal.sale_channel && ` via ${deal.sale_channel}`}
        </span>
        {deal.notes && <span>“{deal.notes}”</span>}
      </div>
      <div className="row" style={{ marginTop: 10 }}>
        {deal.status !== "sold" && (
          <button className="secondary" onClick={() => setOpen(open === "bought" ? null : "bought")}>
            {deal.buy_price == null ? "Mark bought" : "Edit buy"}
          </button>
        )}
        {deal.buy_price != null && (
          <button className="secondary" onClick={() => setOpen(open === "sold" ? null : "sold")}>
            {deal.status === "sold" ? "Edit sale" : "Mark sold"}
          </button>
        )}
      </div>
      {open === "bought" && <BoughtForm deal={deal} onDone={refresh} />}
      {open === "sold" && <SoldForm deal={deal} onDone={refresh} />}
    </div>
  );
}

export default function Deals() {
  const queryClient = useQueryClient();
  const deals = useQuery<Deal[]>({
    queryKey: ["deals", "list"],
    queryFn: () => get<Deal[]>("/api/deals"),
  });
  const [manualTitle, setManualTitle] = useState("");
  const createManual = useMutation({
    mutationFn: () => post("/api/deals", { title: manualTitle }),
    onSuccess: () => {
      setManualTitle("");
      queryClient.invalidateQueries({ queryKey: ["deals"] });
    },
  });

  return (
    <>
      <h1>Deals / P&L</h1>
      <StatsHeader />
      <form
        className="inline-form"
        style={{ marginBottom: 16 }}
        onSubmit={(e) => {
          e.preventDefault();
          createManual.mutate();
        }}
      >
        <label>
          Off-platform find (manual deal)
          <input placeholder="e.g. Garage sale toolbox" value={manualTitle}
            onChange={(e) => setManualTitle(e.target.value)} style={{ width: 260 }} />
        </label>
        <button disabled={!manualTitle || createManual.isPending}>Add deal</button>
      </form>
      {deals.data?.length === 0 && (
        <p className="muted">No deals yet — track one from the deal feed.</p>
      )}
      {deals.data?.map((d) => <DealRow key={d.id} deal={d} />)}
    </>
  );
}
