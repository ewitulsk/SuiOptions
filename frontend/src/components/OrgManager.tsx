// Verified-orgs allowlist panel (lives on /admin, platform-admin only).
//
// Lists every allowlist row from token-info and lets the platform admin
// verify new orgs, toggle enabled, or remove rows. Mutations require the
// admin JWT (same useAdminAuth challenge flow as the token catalog).
// "Verified" = present AND enabled — this is the set api-service /buckets,
// the quoting service, and this frontend serve.

import { useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";
import { useQuery, useQueryClient } from "@tanstack/react-query";

import { useAdminAuth } from "../api/useAdminAuth";
import { AuthExpiredError } from "../api/tokenAdmin";
import {
  deleteVerifiedOrg,
  listVerifiedOrgs,
  setOrgEnabled,
  verifyOrg,
  type VerifiedOrgDto,
} from "../api/orgAdmin";

export function OrgManager({ flash }: { flash: (msg: string) => void }) {
  const account = useCurrentAccount();
  const auth = useAdminAuth();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState<string | null>(null);

  const orgs = useQuery<VerifiedOrgDto[], Error>({
    queryKey: ["verified-orgs-admin"],
    queryFn: listVerifiedOrgs,
  });

  const refetch = () => {
    queryClient.invalidateQueries({ queryKey: ["verified-orgs-admin"] });
    queryClient.invalidateQueries({ queryKey: ["verified-orgs"] });
    queryClient.invalidateQueries({ queryKey: ["buckets"] });
  };

  const runAuthed = async (
    key: string,
    fn: (token: string) => Promise<void>,
    ok: string,
  ) => {
    setBusy(key);
    try {
      const token = await auth.getValidToken();
      await fn(token);
      flash(`✓ ${ok}`);
      refetch();
    } catch (e) {
      if (e instanceof AuthExpiredError) {
        auth.signOut();
        flash("session expired · sign in again");
      } else {
        flash(`failed · ${e instanceof Error ? e.message : String(e)}`);
      }
    } finally {
      setBusy(null);
    }
  };

  return (
    <section className="admin-section">
      <div className="admin-section__head">
        <h2 className="admin-section__title">Verified orgs</h2>
        <div className="admin-section__sub">
          orgs are permissionless on chain — only allowlisted (verified) orgs'
          buckets are served by the API, quoting, and this app.
        </div>
      </div>

      {!auth.isSignedIn && (
        <div className="admin-actions-row" style={{ marginBottom: 16 }}>
          <button
            className="admin-btn admin-btn--primary"
            disabled={!account || auth.busy}
            onClick={() =>
              auth
                .signIn()
                .then(() => flash("✓ signed in"))
                .catch((e) =>
                  flash(`sign-in failed · ${e instanceof Error ? e.message : String(e)}`),
                )
            }
          >
            {auth.busy ? "signing…" : "Sign in to manage orgs"}
          </button>
        </div>
      )}

      {orgs.isLoading && <div className="admin-empty">loading orgs…</div>}
      {orgs.error && (
        <div className="admin-empty">failed to load orgs · {orgs.error.message}</div>
      )}
      {orgs.data && orgs.data.length === 0 && (
        <div className="admin-empty">no verified orgs yet.</div>
      )}

      {orgs.data && orgs.data.length > 0 && (
        <div className="admin-table">
          <div className="admin-table__head admin-table__row">
            <span>Name</span>
            <span>Org id</span>
            <span>Status</span>
            <span>Actions</span>
          </div>
          {orgs.data.map((o) => (
            <div className="admin-table__row" key={o.org_id}>
              <span>{o.name}</span>
              <span>
                <code className="admin-cell__id">
                  {o.org_id.slice(0, 8)}…{o.org_id.slice(-6)}
                </code>
              </span>
              <span>
                {o.enabled ? (
                  <span className="admin-tag admin-tag--ok">verified</span>
                ) : (
                  <span className="admin-tag admin-tag--mute">disabled</span>
                )}
              </span>
              <span className="admin-cell__actions">
                <button
                  className="admin-btn"
                  disabled={!auth.isSignedIn || busy !== null}
                  onClick={() =>
                    runAuthed(
                      `tog-${o.org_id}`,
                      (token) => setOrgEnabled(token, o, !o.enabled).then(() => {}),
                      o.enabled ? `disabled ${o.name}` : `re-enabled ${o.name}`,
                    )
                  }
                >
                  {busy === `tog-${o.org_id}` ? "…" : o.enabled ? "Disable" : "Enable"}
                </button>
                <button
                  className="admin-btn admin-btn--danger"
                  disabled={!auth.isSignedIn || busy !== null}
                  onClick={() =>
                    runAuthed(
                      `del-${o.org_id}`,
                      (token) => deleteVerifiedOrg(token, o.org_id),
                      `removed ${o.name}`,
                    )
                  }
                >
                  {busy === `del-${o.org_id}` ? "…" : "Remove"}
                </button>
              </span>
            </div>
          ))}
        </div>
      )}

      <VerifyOrgForm
        disabled={!auth.isSignedIn}
        busy={busy === "verify"}
        onVerify={(input) =>
          runAuthed(
            "verify",
            (token) => verifyOrg(token, input).then(() => {}),
            `verified ${input.name}`,
          )
        }
      />
    </section>
  );
}

function VerifyOrgForm({
  disabled,
  busy,
  onVerify,
}: {
  disabled: boolean;
  busy: boolean;
  onVerify: (input: VerifiedOrgDto) => void;
}) {
  const [orgId, setOrgId] = useState("");
  const [name, setName] = useState("");
  const ready = orgId.startsWith("0x") && name.length > 0;

  return (
    <div style={{ marginTop: 20 }}>
      <div className="admin-section__sub" style={{ marginBottom: 8 }}>
        Verify an org
      </div>
      <div className="admin-grid">
        <div className="admin-field">
          <label className="admin-field__label">Org object id</label>
          <input
            className="admin-field__input"
            placeholder="0x…"
            value={orgId}
            onChange={(e) => setOrgId(e.target.value.trim())}
          />
        </div>
        <div className="admin-field">
          <label className="admin-field__label">Display name</label>
          <input
            className="admin-field__input"
            placeholder="Acme Options"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </div>
      </div>
      <button
        className="admin-btn admin-btn--primary"
        disabled={disabled || !ready || busy}
        title={disabled ? "Sign in to verify orgs" : undefined}
        onClick={() => onVerify({ org_id: orgId, name, enabled: true })}
      >
        {busy ? "verifying…" : "Verify org"}
      </button>
    </div>
  );
}
