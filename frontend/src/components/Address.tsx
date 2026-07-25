// Tap-to-copy address/digest chip (SO-309).
//
// Abbreviated ids used to hide their full value behind `title=`, which never
// fires on touch. Here the short form is the control: tapping it copies the
// FULL value and shows visible feedback. When the clipboard is unavailable
// (insecure context, older mobile Safari) the full value is revealed inline so
// it can still be selected by hand.

import { useEffect, useState } from "react";

import { shortHex } from "../screens/TradingVaults";

type CopyState = "idle" | "copied" | "failed";

/** Copy-to-clipboard with visible feedback that clears itself. */
export function useCopy(): { state: CopyState; copy: (value: string) => void } {
  const [state, setState] = useState<CopyState>("idle");

  useEffect(() => {
    if (state === "idle") return;
    const h = setTimeout(() => setState("idle"), state === "copied" ? 1500 : 6000);
    return () => clearTimeout(h);
  }, [state]);

  const copy = (value: string) => {
    navigator.clipboard
      .writeText(value)
      .then(() => setState("copied"))
      .catch(() => setState("failed"));
  };

  return { state, copy };
}

export function Address({ value, label }: { value: string; label?: string }) {
  const { state, copy } = useCopy();

  return (
    <span className="addr">
      <button
        type="button"
        className="addr__btn mono-break"
        title={label ? `${label}: ${value}` : value}
        aria-label={`Copy ${label ?? "address"} ${value}`}
        onClick={(e) => {
          // Rows carrying this chip are often clickable themselves.
          e.stopPropagation();
          copy(value);
        }}
      >
        {state === "copied" ? "copied ✓" : shortHex(value)}
      </button>
      {state === "failed" && (
        <span className="addr__full mono-break">{value}</span>
      )}
    </span>
  );
}
