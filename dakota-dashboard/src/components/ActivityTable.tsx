import type { AssetTotal, CustomerFlow, LedgerEvent } from "../api/dakota";
import { formatMinor } from "../api/dakota";
import { Empty, Panel, StatusPill, Table, fmtTime, shortId } from "./ui";

/** Platform- or roster-wide totals per asset. */
export function TotalsPanel({ totals }: { totals: AssetTotal[] }) {
  return (
    <Panel title="Totals" hint="Derived from webhook-recorded events, in whole units.">
      {totals.length === 0 ? (
        <Empty>No settled activity yet.</Empty>
      ) : (
        <Table
          head={
            <tr>
              <th>Asset</th>
              <th className="num">In</th>
              <th className="num">Out</th>
              <th className="num">Net</th>
              <th className="num">Events</th>
            </tr>
          }
        >
          {totals.map((t) => (
            <tr key={t.asset}>
              <td>{t.asset}</td>
              <td className="num">{formatMinor(t.inbound_minor)}</td>
              <td className="num">{formatMinor(t.outbound_minor)}</td>
              <td className="num">{formatMinor(t.inbound_minor - t.outbound_minor)}</td>
              <td className="num">{t.events}</td>
            </tr>
          ))}
        </Table>
      )}
    </Panel>
  );
}

export function FlowsTable({
  flows,
  onSelect,
}: {
  flows: CustomerFlow[];
  onSelect?: (customerId: string) => void;
}) {
  // The LEFT JOIN emits a null-asset row for a customer with no activity;
  // showing it as a blank line is more honest than dropping the customer.
  return (
    <Panel title="By customer">
      {flows.length === 0 ? (
        <Empty>No customers yet.</Empty>
      ) : (
        <Table
          head={
            <tr>
              <th>Customer</th>
              <th>Type</th>
              <th>Asset</th>
              <th className="num">In</th>
              <th className="num">Out</th>
              <th className="num">Events</th>
            </tr>
          }
        >
          {flows.map((f, i) => (
            <tr
              key={`${f.dakota_customer_id}-${f.asset ?? "none"}-${i}`}
              onClick={() => onSelect?.(f.dakota_customer_id)}
              style={onSelect ? { cursor: "pointer" } : undefined}
            >
              <td className="mono">{shortId(f.dakota_customer_id)}</td>
              <td>{f.customer_type}</td>
              <td>{f.asset ?? <span className="muted">no activity</span>}</td>
              <td className="num">{formatMinor(f.inbound_minor)}</td>
              <td className="num">{formatMinor(f.outbound_minor)}</td>
              <td className="num">{f.events}</td>
            </tr>
          ))}
        </Table>
      )}
    </Panel>
  );
}

export function EventFeed({ events, title = "Activity" }: { events: LedgerEvent[]; title?: string }) {
  return (
    <Panel
      title={title}
      hint="Events arrive out of order; the resource's own status is authoritative, not the newest row."
    >
      {events.length === 0 ? (
        <Empty>Nothing recorded yet. Webhooks populate this as transfers settle.</Empty>
      ) : (
        <Table
          head={
            <tr>
              <th>When</th>
              <th>Event</th>
              <th>Customer</th>
              <th>Dir</th>
              <th className="num">Amount</th>
              <th>Asset</th>
              <th>Rate</th>
              <th>Status</th>
            </tr>
          }
        >
          {events.map((e) => (
            <tr key={e.event_id}>
              <td>{fmtTime(e.occurred_at)}</td>
              <td className="mono">{e.event_type}</td>
              <td className="mono">{shortId(e.dakota_customer_id)}</td>
              <td>{e.direction ?? "—"}</td>
              <td className="num">{formatMinor(e.amount_minor)}</td>
              <td>{e.asset ?? "—"}</td>
              <td className="mono">{e.exchange_rate ?? "—"}</td>
              <td>
                <StatusPill status={e.status} />
              </td>
            </tr>
          ))}
        </Table>
      )}
    </Panel>
  );
}
