// Binary selector for the Buy screen's two purchase paths (SO-225): exchange
// secondary trading (SO-416) vs. minting from the market makers. Styled as a
// segmented control with a blue pill that slides between the two options —
// the same measured-offset animation the header nav uses.

import { useEffect, useLayoutEffect, useRef, useState } from "react";

export type BuyMode = "exchange" | "mm";

const OPTIONS: { id: BuyMode; label: string }[] = [
  { id: "exchange", label: "Trade on Exchange" },
  { id: "mm", label: "Buy from market makers" },
];

type Props = {
  mode: BuyMode;
  onChange: (m: BuyMode) => void;
};

export function BuyModeToggle({ mode, onChange }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const [pill, setPill] = useState({ left: 0, width: 0, ready: false });
  // Suppress the slide on first paint so the pill snaps onto the initial option;
  // transitions turn on after mount for subsequent switches.
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
  }, [mode]);

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
          aria-selected={mode === o.id}
          className={"buy-toggle__btn" + (mode === o.id ? " is-active" : "")}
          onClick={() => onChange(o.id)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}
