import { Link, Route, Routes } from "react-router-dom";
import { useWallet } from "@aptos-labs/wallet-adapter-react";
import Landing from "./pages/Landing";
import Collection from "./pages/Collection";
import Item from "./pages/Item";
import WalletPage from "./pages/Wallet";
import Cart from "./pages/Cart";
import Status from "./pages/Status";
import Admin from "./pages/Admin";

function ConnectButton() {
  const { connected, account, connect, disconnect } = useWallet();
  if (connected && account) {
    return (
      <span>
        <code>{account.address.toString().slice(0, 10)}…</code>{" "}
        <button onClick={() => void disconnect()}>Disconnect</button>
      </span>
    );
  }
  return <button onClick={() => void connect("Petra")}>Connect</button>;
}

export default function App() {
  return (
    <div style={{ maxWidth: 960, margin: "0 auto", padding: 16 }}>
      <nav style={{ display: "flex", gap: 12, marginBottom: 16 }}>
        <Link to="/">Market</Link>
        <Link to="/wallet">Wallet</Link>
        <Link to="/cart">Cart</Link>
        <Link to="/status">Status</Link>
        <span style={{ marginLeft: "auto" }}>
          <ConnectButton />
        </span>
      </nav>
      <Routes>
        <Route path="/" element={<Landing />} />
        <Route path="/collections/:id" element={<Collection />} />
        <Route path="/items/:id" element={<Item />} />
        <Route path="/wallet" element={<WalletPage />} />
        <Route path="/cart" element={<Cart />} />
        <Route path="/status" element={<Status />} />
        <Route path="/admin" element={<Admin />} />
      </Routes>
    </div>
  );
}
