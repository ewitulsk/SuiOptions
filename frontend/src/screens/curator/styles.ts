// Shared inline field style for the curator dashboard controls — the single
// definition behind the create-vault form, the CuratorPanel fields and the
// Bluefin flows. 16px is load-bearing: iOS Safari zooms the viewport on focus
// for anything smaller.

import type { CSSProperties } from "react";

export const curatorFieldStyle: CSSProperties = {
  width: "100%",
  boxSizing: "border-box",
  padding: "5px 6px",
  fontSize: 16,
  borderRadius: 6,
  border: "1px solid var(--aqua-line, rgba(92,107,122,0.25))",
  background: "transparent",
  color: "inherit",
};
