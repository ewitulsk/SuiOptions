// Reusable ceremony UX (SO-305): a hook that drives any async ceremony
// (DKG or signing) with progress/error state, plus a small inline status
// component. Curator ops are never gas-sponsored and ceremonies run FROM
// the parent address — nothing here touches the gas station.

import { useCallback, useState } from "react";

import type { CeremonyProgress } from "../../frost/ceremony";

export type CeremonyState =
  | { status: "idle" }
  | { status: "running"; phase: string }
  | { status: "error"; message: string }
  | { status: "done"; message: string };

export function useCeremony() {
  const [state, setState] = useState<CeremonyState>({ status: "idle" });

  const run = useCallback(
    async <T,>(
      fn: (onProgress: CeremonyProgress) => Promise<T>,
      doneMessage: string,
    ): Promise<T | null> => {
      setState({ status: "running", phase: "Starting…" });
      try {
        const result = await fn((phase) => setState({ status: "running", phase }));
        setState({ status: "done", message: doneMessage });
        return result;
      } catch (err) {
        setState({
          status: "error",
          message: err instanceof Error ? err.message : String(err),
        });
        return null;
      }
    },
    [],
  );

  const reset = useCallback(() => setState({ status: "idle" }), []);
  return { state, run, reset, busy: state.status === "running" };
}

export function CeremonyStatus({ state }: { state: CeremonyState }) {
  if (state.status === "idle") return null;
  const tone =
    state.status === "error" ? "is-danger" : state.status === "done" ? "is-success" : "is-info";
  const text =
    state.status === "running"
      ? state.phase
      : state.status === "error"
        ? state.message
        : state.message;
  return (
    <div
      className={`status-pill ${tone}`}
      role="status"
      style={{ display: "block", marginTop: 8, padding: "6px 10px", fontSize: 12, lineHeight: 1.5 }}
    >
      {state.status === "running" && "⏳ "}
      {state.status === "error" && "⚠ "}
      {state.status === "done" && "✓ "}
      {text}
    </div>
  );
}
