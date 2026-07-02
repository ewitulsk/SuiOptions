import { Toast } from "tideline-frontend";

// Transient status pill shown after an action. One dot color per variant.
export const Success = () => (
  <Toast message="Wrote 0.05 TBTC — earned 182.40 USDC" variant="success" />
);

export const ErrorToast = () => (
  <Toast message="Transaction reverted — bucket invalidated" variant="error" />
);

export const Info = () => <Toast message="Waiting on market makers…" variant="info" />;
