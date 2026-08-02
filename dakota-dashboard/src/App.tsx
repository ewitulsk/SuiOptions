import { Navigate, NavLink, Outlet, Route, Routes, useLocation } from "react-router-dom";

import Assets from "./screens/Assets";
import Customers from "./screens/Customers";
import Flows from "./screens/Flows";
import Login from "./screens/Login";
import Ops from "./screens/Ops";
import Ramps from "./screens/Ramps";
import Settings from "./screens/Settings";
import Signup from "./screens/Signup";
import Treasury from "./screens/Treasury";
import { homeFor, useSession } from "./state/session";
import type { Role } from "./api/auth";

/** Which routes exist for a role.
 *
 *  Roles are not URL prefixes to be guessed at — the JWT decides, and
 *  dakota-service enforces the same boundary server-side. This map only
 *  controls what gets rendered and linked. */
const NAV: Record<Role, Array<{ to: string; label: string }>> = {
  admin: [
    { to: "/admin/flows", label: "Flows" },
    { to: "/admin/customers", label: "Customers" },
    { to: "/admin/ramps", label: "Ramps" },
    { to: "/admin/assets", label: "Assets & rates" },
    { to: "/admin/treasury", label: "Treasury" },
    { to: "/admin/ops", label: "Ops" },
    { to: "/settings", label: "Security" },
  ],
  business: [
    { to: "/business/flows", label: "Flows" },
    { to: "/business/customers", label: "My customers" },
    { to: "/business/ramps", label: "Ramps" },
    { to: "/settings", label: "Security" },
  ],
  individual: [
    { to: "/customer/flows", label: "My activity" },
    { to: "/customer/ramps", label: "Ramps" },
    { to: "/settings", label: "Security" },
  ],
};

const TITLE: Record<Role, string> = {
  admin: "Dakota Console",
  business: "Partner Console",
  individual: "Dakota",
};

function Shell() {
  const { session } = useSession();
  const location = useLocation();

  if (!session) {
    return <Navigate to="/login" replace state={{ from: location.pathname }} />;
  }

  return (
    <div className="app">
      <aside className="sidebar">
        <h1>{TITLE[session.role]}</h1>
        <p className="role">{session.role}</p>
        <nav>
          {NAV[session.role].map((item) => (
            <NavLink
              key={item.to}
              to={item.to}
              className={({ isActive }) => (isActive ? "active" : "")}
            >
              {item.label}
            </NavLink>
          ))}
        </nav>
      </aside>
      <main className="main">
        <Outlet />
      </main>
    </div>
  );
}

/** Send an authenticated visitor to their own home rather than a 404. */
function RoleHome() {
  const { session } = useSession();
  return <Navigate to={session ? homeFor(session.role) : "/login"} replace />;
}

export default function App() {
  const { session } = useSession();

  return (
    <Routes>
      <Route
        path="/login"
        element={session ? <Navigate to={homeFor(session.role)} replace /> : <Login />}
      />
      <Route path="/signup" element={<Signup />} />

      <Route element={<Shell />}>
        {/* Admin: the whole platform. */}
        <Route path="/admin" element={<Navigate to="/admin/flows" replace />} />
        <Route path="/admin/flows" element={<Flows title="Platform flows" />} />
        <Route path="/admin/customers" element={<Customers canCreateBusiness />} />
        <Route path="/admin/ramps" element={<Ramps />} />
        <Route path="/admin/assets" element={<Assets />} />
        <Route path="/admin/treasury" element={<Treasury />} />
        <Route path="/admin/ops" element={<Ops />} />

        {/* Partner business: its own roster. Same components — the service
            scopes the data off the token, so nothing here filters. */}
        <Route path="/business" element={<Navigate to="/business/flows" replace />} />
        <Route path="/business/flows" element={<Flows title="Our flows" />} />
        <Route path="/business/customers" element={<Customers canCreateBusiness={false} />} />
        <Route path="/business/ramps" element={<Ramps />} />

        {/* Individual: itself. */}
        <Route path="/customer" element={<Navigate to="/customer/flows" replace />} />
        <Route path="/customer/flows" element={<Flows title="My activity" />} />
        <Route path="/customer/ramps" element={<Ramps />} />

        <Route path="/settings" element={<Settings />} />
      </Route>

      <Route path="*" element={<RoleHome />} />
    </Routes>
  );
}
