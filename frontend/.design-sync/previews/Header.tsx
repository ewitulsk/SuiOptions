import { Header } from "tideline-frontend";

// The top nav bar: tideline wordmark + animated wave backdrop, the route pill
// nav (Earn · Buy · Vaults · Dashboard · Activity · Github), gas-sponsor and
// theme toggles, and the wallet affordance. No wallet is connected in preview,
// so the right side renders its real disconnected "Connect wallet" control
// (a ConnectMenu / plain connect button depending on session-login availability).
// Header is full-width by design — see learnings for the cardMode:"single" ask.

export const Disconnected = () => <Header />;
