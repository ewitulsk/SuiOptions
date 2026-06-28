// Binary selector for the composer's option type: covered CALL vs cash-secured
// PUT. Orthogonal to the writer/trader `View`. Styled as the same segmented
// control with a sliding pill as `BuyModeToggle` (reuses its `buy-toggle` CSS).

import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { OptionType } from "../types";

const OPTIONS: { id: OptionType; label: string }[] = [
  { id: "call", label: "Calls" },
  { id: "put", label: "Puts" },
];

type Props = {
  optionType: OptionType;
  onChange: (t: OptionType) => void;
};

export function OptionTypeToggle({ optionType, onChange }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const [pill, setPill] = useState({ left: 0, width: 0, ready: false });
  const [animated, setAnimated] = useState(false);

  useLayoutEffect(() => {
    const sync = () => {
      const active = ref.current?.querySelector<HTMLButtonElement>("button.is-active");
      if (!active) {
        setPill((p) => ({ ...p, ready: false }));
        return;
      }
      setPill({ left: active.offsetLeft, width: active.offsetWidth, ready: true });
    };
    sync();
    window.addEventListener("resize", sync);
    return () => window.removeEventListener("resize", sync);
  }, [optionType]);

  useEffect(() => setAnimated(true), []);

  return (
    <div className="buy-toggle" role="tablist" ref={ref}>
      <span
        className="buy-toggle__pill"
        aria-hidden
        style={{
          transform: `translateX(${pill.left}px)`,
          width: pill.width,
          opacity: pill.ready ? 1 : 0,
          transition: animated ? undefined : "none",
        }}
      />
      {OPTIONS.map((o) => (
        <button
          key={o.id}
          role="tab"
          aria-selected={optionType === o.id}
          className={"buy-toggle__btn" + (optionType === o.id ? " is-active" : "")}
          onClick={() => onChange(o.id)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}
