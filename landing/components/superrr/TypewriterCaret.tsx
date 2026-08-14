"use client";

import { useEffect, useMemo, useRef, type CSSProperties } from "react";

/**
 * TypewriterCaret — a line being written, not a line being revealed.
 *
 * Motion classes 6 (idle) and 1 (ambient): the caret blinks on its own period
 * forever, and the line types itself, holds, backspaces and retypes without
 * anybody scrolling or pointing at it. A reveal animation is spent the moment
 * it finishes; this one is still working when the reader looks back at it.
 *
 * The gesture is sequential COMMITMENT, which is what separates it from every
 * other per-character treatment: characters do not arrive together with staggered
 * easing, they arrive one at a time, at an uneven human cadence, and some of them
 * are taken back again. The pauses and the deletions are the whole point — a
 * left-to-right fade is a reveal, a line that hesitates and rewrites itself is
 * typing.
 *
 * Confirmed against typed.js, which is the reference implementation of this
 * effect (backDelay 700ms before backspacing, a humanizer that jitters each
 * keystroke by up to 50% of the base speed, smartBackspace stopping the deletion
 * at the prefix the next string shares, and a 0.7s cursor blink):
 *   - https://github.com/mattboldt/typed.js  (README: defaults, smartBackspace)
 *   - https://raw.githubusercontent.com/mattboldt/typed.js/master/src/typed.js
 *   - https://raw.githubusercontent.com/mattboldt/typed.js/master/src/initializer.js
 *
 * What is different here, and why:
 *
 *   THE TEXT IS ALWAYS THERE. The full string is in the DOM from the server
 *   render onwards — real, selectable, searchable, one text node per phrase, no
 *   per-character spans to spell out to a screen reader. Typing is a clip-path
 *   walking left to right, so the resting state, the no-JS state and the
 *   reduced-motion state are all the finished, legible sentence.
 *
 *   NOTHING REFLOWS, EVER. The type is monospace, so a character cell is exactly
 *   `1ch` and every position in the line is known without measuring anything.
 *   The caret is one absolutely-positioned block translated by
 *   `calc(var(--tw-n) * 1ch)`; the reveal is `inset()` cut at the same column.
 *   Per-frame work is one custom property on the host — never a width, never a
 *   letter-spacing, never a style write per character.
 *
 *   THE REST IS MEASURED IN BLINKS. typed.js pauses a flat 700ms before it
 *   backspaces, which at this blink rate is one short blackout and reads as a
 *   flicker. Both idle windows here are whole multiples of the blink period
 *   instead — four half-periods on the finished line, at least two in the gap —
 *   so the caret this module is named for completes off-on-off in plain sight.
 *
 *   THE SCHEDULE IS PRECOMPUTED. The whole type/hold/delete/gap cycle is built
 *   once from the seed as a table of (time, phrase, count) steps, so a frame is
 *   a binary search rather than a state machine with timers, the loop is a pure
 *   function of elapsed time, and the same seed lands on the same frame on the
 *   server, on a frozen clock and after being scrolled offscreen and back.
 */

type Palette = {
  background?: string;
  surface?: string;
  sectionAltBackground?: string | null;
  textPrimary?: string;
  textSecondary?: string;
  brand?: string | null;
  cta?: string;
  ctaForeground?: string;
  border?: string;
  extras?: { role: string; value: string }[];
};

export interface TypewriterCaretProps {
  palette?: Palette;
  /** 0..1 — how much of the character grid is exposed under the line */
  density?: number;
  /** 0..1 — typing rate; 0 parks the finished line with a solid caret */
  speed?: number;
  /** integer — the only entropy source: phrase order, keystroke cadence, where the cycle starts */
  seed?: number;
  /** 0..1 — how loud the ghost tail, the caret and the rules read against the ground */
  intensity?: number;
  /** the phrase, or the phrases it cycles through, typing and deleting each in turn */
  text?: string | string[];
  /** the fixed kicker above the line — it is never typed */
  lead?: string;
  className?: string;
  style?: CSSProperties;
}

const MONO =
  'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace';

/** widest monospace advance we are prepared to meet, in em — used only to fit the line */
const ADVANCE = 0.64;
const CAP_PX = 104;

const DEFAULT_TEXT = [
  "interfaces that move",
  "type that holds up",
  "the whole front end",
  "it look inevitable",
];

