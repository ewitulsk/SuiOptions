import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";

import * as api from "../api/dakota";
import type { Customer } from "../api/dakota";
import { CopyField, Empty, ErrorBox, Panel, StatusPill, Table, fmtTime, shortId } from "../components/ui";
import { useAuthed } from "../state/session";

/** Customer roster + creation.
 *
 *  Shared by the admin and business roles: the service scopes the list off the
 *  token, so a business sees exactly its own customers without this screen
 *  filtering anything. `canCreateBusiness` is the one genuine difference —
 *  only an admin can mint a partner business. */
export default function Customers({ canCreateBusiness }: { canCreateBusiness: boolean }) {
  const { token, role } = useAuthed();
  const qc = useQueryClient();
  const customers = useQuery({
    queryKey: ["customers"],
    queryFn: () => api.listCustomers(token),
  });

  const [name, setName] = useState("");
  const [ref, setRef] = useState("");
  const [type, setType] = useState<"individual" | "business">("individual");
  const [isSubClient, setIsSubClient] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [created, setCreated] = useState<api.CreateCustomerResult | null>(null);
  const [inviteFor, setInviteFor] = useState<{ id: string; invite: api.Invite } | null>(null);

  const create = async () => {
    setBusy(true);
    setError(null);
    setCreated(null);
    try {
      setCreated(
        await api.createCustomer(token, {
          name,
          customer_type: isSubClient ? "business" : type,
          external_ref: ref || undefined,
          is_sub_client: isSubClient || undefined,
          with_invite: true,
        }),
      );
      setName("");
      setRef("");
      await qc.invalidateQueries({ queryKey: ["customers"] });
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  const approve = async (id: string) => {
    setError(null);
    try {
      await api.simulateOnboarding(token, id);
      await qc.invalidateQueries({ queryKey: ["customers"] });
    } catch (e) {
      setError(e);
    }
  };

  const invite = async (id: string) => {
    setError(null);
    try {
      setInviteFor({ id, invite: await api.createInvite(token, id) });
    } catch (e) {
      setError(e);
    }
  };

  return (
    <>
      <h2>Customers</h2>
      <ErrorBox error={error} />

      <Panel
        title="Create a customer"
        hint="We collect a name and a reference, then hand off to Dakota's hosted form. Beneficial owners, documents and SSNs are entered there and never touch this console."
      >
        <div className="row">
          <label>
            <span>Legal name</span>
            <input value={name} onChange={(e) => setName(e.target.value)} />
          </label>
          <label>
            <span>Your reference (optional)</span>
            <input value={ref} onChange={(e) => setRef(e.target.value)} />
          </label>
          {!isSubClient && (
            <label>
              <span>Type</span>
              <select value={type} onChange={(e) => setType(e.target.value as typeof type)}>
                <option value="individual">Individual</option>
                <option value="business">Business</option>
              </select>
            </label>
          )}
        </div>

        {canCreateBusiness && (
          <label style={{ display: "flex", gap: 8, alignItems: "center" }}>
            <input
              type="checkbox"
              style={{ width: "auto" }}
              checked={isSubClient}
              onChange={(e) => setIsSubClient(e.target.checked)}
            />
            <span style={{ margin: 0 }}>
              Partner business — gets its own customers beneath it. Immutable after creation.
            </span>
          </label>
        )}

        <div className="actions">
          <button disabled={busy || !name} onClick={() => void create()}>
            {busy ? "Creating…" : "Create"}
          </button>
        </div>
      </Panel>

      {created && (
        <Panel title="Send these to the customer">
          <div className="success">
            Created <code>{created.customer.dakota_customer_id}</code>.
          </div>
          <CopyField label="Dakota onboarding form (KYB/KYC)" value={created.application_url} />
          {created.invite && (
            <CopyField
              label="Console signup link"
              value={`${window.location.origin}/signup?invite=${created.invite.invite_id}`}
            />
          )}
          <p className="muted">
            The Dakota link collects the verification data. The console link lets them sign in
            here afterwards to run ramps.
          </p>
        </Panel>
      )}

      {inviteFor && (
        <Panel title="Signup link">
          <CopyField
            label={`For ${shortId(inviteFor.id)}`}
            value={`${window.location.origin}/signup?invite=${inviteFor.invite.invite_id}`}
          />
          <p className="muted">Expires {fmtTime(inviteFor.invite.expires_at)}. Single use.</p>
        </Panel>
      )}

      <Panel title="Roster">
        {customers.isLoading ? (
          <Empty>Loading…</Empty>
        ) : customers.data?.length ? (
          <Table
            head={
              <tr>
                <th>Id</th>
                <th>Ref</th>
                <th>Type</th>
                <th>KYB</th>
                <th>Application</th>
                <th>Created</th>
                <th></th>
              </tr>
            }
          >
            {customers.data.map((c: Customer) => (
              <tr key={c.dakota_customer_id}>
                <td className="mono">{shortId(c.dakota_customer_id)}</td>
                <td>{c.external_ref ?? "—"}</td>
                <td>
                  {c.customer_type}
                  {c.is_sub_client ? " (partner)" : ""}
                </td>
                <td>
                  <StatusPill status={c.kyb_status} />
                </td>
                <td>
                  <StatusPill status={c.application_status} />
                </td>
                <td>{fmtTime(c.created_at)}</td>
                <td>
                  <div className="actions">
                    <button className="secondary" onClick={() => void invite(c.dakota_customer_id)}>
                      Invite
                    </button>
                    {role === "admin" && c.kyb_status !== "active" && (
                      <button
                        className="secondary"
                        title="Sandbox only: runs the kyb_approve simulation"
                        onClick={() => void approve(c.dakota_customer_id)}
                      >
                        Approve
                      </button>
                    )}
                  </div>
                </td>
              </tr>
            ))}
          </Table>
        ) : (
          <Empty>No customers yet.</Empty>
        )}
      </Panel>
    </>
  );
}
