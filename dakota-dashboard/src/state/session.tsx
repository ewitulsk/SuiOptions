import { createContext, useContext, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";

import * as auth from "../api/auth";
import type { Role, Session } from "../api/auth";

type Ctx = {
  session: Session | null;
  setSession: (s: Session | null) => void;
  logout: () => void;
};

const SessionContext = createContext<Ctx>({
  session: null,
  setSession: () => {},
  logout: () => {},
});

export function SessionProvider({ children }: { children: ReactNode }) {
  const [session, setRaw] = useState<Session | null>(() => auth.loadSession());

  const setSession = (s: Session | null) => {
    if (s) auth.storeSession(s);
    else auth.clearSession();
    setRaw(s);
  };

  // Slide the token forward well before it expires. The window is bounded
  // server-side by refresh_max_secs, so this extends a session in use without
  // making one immortal.
  useEffect(() => {
    if (!session) return;
    const secondsLeft = auth.jwtExp(session.token) - Date.now() / 1000;
    const delay = Math.max(30, secondsLeft - 300) * 1000;
    const timer = setTimeout(() => {
      auth
        .refresh(session.token)
        .then(setSession)
        // A failed refresh means the window closed or the IP changed; drop to
        // the login screen rather than looping on 401s.
        .catch(() => setSession(null));
    }, delay);
    return () => clearTimeout(timer);
  }, [session]);

  const value = useMemo(
    () => ({ session, setSession, logout: () => setSession(null) }),
    [session],
  );
  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

export const useSession = () => useContext(SessionContext);

/** Session that is known to exist — for use inside authenticated routes. */
export function useAuthed(): Session {
  const { session } = useSession();
  if (!session) throw new Error("useAuthed outside an authenticated route");
  return session;
}

export const homeFor = (role: Role) =>
  role === "admin" ? "/admin" : role === "business" ? "/business" : "/customer";
