import { useQuery, useQueryClient } from "@tanstack/react-query";
import { NavLink, Navigate, Route, Routes } from "react-router-dom";

import { ApiError, User, get, post } from "./api";
import Deals from "./pages/Deals";
import Feed from "./pages/Feed";
import Login from "./pages/Login";
import Searches from "./pages/Searches";

export default function App() {
  const queryClient = useQueryClient();
  const me = useQuery<User>({
    queryKey: ["me"],
    queryFn: () => get<User>("/auth/me"),
    retry: (count, error) => !(error instanceof ApiError && error.status === 401) && count < 2,
  });

  if (me.isLoading) return <main className="muted">Loading…</main>;

  if (me.isError || !me.data) {
    return <Login onLoggedIn={() => me.refetch()} />;
  }

  const logout = async () => {
    await post("/auth/logout");
    queryClient.clear();
    me.refetch();
  };

  return (
    <>
      <nav>
        <strong>scraper</strong>
        <NavLink to="/">Deal feed</NavLink>
        <NavLink to="/searches">Searches</NavLink>
        <NavLink to="/deals">Deals / P&L</NavLink>
        <span className="spacer" />
        <span className="muted">{me.data.username}</span>
        <button className="secondary" onClick={logout}>
          Log out
        </button>
      </nav>
      <main>
        <Routes>
          <Route path="/" element={<Feed />} />
          <Route path="/searches" element={<Searches />} />
          <Route path="/deals" element={<Deals />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </main>
    </>
  );
}
