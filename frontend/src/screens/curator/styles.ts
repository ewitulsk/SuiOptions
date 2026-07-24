// Shared inline field style for the curator dashboard controls — matches
// the create-vault form and the existing CuratorPanel fields.

import type { CSSProperties } from "react";

export const curatorFieldStyle: CSSProperties = {
  width: "100%",
  padding: 6,
  borderRadius: 6,
  border: "1px solid var(--aqua-line, rgba(92,107,122,0.25))",
  background: "transparent",
  color: "inherit",
};
