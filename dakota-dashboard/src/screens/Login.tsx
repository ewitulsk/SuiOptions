import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { ConnectButton, useCurrentAccount, useSignPersonalMessage } from "@mysten/dapp-kit";

import * as auth from "../api/auth";
import { ErrorBox, Panel } from "../components/ui";
import { homeFor, useSession } from "../state/session";

/** Sign the server's challenge and exchange it for a session.
 *
 *  Shared with the settings screen, which uses the identical proof to *attach*
 *  a wallet to an existing account. */
export function useWalletProof() {
  const account = useCurrentAccount();
  const { mutateAsync: signPersonalMessage } = useSignPersonalMessage();

  return async () => {
    if (!account) throw new Error("connect a wallet first");
    const message = await auth.fetchChallenge();
    const bytes = new TextEncoder().encode(message);
    const res = await signPersonalMessage({ message: bytes });
    // dapp-kit returns both already base64-encoded, which is what the service
    // expects — re-encoding here would corrupt them.
    return { signature: res.signature, bytes: res.bytes };
  };
}

export default function Login() {
  const { setSession } = useSession();
  const navigate = useNavigate();
  const account = useCurrentAccount();
  const proveWallet = useWalletProof();

  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);

  const finish = (s: auth.Session) => {
    setSession(s);
    navigate(homeFor(s.role), { replace: true });
  };

  const run = async (fn: () => Promise<auth.Session>) => {
    setBusy(true);
    setError(null);
    try {
      finish(await fn());
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="centered">
      <div className="card">
        <h2>Dakota Console</h2>
        <ErrorBox error={error} />

        <Panel title="Sign in">
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void run(() => auth.loginWithPassword(username, password));
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
              <span>Password</span>
              <input
                type="password"
                autoComplete="current-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
            </label>
            <button disabled={busy || !username || !password}>Sign in</button>
          </form>
        </Panel>

        <Panel
          title="Or use a Sui wallet"
          hint="If your account already has a password, a wallet can be added to it from Settings — either one then signs you in."
        >
          <div className="actions" style={{ flexDirection: "column", alignItems: "stretch" }}>
            <ConnectButton />
            <button
              className="secondary"
              disabled={busy || !account}
              onClick={() =>
                void run(async () => {
                  const { signature, bytes } = await proveWallet();
                  return auth.loginWithWallet(signature, bytes);
                })
              }
            >
              Sign in with wallet
            </button>
          </div>
        </Panel>

        <p className="muted" style={{ fontSize: 12 }}>
          No account? You need an invite link. Ask whoever runs this console —
          there is no self-serve signup, and no password reset, because we store
          no email addresses.
        </p>
      </div>
    </div>
  );
}
