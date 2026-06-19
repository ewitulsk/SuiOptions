import { useEffect, useLayoutEffect, useRef, useState } from "react";

export type PillGeom = {
  left: number;
  top: number;
  width: number;
  height: number;
  ready: boolean;
};

/**
 * Measured-offset sliding indicator for segmented selectors. Tracks the
 * `.is-active` child inside `ref` and reports its box so a single pill element
 * can animate between options — the same pattern the Buy-mode toggle uses.
 * `top`/`height` are reported too so wrapping selectors can translate in 2D.
 * `animated` is false on first paint so the pill snaps onto the initial option
 * instead of sliding in from the edge.
 */
export function useSegmentPill(activeKey: string | number) {
  const ref = useRef<HTMLDivElement>(null);
  const [geom, setGeom] = useState<PillGeom>({
    left: 0,
    top: 0,
    width: 0,
    height: 0,
    ready: false,
  });
  const [animated, setAnimated] = useState(false);

  useLayoutEffect(() => {
    const sync = () => {
      const active = ref.current?.querySelector<HTMLElement>(".is-active");
      if (!active) {
        setGeom((g) => ({ ...g, ready: false }));
        return;
      }
      setGeom({
        left: active.offsetLeft,
        top: active.offsetTop,
        width: active.offsetWidth,
        height: active.offsetHeight,
        ready: true,
      });
    };
    sync();
    window.addEventListener("resize", sync);
    return () => window.removeEventListener("resize", sync);
  }, [activeKey]);

  useEffect(() => setAnimated(true), []);

  return { ref, geom, animated };
}
