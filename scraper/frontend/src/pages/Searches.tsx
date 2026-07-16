import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { FormEvent, useState } from "react";

import { ApiError, SavedSearch, get, post, put } from "../api";

const EMPTY = {
  source: "ebay",
  name: "",
  query: "",
  category: "",
  min_price: "",
  max_price: "",
  poll_interval_seconds: 300,
  alert_threshold: 1.0,
  active: true,
};

export default function Searches() {
  const queryClient = useQueryClient();
  const searches = useQuery<SavedSearch[]>({
    queryKey: ["searches"],
    queryFn: () => get<SavedSearch[]>("/api/searches"),
  });
  const [form, setForm] = useState(EMPTY);

  const create = useMutation({
    mutationFn: () =>
      post("/api/searches", {
        ...form,
        category: form.category || null,
        min_price: form.min_price || null,
        max_price: form.max_price || null,
      }),
    onSuccess: () => {
      setForm(EMPTY);
      queryClient.invalidateQueries({ queryKey: ["searches"] });
    },
  });

  const toggle = useMutation({
    mutationFn: (s: SavedSearch) =>
      put(`/api/searches/${s.id}`, {
        source: s.source,
        name: s.name,
        query: s.query,
        category: s.category,
        min_price: s.min_price,
        max_price: s.max_price,
        poll_interval_seconds: s.poll_interval_seconds,
        alert_threshold: s.alert_threshold,
        active: !s.active,
      }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["searches"] }),
  });

  const submit = (e: FormEvent) => {
    e.preventDefault();
    create.mutate();
  };

  return (
    <>
      <h1>Saved searches</h1>
      <table>
        <thead>
          <tr>
            <th>Name</th>
            <th>Source</th>
            <th>Query</th>
            <th>Price range</th>
            <th>Poll</th>
            <th>Last polled</th>
            <th>Active</th>
          </tr>
        </thead>
        <tbody>
          {searches.data?.map((s) => (
            <tr key={s.id}>
              <td>{s.name}</td>
              <td>{s.source}</td>
              <td>{s.query}</td>
              <td className="muted">
                {s.min_price ?? "–"} … {s.max_price ?? "–"}
              </td>
              <td className="muted">{s.poll_interval_seconds}s</td>
              <td className="muted">
                {s.last_polled_at ? new Date(s.last_polled_at).toLocaleString() : "never"}
              </td>
              <td>
                <button className="secondary" onClick={() => toggle.mutate(s)}>
                  {s.active ? "Pause" : "Resume"}
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      <h2>New search</h2>
      <form className="inline-form" onSubmit={submit}>
        <label>
          Name
          <input
            required
            value={form.name}
            onChange={(e) => setForm({ ...form, name: e.target.value })}
          />
        </label>
        <label>
          Source
          <select
            value={form.source}
            onChange={(e) => setForm({ ...form, source: e.target.value })}
          >
            <option value="ebay">eBay</option>
          </select>
        </label>
        <label>
          Query
          <input
            required
            value={form.query}
            onChange={(e) => setForm({ ...form, query: e.target.value })}
          />
        </label>
        <label>
          Min $
          <input
            type="number"
            value={form.min_price}
            onChange={(e) => setForm({ ...form, min_price: e.target.value })}
            style={{ width: 90 }}
          />
        </label>
        <label>
          Max $
          <input
            type="number"
            value={form.max_price}
            onChange={(e) => setForm({ ...form, max_price: e.target.value })}
            style={{ width: 90 }}
          />
        </label>
        <label>
          Poll every (s)
          <input
            type="number"
            min={60}
            value={form.poll_interval_seconds}
            onChange={(e) => setForm({ ...form, poll_interval_seconds: +e.target.value })}
            style={{ width: 110 }}
          />
        </label>
        <button disabled={create.isPending}>Add search</button>
        {create.isError && (
          <span className="error">
            {create.error instanceof ApiError ? create.error.message : "Failed"}
          </span>
        )}
      </form>
    </>
  );
}
