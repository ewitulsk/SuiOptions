// Token-info admin panel (lives on /admin).
//
// Lists every supported token from token-info and lets an admin add/remove
// them. Mutations require an admin JWT obtained by signing a challenge with
// the connected wallet (see useAdminAuth); the wallet must be on the
// auth-service allowlist. Reads are open, so the table renders even before
// sign-in.

import { useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";
import { useQueryClient } from "@tanstack/react-query";

import { useAdminAuth } from "../api/useAdminAuth";
import { useSupportedTokens } from "../api/useSupportedTokens";
import { AuthExpiredError, createToken, deleteToken } from "../api/tokenAdmin";

export function TokenManager({ flash }: { flash: (msg: string) => void }) {
  const account = useCurrentAccount();
  const auth = useAdminAuth();
  const tokens = useSupportedTokens();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState<string | null>(null);

  const refetch = () =>
    queryClient.invalidateQueries({ queryKey: ["supported-tokens"] });

  // Run an authenticated mutation, surfacing auth-expiry as a re-sign prompt.
  const runAuthed = async (key: string, fn: (token: string) => Promise<void>, ok: string) => {
    setBusy(key);
    try {
      const token = await auth.getValidToken();
      await fn(token);
      flash(`✓ ${ok}`);
      await refetch();
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

  const onSignIn = async () => {
    try {
      await auth.signIn();
      flash("✓ signed in");
    } catch (e) {
      flash(`sign-in failed · ${e instanceof Error ? e.message : String(e)}`);
    }
  };

  return (
    <section className="admin-section">
      <div className="admin-section__head">
        <h2 className="admin-section__title">Supported tokens</h2>
        <div className="admin-section__sub">
          token-info catalog · add or remove tokens. Mutations require a signed-in
          admin wallet.
        </div>
      </div>

      {/* Sign-in row */}
      <div className="admin-actions-row" style={{ marginBottom: 16 }}>
        {auth.isSignedIn ? (
          <>
            <span className="admin-tag admin-tag--ok">
              signed in{auth.address ? ` · ${auth.address.slice(0, 6)}…${auth.address.slice(-4)}` : ""}
            </span>
            <button className="admin-btn" onClick={auth.signOut}>
              Sign out
            </button>
          </>
        ) : (
          <button
            className="admin-btn admin-btn--primary"
            disabled={!account || auth.busy}
            onClick={onSignIn}
            title={account ? "Sign a challenge to manage tokens" : "Connect a wallet first"}
          >
            {auth.busy ? "signing…" : "Sign in to manage tokens"}
          </button>
        )}
      </div>

      {/* Catalog table */}
      {tokens.isLoading && <div className="admin-empty">loading tokens…</div>}
      {tokens.error && (
        <div className="admin-empty">failed to load tokens · {tokens.error.message}</div>
      )}
      {tokens.data && tokens.data.length === 0 && (
        <div className="admin-empty">no tokens in the catalog yet.</div>
      )}

      {tokens.data && tokens.data.length > 0 && (
        <div className="admin-table">
          <div className="admin-table__head admin-table__row">
            <span>Ticker</span>
            <span>Name</span>
            <span>Coin type</span>
            <span>Decimals</span>
            <span>Status</span>
            <span>Actions</span>
          </div>
          {tokens.data.map((t) => (
            <div className="admin-table__row" key={t.coin_type}>
              <span>{t.ticker}</span>
              <span>{t.name}</span>
              <span>
                <code className="admin-cell__id">
                  {t.coin_type.length > 24
                    ? `${t.coin_type.slice(0, 10)}…${t.coin_type.slice(-10)}`
                    : t.coin_type}
                </code>
              </span>
              <span>{t.decimals}</span>
              <span>
                {t.enabled ? (
                  <span className="admin-tag admin-tag--ok">enabled</span>
                ) : (
                  <span className="admin-tag admin-tag--mute">disabled</span>
                )}
              </span>
              <span className="admin-cell__actions">
                <button
                  className="admin-btn admin-btn--danger"
                  disabled={!auth.isSignedIn || busy !== null}
                  onClick={() =>
                    runAuthed(
                      `del-${t.coin_type}`,
                      (token) => deleteToken(token, t.coin_type),
                      `removed ${t.ticker}`,
                    )
                  }
                >
                  {busy === `del-${t.coin_type}` ? "…" : "Remove"}
                </button>
              </span>
            </div>
          ))}
        </div>
      )}

      {/* Add form */}
      <AddTokenForm
        disabled={!auth.isSignedIn}
        busy={busy === "add"}
        onAdd={(input) =>
          runAuthed("add", (token) => createToken(token, input).then(() => {}), `added ${input.ticker}`)
        }
      />
    </section>
  );
}

function AddTokenForm({
  disabled,
  busy,
  onAdd,
}: {
  disabled: boolean;
  busy: boolean;
  onAdd: (input: {
    coin_type: string;
    ticker: string;
    name: string;
    logo_uri: string | null;
    decimals: number;
    pyth_feed_id: string | null;
    enabled: boolean;
  }) => void;
}) {
  const [coinType, setCoinType] = useState("");
  const [ticker, setTicker] = useState("");
  const [name, setName] = useState("");
  const [logoUri, setLogoUri] = useState("");
  const [decimals, setDecimals] = useState("9");
  const [pythFeedId, setPythFeedId] = useState("");
  const [enabled, setEnabled] = useState(true);

  const ready = coinType && ticker && name && decimals !== "";

  return (
    <div style={{ marginTop: 20 }}>
      <div className="admin-section__sub" style={{ marginBottom: 8 }}>
        Add a token
      </div>
      <div className="admin-grid">
        <Field label="Coin type">
          <input
            className="admin-field__input"
            placeholder="0x…::tbtc::TBTC"
            value={coinType}
            onChange={(e) => setCoinType(e.target.value.trim())}
          />
        </Field>
        <Field label="Ticker">
          <input
            className="admin-field__input"
            placeholder="TBTC"
            value={ticker}
            onChange={(e) => setTicker(e.target.value.trim())}
          />
        </Field>
        <Field label="Name">
          <input
            className="admin-field__input"
            placeholder="Test Bitcoin"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </Field>
        <Field label="Decimals">
          <input
            className="admin-field__input"
            type="number"
            min={0}
            max={38}
            value={decimals}
            onChange={(e) => setDecimals(e.target.value)}
          />
        </Field>
        <Field label="Logo URI (optional)">
          <input
            className="admin-field__input"
            placeholder="https://…"
            value={logoUri}
            onChange={(e) => setLogoUri(e.target.value.trim())}
          />
        </Field>
        <Field label="Pyth feed id (optional)">
          <input
            className="admin-field__input"
            placeholder="0x… (32-byte hex)"
            value={pythFeedId}
            onChange={(e) => setPythFeedId(e.target.value.trim())}
          />
        </Field>
        <Field label="Enabled">
          <input
            type="checkbox"
            checked={enabled}
            onChange={(e) => setEnabled(e.target.checked)}
          />
        </Field>
      </div>
      <button
        className="admin-btn admin-btn--primary"
        disabled={disabled || !ready || busy}
        title={disabled ? "Sign in to add tokens" : undefined}
        onClick={() =>
          onAdd({
            coin_type: coinType,
            ticker,
            name,
            logo_uri: logoUri || null,
            decimals: Number(decimals),
            pyth_feed_id: pythFeedId || null,
            enabled,
          })
        }
      >
        {busy ? "adding…" : "Add token"}
      </button>
    </div>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="admin-field">
      <label className="admin-field__label">{label}</label>
      {children}
    </div>
  );
}