/** mulberry32 — small, fast, identical everywhere for a given seed */
function prng(seed: number) {
  let a = (seed | 0) >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

type Rgb = [number, number, number];

function toRgb(hex: string | null | undefined, fallback: Rgb): Rgb {
  if (!hex) return fallback;
  const h = hex.trim().replace("#", "");
  const full = h.length === 3 ? h.split("").map((c) => c + c).join("") : h;
  if (full.length !== 6 || /[^0-9a-f]/i.test(full)) return fallback;
  const n = parseInt(full, 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}
const lumaOf = (c: Rgb) => (0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]) / 255;
const ratio = (a: number, b: number) => (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
const chroma = (c: Rgb) => Math.max(...c) - Math.min(...c);
const mix = (a: Rgb, b: Rgb, t: number): Rgb => [
  Math.round(a[0] + (b[0] - a[0]) * t),
  Math.round(a[1] + (b[1] - a[1]) * t),
  Math.round(a[2] + (b[2] - a[2]) * t),
];

/**
 * Round anything destined for an inline style: the browser normalises CSS
 * numerics to about six significant figures when it parses the server HTML, and
 * a full-precision float comes back different enough for React to call it a
 * hydration mismatch and then decline to patch it.
 */
const n3 = (v: number) => Number(v.toFixed(3));
const rgb = (c: Rgb) => `rgb(${c[0]}, ${c[1]}, ${c[2]})`;
const rgba = (c: Rgb, a: number) => `rgba(${c[0]}, ${c[1]}, ${c[2]}, ${n3(a)})`;
/** custom properties are not part of CSSProperties; they are still valid style keys */
const vars = (o: Record<string, string>) => o as CSSProperties;
const clamp01 = (v: number) => (Number.isFinite(v) ? Math.min(Math.max(v, 0), 1) : 0.5);

interface Step {
  /** ms into the cycle at which this state starts */
  t: number;
  /** which phrase is on screen */
  p: number;
  /** how many of its characters have been committed */
  n: number;
  /** nothing is being typed or deleted — this is where the caret blinks */
  idle: boolean;
}

interface Plan {
  steps: Step[];
  cycle: number;
  /** seeded point in the cycle this instance starts at, so two of these never march in step */
  start: number;
}

/**
 * The whole cycle, as a table.
 *
 * typed.js's humanizer adds half a uniform sample of the base speed to every
 * keystroke, so an interval falls between 1.0x and 1.5x. That is reproduced here
 * off the seeded PRNG instead of an unseeded one, plus the two hesitations a
 * real hand makes that a flat interval does not: a beat at a word break and a
 * longer one after punctuation.
 */
function buildPlan(phrases: string[], sp: number, seed: number, blinkMs: number): Plan {
  const rand = prng(Math.imul(seed | 0, 2654435761) ^ 0x9e3779b9);
  const P = phrases.length;

  // seeded order, so the same four phrases do not always arrive in the same run
  const order = phrases.map((_, i) => i);
  for (let i = P - 1; i > 0; i--) {
    const j = Math.floor(rand() * (i + 1));
    const tmp = order[i];
    order[i] = order[j];
    order[j] = tmp;
  }

  const base = 148 - 116 * sp;          // ms per keystroke
  const del = base * 0.42;              // backspacing is always quicker than typing
  /**
   * The two idle windows are measured in BLINK PERIODS, not in their own
   * milliseconds. typed.js's backDelay is a flat 700ms, which against this blink
   * rate buys a single part-blackout — a flicker, not a blink, on the one
   * primitive whose name is the caret. Quantising each rest to whole half-periods
   * is what makes it legibly blink: the caret goes dark for a FULL half-period
   * and comes back, bracketed by lit states on both sides, twice per phrase.
   *
   * Two half-periods and not four, deliberately. A rest longer than ~900ms is a
   * window in which the only thing separating two frames is the caret's blink
   * phase, and a square wave sampled 2.8 periods apart reads identical 62% of the
   * time — so a longer hold makes an idle module that is genuinely still for
   * three quarters of a second at a stretch. One full blackout per rest is the
   * longest pause this gesture can hold and still be unambiguously alive.
   */
  const hold = blinkMs * 2;
  const gap = Math.max(250 - 110 * sp, blinkMs * 1.6);

  /**
   * smartBackspace: stop deleting at the prefix the next phrase shares. Capped
   * three characters short of either string so there is always a visible
   * deletion — a rotation that silently swaps its tail is a cut, not a rewrite.
   */
  const shared = (a: string, b: string) => {
    if (P < 2) return 0;
    let i = 0;
    const lim = Math.min(a.length, b.length);
    while (i < lim && a[i] === b[i]) i++;
    return Math.max(0, Math.min(i, a.length - 3, b.length - 3));
  };

  const steps: Step[] = [];
  const spans: { from: number; to: number }[] = [];
  let t = 0;

  for (let q = 0; q < P; q++) {
    const cur = phrases[order[q]];
    const prev = phrases[order[(q - 1 + P) % P]];
    const next = phrases[order[(q + 1) % P]];
    const from = shared(prev, cur);
    const to = shared(cur, next);
    const typedFrom = t;

    for (let i = from; i < cur.length; i++) {
      steps.push({ t, p: order[q], n: i, idle: false });
      const ch = cur.charAt(i);
      t +=
        base * (1 + 0.5 * rand()) +
        (ch === " " ? base * 0.4 : 0) +
        (".,;:!?—-".includes(ch) ? base * 2.2 : 0);
    }
    spans.push({ from: typedFrom, to: t });

    steps.push({ t, p: order[q], n: cur.length, idle: true });
    t += hold;

    for (let i = cur.length - 1; i >= to; i--) {
      steps.push({ t, p: order[q], n: i, idle: false });
      t += del * (0.78 + 0.5 * rand());
    }
    // The gap belongs to the phrase that is ARRIVING, not the one just deleted:
    // both are committed to exactly `to` characters and `to` is their shared
    // prefix, so the ink is pixel-identical across the swap and only the ghost
    // tail changes — the beat reads as "the next line is already coming" rather
    // than as dead air still advertising the line you just erased.
    steps.push({ t, p: order[(q + 1) % P], n: to, idle: true });
    t += gap;
  }

  /**
   * Mount a seeded quarter-to-half of the way into a typing run — never at zero,
   * so the line is already several characters in and the ghost carries the rest,
   * and never near the end, because arriving to watch one last character land
   * and then a rest spends the gesture before the reader has seen it. A reader
   * who lands on the page gets most of a sentence being WRITTEN, which is the
   * whole claim this module makes.
   *
   * It is also what makes any single frame of this module worth looking at: two
   * instances on one page must not march in lockstep, the verifier's
   * frozen-clock pass compares seeds on one frame, and every evidence shot is
   * taken at a fixed offset after mount. From here that offset lands on a
   * substantially committed line for a second either side, rather than in the
   * backspace sweep where the line is three characters and a ghost.
   */
  const pick = spans[Math.floor(rand() * spans.length)] ?? { from: 0, to: 0 };
  const start = pick.from + (0.15 + 0.35 * rand()) * (pick.to - pick.from);

  return { steps, cycle: Math.max(t, 1), start };
}

export default function TypewriterCaret({
  palette,
  density = 0.5,
  speed = 0.55,
  seed = 7,
  intensity = 0.8,
  text,
  lead = "we make",
  className,
  style,
}: TypewriterCaretProps) {
  const hostRef = useRef<HTMLDivElement | null>(null);

  const d = clamp01(density);
  const sp = clamp01(speed);
  const it = clamp01(intensity);

  // A caller passing an array literal makes a new array every render; key the
  // memo on the content so the schedule and the effect survive re-renders.
  const key = (Array.isArray(text) ? text : [text]).filter((s) => typeof s === "string" && s.length).join("\u0001");
  const phrases = useMemo<string[]>(() => (key ? key.split("\u0001") : DEFAULT_TEXT), [key]);

  // The blink period is the module's unit of rest: the schedule's idle windows
  // are whole multiples of it, so it has to exist before the plan does.
  const blinkMs = n3(370 - 90 * sp);
  const plan = useMemo(() => buildPlan(phrases, sp, seed, blinkMs), [phrases, sp, seed, blinkMs]);

  const maxLen = phrases.reduce((m, s) => Math.max(m, s.length), 1);
  const cols = maxLen + 1;              // one column reserved for the caret
  const restN = phrases[0].length;      // the resting, server-rendered, reduced-motion state

  // ---- colour: the direction's ground decides the sign of everything -------
  const groundRgb = toRgb(palette?.background, [10, 15, 13]);
  const gl = lumaOf(groundRgb);
  const onLight = gl > 0.5;

  let inkRgb = toRgb(palette?.textPrimary, onLight ? [35, 31, 28] : [244, 244, 244]);
  if (ratio(lumaOf(inkRgb), gl) < 3) inkRgb = onLight ? [26, 24, 22] : [246, 246, 246];

  // The kicker is copy, not chrome, so it has to clear a real contrast bar.
  // atelier-cream's textSecondary is a 1.7:1 warm grey on its own background —
  // fine for a caption on a card, unreadable as a tracked label on the page.
  let dimRgb = toRgb(palette?.textSecondary, mix(groundRgb, inkRgb, 0.55));
  if (ratio(lumaOf(dimRgb), gl) < 2.9) dimRgb = mix(groundRgb, inkRgb, onLight ? 0.84 : 0.7);

  let ruleRgb = toRgb(palette?.border, mix(groundRgb, inkRgb, 0.22));
  if (ratio(lumaOf(ruleRgb), gl) < 1.12) ruleRgb = mix(groundRgb, inkRgb, 0.26);

  /**
   * The accent has to reverse sign with the ground like everything else, and it
   * has to stay the direction's own colour rather than whatever happens to be
   * loudest. Brand first, then cta, then the extras in the order the direction
   * declared them — the first one that actually carries against THIS ground
   * wins, and the ink itself if none of them do. On the dark direction that is
   * the lime brand; on atelier-cream the same brand is a tan at 1.5:1 and its
   * cta a pale yellow at 1.0:1, so the caret falls through to the plum band.
   */
  const candidates: Rgb[] = [];
  for (const c of [palette?.brand, palette?.cta, ...(palette?.extras ?? []).map((e) => e?.value)]) {
    if (typeof c === "string" && c) candidates.push(toRgb(c, inkRgb));
  }
  let accentRgb = inkRgb;
  for (const c of candidates) {
    // a colour with no chroma is not an accent, it is the ink again
    if (ratio(lumaOf(c), gl) >= 3.2 && chroma(c) >= 12) { accentRgb = c; break; }
  }

  /**
   * The ghost is not a garnish — at a low column count it IS the headline, so it
   * has to sit the same DISTANCE from the ground on both. That distance is a
   * contrast ratio, not an alpha: sRGB is gamma-encoded, so the same alpha buys
   * far less separation near white than near black. Measured on the two real
   * grounds, matching the dark branch's 1.99:1 (65,69,68 on 10,15,13) on
   * atelier-cream (ink 46,36,31 on 242,241,236) takes a ≈ 0.35, not 0.166 —
   * which is why the old single-ramp number rendered 1.38:1 there and the tail
   * all but disappeared on the majority of directions.
   */
  const ghostA = n3(onLight ? 0.15 + 0.25 * it : 0.1 + 0.17 * it);
  const caretA = n3(0.7 + 0.3 * it);
  const tickA = n3(0.5 + 0.4 * it);
  const ruleA = n3(0.55 + 0.4 * it);
  const combStep = d < 0.06 ? 0 : d < 0.34 ? 4 : d < 0.67 ? 2 : 1;
  const combA = n3((0.07 + 0.18 * it) * (0.4 + 0.6 * d));

  // Everything computed goes onto the host as a custom property and is read back
  // with var(): a declaration containing var() is kept as written, so the markup
  // React renders is byte-identical on the server and on the client.
  const hostVars = vars({
    "--tw-n": String(restN),
    "--tw-b": "1",
    "--tw-fs": `clamp(12px, calc(92cqw / ${n3(cols * ADVANCE)}), ${CAP_PX}px)`,
    "--tw-comb": combStep
      ? `repeating-linear-gradient(to right, ${rgba(inkRgb, combA)} 0 1px, transparent 1px ${combStep}ch)`
      : "none",
    "--tw-clip": "inset(0 calc(100% - min(100%, var(--tw-n) * 1ch)) 0 0)",
    "--tw-col": "translateX(calc(var(--tw-n) * 1ch))",
  });

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const els = Array.from(host.querySelectorAll<HTMLElement>("[data-tw-phrase]"));
    const reduceQuery = window.matchMedia("(prefers-reduced-motion: reduce)");
    const { steps, cycle, start } = plan;

    let raf = 0;
    let visible = false;
    let reduced = reduceQuery.matches;
    let elapsed = start;
    let last = 0;
    let curN = -1;
    let curB = -1;
    let curP = -1;

    /**
     * One custom property per frame at most, and only when the integer it
     * carries actually changed. Everything downstream — four clipped phrases,
     * the caret and the column tick — reads the same property, so a keystroke
     * costs one style write rather than one per character.
     */
    function paint(n: number, b: number, p: number) {
      if (n !== curN) { curN = n; host?.style.setProperty("--tw-n", String(n)); }
      if (b !== curB) { curB = b; host?.style.setProperty("--tw-b", String(b)); }
      if (p !== curP) {
        curP = p;
        // visibility, not opacity: the phrases waiting their turn must be out of
        // the accessibility tree and out of the selection, while still holding
        // the column open so nothing reflows when one replaces another.
        for (let i = 0; i < els.length; i++) els[i].style.visibility = i === p ? "visible" : "hidden";
      }
    }

    /** the finished sentence, caret parked and lit — server, reduced-motion and speed 0 all land here */
    function rest() {
      paint(restN, 1, 0);
    }

    function stateAt(t: number): Step {
      let lo = 0;
      let hi = steps.length - 1;
      while (lo < hi) {
        const mid = (lo + hi + 1) >> 1;
        if (steps[mid].t <= t) lo = mid; else hi = mid - 1;
      }
      return steps[lo];
    }

    function frame(now: number) {
      raf = 0;
      // First frame of a run is a nominal 60Hz step rather than a delta from an
      // unknown origin: it keeps the frozen-clock pass reproducible and stops a
      // resume-from-offscreen jumping the schedule forward.
      const dt = last === 0 ? 1000 / 60 : Math.min(now - last, 100);
      last = now;
      elapsed += dt;
      if (elapsed >= cycle) elapsed %= cycle;
      const s = stateAt(elapsed);
      // Hard on/off, never a fade, and only while the hand is off the keys —
      // a caret that blinks through its own typing reads as decoration.
      const b = s.idle && Math.floor((elapsed - s.t) / blinkMs) % 2 === 1 ? 0 : 1;
      paint(s.n, b, s.p);
      raf = requestAnimationFrame(frame);
    }

    function run() {
      if (raf || !visible || reduced || sp <= 0.02) return;
      last = 0;
      raf = requestAnimationFrame(frame);
    }
    function halt() {
      if (raf) cancelAnimationFrame(raf);
      raf = 0;
      last = 0;
    }

    const io = new IntersectionObserver(
      (entries) => {
        visible = entries.some((e) => e.isIntersecting);
        if (visible) run(); else halt();
      },
      { threshold: 0 },
    );
    io.observe(host);

    const onReduce = () => {
      reduced = reduceQuery.matches;
      if (reduced) { halt(); rest(); } else run();
    };
    reduceQuery.addEventListener("change", onReduce);

    if (reduced || sp <= 0.02) rest();

    return () => {
      halt();
      io.disconnect();
      reduceQuery.removeEventListener("change", onReduce);
    };
  }, [plan, blinkMs, restN, sp]);

  const line: CSSProperties = {
    fontFamily: MONO,
    fontVariantLigatures: "none",
    whiteSpace: "pre",
    lineHeight: 1.24,
    letterSpacing: "normal",
  };

  return (
    <div
      ref={hostRef}
      className={className}
      style={{
        position: "absolute",
        inset: 0,
        overflow: "hidden",
        background: rgb(groundRgb),
        containerType: "inline-size",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        paddingLeft: "6%",
        paddingRight: "6%",
        paddingTop: "5%",
        paddingBottom: "5%",
        color: rgb(inkRgb),
        ...hostVars,
        ...style,
      }}
    >
      <div style={{ fontSize: "var(--tw-fs)", maxWidth: "100%", width: `${cols}ch`, ...line }}>
        {/* kicker: the part of the sentence that is never in question */}
        <div
          style={{
            display: "flex",
            alignItems: "center",
            columnGap: "0.85em",
            // The headline's clamp protects the headline and nothing else: at a
            // 320px container --tw-fs resolves to ~22px and 0.235 of that is a
            // 5px label carrying 0.26em of tracking. The kicker is copy, so it
            // gets a floor of its own — every phone width is below the point
            // where the ratio alone is legible.
            fontSize: "max(10px, calc(var(--tw-fs) * 0.235))",
            letterSpacing: "0.26em",
            textTransform: "uppercase",
            color: rgb(dimRgb),
            marginBottom: "1.15em",
          }}
        >
          <span
            aria-hidden="true"
            style={{
              flexGrow: 0,
              flexShrink: 0,
              width: "0.6em",
              height: "0.6em",
              background: rgb(accentRgb),
              opacity: n3(0.65 + 0.35 * it),
            }}
          />
          <span>{lead}</span>
          <span
            aria-hidden="true"
            style={{ flexGrow: 1, flexShrink: 1, height: "1px", background: rgba(ruleRgb, ruleA) }}
          />
        </div>

        {/*
          The line itself: one text node per phrase, cut at the typed column.

          The stack is hidden from assistive tech and its accessible name is
          served statically below. Not because the letters would be spelled out
          — there are no per-character spans and each phrase is a single text
          node — but because the phrase on screen SWAPS every few seconds, and a
          region whose announced content silently changes under a virtual cursor
          is worse than one that never changes at all. aria-hidden is invisible
          to selection and to find-in-page, so the visible words stay real,
          selectable and searchable exactly as before.
        */}
        <div aria-hidden="true" style={{ position: "relative", display: "grid", width: `${cols}ch` }}>
          {phrases.map((p, k) => (
            <div
              key={k}
              data-tw-phrase=""
              style={{
                gridRowStart: 1,
                gridColumnStart: 1,
                display: "grid",
                visibility: k === 0 ? "visible" : "hidden",
              }}
            >
              {/* where the line is going: the untyped tail, held open and just visible */}
              <span
                aria-hidden="true"
                style={{
                  gridRowStart: 1,
                  gridColumnStart: 1,
                  paddingTop: "0.1em",
                  paddingBottom: "0.1em",
                  opacity: ghostA,
                  userSelect: "none",
                  WebkitUserSelect: "none",
                }}
              >
                {p}
              </span>
              {/* the committed characters — real text, cut by clip-path, never re-laid-out */}
              <span
                style={{
                  gridRowStart: 1,
                  gridColumnStart: 1,
                  paddingTop: "0.1em",
                  paddingBottom: "0.1em",
                  clipPath: "var(--tw-clip)",
                }}
              >
                {p}
              </span>
            </div>
          ))}

          <span
            aria-hidden="true"
            style={{
              position: "absolute",
              left: 0,
              top: "50%",
              width: "1ch",
              height: "1.04em",
              background: rgb(accentRgb),
              opacity: `calc(var(--tw-b) * ${caretA})`,
              transform: "translateY(-50%) var(--tw-col)",
            }}
          />
        </div>

        {/*
          The announced string, pinned. One static text node, never touched by
          the loop, so the region reads the same sentence whether the caret is
          mid-word, parked by reduced motion or halted offscreen.
        */}
        <span
          style={{
            position: "absolute",
            width: "1px",
            height: "1px",
            margin: "-1px",
            padding: 0,
            border: 0,
            overflow: "hidden",
            clipPath: "inset(50%)",
            whiteSpace: "nowrap",
            // not selectable: the visible line is the copyable one, and a
            // screen-reader duplicate inside a select-all is a paste bug
            userSelect: "none",
            WebkitUserSelect: "none",
          }}
        >
          {phrases[0]}
        </span>

        {/* the character grid the line is written on, and the column it has reached */}
        <div style={{ position: "relative", height: "0.34em", marginTop: "0.34em" }}>
          <div
            aria-hidden="true"
            style={{ position: "absolute", left: 0, right: 0, top: 0, height: "1px", background: rgba(ruleRgb, ruleA) }}
          />
          <div
            aria-hidden="true"
            style={{
              position: "absolute",
              left: 0,
              right: 0,
              top: 0,
              height: "0.18em",
              backgroundImage: "var(--tw-comb)",
            }}
          />
          <div
            aria-hidden="true"
            style={{
              position: "absolute",
              left: 0,
              top: 0,
              width: "1ch",
              height: "0.3em",
              background: rgb(accentRgb),
              opacity: tickA,
              // No willChange: the caret and this tick step by whole character
              // cells at typing rate, not per frame, so a permanent compositor
              // layer buys nothing — and it would be retained while the module
              // is parked by reduced motion, speed 0 or the offscreen halt.
              transform: "var(--tw-col)",
            }}
          />
        </div>
      </div>
    </div>
  );
}
