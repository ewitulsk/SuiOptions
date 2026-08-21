import { useEffect, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { ConnectButton, useCurrentAccount } from "@mysten/dapp-kit";

import * as auth from "../api/auth";
import { ErrorBox, Panel } from "../components/ui";
import { homeFor, useSession } from "../state/session";
import { useWalletProof } from "./Login";

/** Redeem an invite into an account.
 *
 *  The invite carries the role and scope; nothing the visitor types here
 *  influences what they end up being able to see. */
export default function Signup() {
  const [params] = useSearchParams();
  const invite = params.get("invite") ?? "";
  const { setSession } = useSession();
  const navigate = useNavigate();
  const account = useCurrentAccount();
  const proveWallet = useWalletProof();

  const [preview, setPreview] = useState<Awaited<ReturnType<typeof auth.previewInvite>> | null>(null);
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!invite) return;
    auth.previewInvite(invite).then(setPreview, setError);
  }, [invite]);

  const run = async (fn: () => Promise<auth.Session>) => {
    setBusy(true);
    setError(null);
    try {
      const s = await fn();
      setSession(s);
      navigate(homeFor(s.role), { replace: true });
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  if (!invite) {
    return (
      <div className="centered">
        <div className="card">
          <ErrorBox error="This link is missing its invite code." />
        </div>
      </div>
    );
  }

  const mismatch = confirm.length > 0 && confirm !== password;
  const tooShort = password.length > 0 && password.length < 12;

  return (
    <div className="centered">
      <div className="card">
        <h2>Create your account</h2>
        <ErrorBox error={error} />

        {preview && !preview.valid && (
          <ErrorBox error={`This invite is ${preview.reason}. Ask for a fresh link.`} />
        )}
        {preview?.valid && (
          <p className="muted">
            Joining as <strong>{preview.role}</strong>
            {preview.label ? ` — ${preview.label}` : ""}.
          </p>
        )}

        <Panel
          title="Username and password"
          hint="Pick any handle you like — it is not an email address, and we never ask for one. That also means there is no password reset: if you lose it, you need a new invite."
        >
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void run(() => auth.register(invite, { username, password }));
            }}
          >
            <label>
              <span>Username</span>
              <input
                autoComplete="username"
                value={username}
                onChange={(e) => setUsername(e.target.value)}
              />
            </label>
            <label>
              <span>Password (at least 12 characters)</span>
              <input
                type="password"
                autoComplete="new-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
            </label>
            <label>
              <span>Confirm password</span>
              <input
                type="password"
                autoComplete="new-password"
                value={confirm}
                onChange={(e) => setConfirm(e.target.value)}
              />
            </label>
            {tooShort && <p className="muted">Needs at least 12 characters.</p>}
            {mismatch && <p className="muted">Passwords do not match.</p>}
            <button disabled={busy || !username || tooShort || mismatch || !password}>
              Create account
            </button>
          </form>
        </Panel>

        <Panel title="Or use a Sui wallet" hint="You can add a password later from Settings.">
          <div className="actions" style={{ flexDirection: "column", alignItems: "stretch" }}>
            <ConnectButton />
            <button
              className="secondary"
              disabled={busy || !account}
              onClick={() =>
                void run(async () => auth.register(invite, await proveWallet()))
              }
            >
              Create account with wallet
            </button>
          </div>
        </Panel>
      </div>
    </div>
  );
}
