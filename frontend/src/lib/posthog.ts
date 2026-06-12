import posthog from "posthog-js";
import { ENV } from "../config";

const key = import.meta.env.VITE_POSTHOG_KEY as string | undefined;

if (key) {
  posthog.init(key, {
    // Deployed builds set VITE_POSTHOG_HOST=/ph — a same-origin proxy via
    // vercel.json rewrites so ad blockers don't drop events. Local dev hits
    // PostHog directly. ui_host keeps the toolbar pointed at the real UI.
    api_host: import.meta.env.VITE_POSTHOG_HOST,
    ui_host: "https://us.posthog.com",
    capture_exceptions: true,
  });
  // Super property on every event so testnet/staging traffic is filterable.
  posthog.register({ network: ENV });
}

export { posthog };
