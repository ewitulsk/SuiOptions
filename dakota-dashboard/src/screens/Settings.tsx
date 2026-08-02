import { useEffect, useState } from "react";
import { ConnectButton, useCurrentAccount } from "@mysten/dapp-kit";

import * as auth from "../api/auth";
import { Empty, ErrorBox, Panel, Table, fmtTime } from "../components/ui";
import { useAuthed, useSession } from "../state/session";
import { useWalletProof } from "./Login";

/** Manage the login methods attached to this account.
 *
 *  Both directions of linking live here: a wallet account adding a password,
 *  and a password account adding a wallet. Either one then signs you in — they
 *  are two doors onto the same account, not two accounts. */
export default function Settings() {
  const { token } = useAuthed();
  const { setSession } = useSession();
  const account = useCurrentAccount();
  const proveWallet = useWalletProof();

  const [me, setMe] = useState<auth.Me | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [ok, setOk] = useState<string | null>(null);
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);

  const reload = () => {
    auth.me(token).then(setMe, setError);
  };
  useEffect(reload, [token]);

  const run = async (fn: () => Promise<unknown>, message: string) => {
    setBusy(true);
    setError(null);
    setOk(null);
    try {
      await fn();
      setOk(message);
      reload();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  const hasPassword = me?.identities.some((i) => i.kind === "password");
  const hasWallet = me?.identities.some((i) => i.kind === "sui_wallet");

  return (
    <>
      <h2>Security</h2>
      <ErrorBox error={error} />
      {ok && <div className="success">{ok}</div>}

      <Panel title="Account">
        <p className="muted">
          {me ? (
            <>
              <code>{me.user_id}</code> · role <strong>{me.role}</strong>
              {me.scope ? (
                <>
                  {" "}
                  · scoped to <code>{me.scope}</code>
                </>
              ) : null}
            </>
          ) : (
            "Loading…"
          )}
        </p>
      </Panel>

      <Panel title="Login methods" hint="Any one of these signs you into this account.">
        {me?.identities.length ? (
          <Table
            head={
              <tr>
                <th>Method</th>
                <th>Identifier</th>
                <th>Added</th>
                <th>Last used</th>
                <th></th>
              </tr>
            }
          >
            {me.identities.map((i) => (
              <tr key={i.id}>
                <td>{i.kind === "password" ? "Password" : "Sui wallet"}</td>
                <td className="mono">{i.identifier}</td>
                <td>{fmtTime(i.created_at)}</td>
                <td>{fmtTime(i.last_used_at)}</td>
                <td>
                  <button
                    className="secondary"
                    // The server refuses the last one anyway; disabling it here
                    // saves an error the user cannot act on.
                    disabled={busy || me.identities.length <= 1}
                    title={
                      me.identities.length <= 1
                        ? "You cannot remove your only way in — there is no password reset."
                        : undefined
                    }
                    onClick={() =>
                      void run(() => auth.removeIdentity(token, i.id), "Removed.")
                    }
                  >
                    Remove
                  </button>
                </td>
              </tr>
            ))}
          </Table>
        ) : (
          <Empty>Loading…</Empty>
        )}
      </Panel>

      {!hasPassword && (
        <Panel title="Add a password" hint="Lets you sign in without a wallet.">
          <div className="row">
            <label>
              <span>Username</span>
              <input value={username} onChange={(e) => setUsername(e.target.value)} />
            </label>
            <label>
              <span>Password (12+ characters)</span>
              <input
                type="password"
                autoComplete="new-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
            </label>
          </div>
          <button
            disabled={busy || !username || password.length < 12}
            onClick={() =>
              void run(
                () => auth.addIdentity(token, { username, password }),
                "Password added — you can now sign in with it.",
              )
            }
          >
            Add password
          </button>
        </Panel>
      )}

      {!hasWallet && (
        <Panel
          title="Add a Sui wallet"
          hint="You will be asked to sign a one-time challenge, which is how we know the wallet is really yours."
        >
          <div className="actions" style={{ flexDirection: "column", alignItems: "stretch" }}>
            <ConnectButton />
            <button
              disabled={busy || !account}
              onClick={() =>
                void run(
                  async () => auth.addIdentity(token, await proveWallet()),
                  "Wallet added — you can now sign in with it.",
                )
              }
            >
              Add wallet
            </button>
          </div>
        </Panel>
      )}

      <Panel title="Sign out">
        <button className="secondary" onClick={() => setSession(null)}>
          Sign out
        </button>
      </Panel>
    </>
  );
}
