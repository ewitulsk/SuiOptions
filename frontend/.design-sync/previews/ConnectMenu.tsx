import { ConnectMenu } from "tideline-frontend";

// The wallet-connect affordance shown while disconnected. When a session-login
// deployment is configured it's a dropdown trigger that fans out into Sui
// wallet + "Sign in with Phantom / MetaMask / WalletConnect" options; with no
// session deployment (the preview default) it collapses to the real plain
// "Connect wallet" pill. Either way this is the genuine disconnected state —
// the dropdown only mounts on click, so the resting trigger is what shows here.

export const Disconnected = () => <ConnectMenu onSuiWallet={() => {}} />;
