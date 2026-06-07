// Admin sign-in state for the token manager.
//
// `signIn` runs the challenge-response: fetch a message, sign it with the
// connected wallet (`signPersonalMessage`), exchange it for a JWT. The token
// is persisted in localStorage and transparently refreshed (same IP, no
// re-signing) when it's close to expiry via `getValidToken`.

import { useCallback, useState } from "react";
import { useSignPersonalMessage } from "@mysten/dapp-kit";

import {
  clearStoredToken,
  fetchChallenge,
  getStoredToken,
  jwtExp,
  jwtSubject,
  login,
  refresh,
  setStoredToken,
} from "./authClient";

// Refresh when fewer than this many seconds remain on the token.
const REFRESH_SKEW_SECS = 300;

export function useAdminAuth() {
  const { mutateAsync: signPersonalMessage } = useSignPersonalMessage();
  const [token, setToken] = useState<string | null>(() => getStoredToken());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const address = token ? jwtSubject(token) : null;
  const isSignedIn = token !== null && jwtExp(token) * 1000 > Date.now();

  const signIn = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const message = await fetchChallenge();
      const bytes = new TextEncoder().encode(message);
      // dapp-kit returns { signature, bytes } — both base64, exactly what
      // auth-service /login expects.
      const signed = await signPersonalMessage({ message: bytes });
      const resp = await login(signed.signature, signed.bytes);
      setStoredToken(resp.token);
      setToken(resp.token);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      throw e;
    } finally {
      setBusy(false);
    }
  }, [signPersonalMessage]);

  const signOut = useCallback(() => {
    clearStoredToken();
    setToken(null);
  }, []);

  /** Return a token good for at least REFRESH_SKEW_SECS, refreshing if needed.
   *  Throws if not signed in or the session has fully elapsed. */
  const getValidToken = useCallback(async (): Promise<string> => {
    const current = token ?? getStoredToken();
    if (!current) throw new Error("not signed in");
    const secsLeft = jwtExp(current) - Math.floor(Date.now() / 1000);
    if (secsLeft > REFRESH_SKEW_SECS) return current;
    // Near/after expiry — try a same-IP refresh; fall back to forcing re-login.
    try {
      const resp = await refresh(current);
      setStoredToken(resp.token);
      setToken(resp.token);
      return resp.token;
    } catch (e) {
      signOut();
      throw e;
    }
  }, [token, signOut]);

  return { token, address, isSignedIn, busy, error, signIn, signOut, getValidToken };
}
