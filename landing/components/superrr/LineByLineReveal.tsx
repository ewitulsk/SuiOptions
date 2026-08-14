"use client";

import { useEffect, useMemo, useRef, type CSSProperties } from "react";

/**
 * LineByLineReveal — a block of copy setting itself, one line at a time.
 *
 * Motion class 3 (scroll-linked). Every line sits in its own clipped slot with
 * a hairline at the slot's edge; as the section passes, each line lifts out of
 * its slot in turn and the hairline it came out of goes out behind it. Because
 * the clip is a hard edge rather than a fade, the line reads as rising out of
 * the page instead of materialising on top of it — and because it is bound to
 * the scroll rather than fired once, scrolling back puts the block away again.
 *
 * The standard formulation, confirmed against two real implementations:
 *
 *   - https://gsap.com/docs/v3/Plugins/SplitText/ — `mask: "lines"` wraps every
 *     split line "in an *extra* element with [overflow] clip", and the reveal is
 *     that inner element travelling from one line-height below to zero, one
 *     stagger step per line. The wrapper is the whole technique; without it the
 *     lift is just a slide.
 *   - https://www.bram.us/2024/02/14/scroll-driven-animations-you-want-overflow-clip-not-overflow-hidden/
 *     — `overflow: hidden` "creates a scroll container" even though it only
 *     clips visually, which is why the masked-line pattern should clip rather
 *     than hide. This module uses `clip-path: inset()` for the same reason plus
 *     one more: a NEGATIVE top inset leaves the top edge open, so an ascender or
 *     a diacritic on a landed line can never be shaved by its own mask, while
 *     the bottom edge stays exactly where the hairline is drawn.
 *
 * What is done differently here, and why this is a primitive rather than a demo:
 *
 *   The per-line offsets are AUTHORED, not instantiated. The reference versions
 *   give every line its own tween, its own ScrollTrigger or its own
 *   `view-timeline`. This one publishes a single number on the host — `--p`, the
 *   block's assembly in 0..1, read from the element's own bounding rect — and
 *   each line solves its own window out of it: `clamp(0, (p - s_i)/WIN, 1)`,
 *   then an ease-out cubic, both evaluated by the style engine. One property
 *   write per frame for the whole block, and the clamp SATURATES, so only the
 *   line or two actually crossing the edge repaint.
 *
 *   THE WORDS ARE THE REAL WORDS, IN THE DOCUMENT. Nothing is drawn into a
 *   canvas, nothing is duplicated, every line is selectable and in reading
 *   order. The split is by LINE, not by character, so each fragment is already
 *   whole words in sequence: there is nothing atomised for an `aria-label` to
 *   reassemble, and adding one would mean hiding the only copy of the text
 *   behind a second, duplicated string that can drift from it. The per-character
 *   modules in this family need that pattern; a line split does not, and the
 *   accessible name is never animated either way.
 *
 *   EVERY RESTING STATE IS THE FINISHED BLOCK. `--p` falls back to 1 in every
 *   expression and is written as 1 on the host, so the server HTML, the frame
 *   before hydration, a JS failure, `speed = 0` and `prefers-reduced-motion` all
 *   show the same fully set, fully legible copy with the slot rules gone. Even
 *   the failure mode is safe: if a custom-property expression were ever invalid,
 *   `transform` computes to `none` and the line sits at its landed position.
 *
 *   NOTHING THAT REFLOWS IS TOUCHED. The lift is a transform and the rules are
 *   opacity and scaleX. Tracking, weight, size and margins are authored once —
 *   per-frame `letter-spacing` on a block of display type relayouts the whole
 *   column and shows up as long frames.
 *
 * Progress comes from the element's own getBoundingClientRect, never from
 * window.scrollY: the module has no idea where it sits in the page and must not
 * care. The scroll listener is passive, RAF-coalesced, and REMOVED rather than
 * early-returned when the section leaves the viewport.
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

export interface LineByLineRevealProps {
  /** the direction's colour tokens; every field is read defensively */
  palette?: Palette;
  /** 0..1 — the measure: 26 to 40 characters a line, so it also sets how many lines */
  density?: number;
  /** 0..1 — how much of the scroll the assembly tracks; 0 parks it on the finished block */
  speed?: number;
  /** integer — the only entropy source; fixes the passage and the settle of each line */
  seed?: number;
  /** 0..1 — how present the slot rules and the accent are; the type's ink never moves */
  intensity?: number;
  /**
   * the copy. A string is wrapped to the measure; an array is taken as
   * pre-broken lines (an over-long one is still wrapped so it cannot push the
   * block out of its box). This is REAL text: in the document, selectable, read
   * once by assistive tech, in the order given.
   */
  text?: string | string[];
  /** the small line above; defaults to the passage's own, "" for none */
  eyebrow?: string;
  /** the note below; defaults to the passage's own, "" for none */
  footer?: string;
  /**
   * the element the block itself is: a statement set as a section headline
   * should be a heading, or it never appears in a screen reader's heading list.
   * The rows are this element's children in reading order, so the semantics land
   * on the copy rather than on the module's positioning box. Defaults to "div".
   */
  as?: "h1" | "h2" | "h3" | "p" | "div";
  className?: string;
  style?: CSSProperties;
}

type Style = CSSProperties & { [k: `--${string}`]: string };
type RGB = [number, number, number];

/**
 * Round anything destined for an inline style.
 *
 * The browser normalises CSS numerics to about six significant figures when it
 * parses the server HTML, so a full-precision float renders as
 * `opacity:0.5769188914372069`, reads back as `0.576919`, React reports a
 * hydration mismatch and then declines to patch it — leaving the server's values
 * in the DOM. Every DOM-based primitive has to do this.
 */
const n3 = (v: number) => Number(v.toFixed(3));
const clamp01 = (v: number) => (v > 1 ? 1 : v < 0 ? 0 : v);

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

function toRgb(hex: string | null | undefined, fallback: RGB): RGB {
  if (!hex) return fallback;
  const h = hex.trim().replace("#", "");
  const full = h.length === 3 ? h.split("").map((c) => c + c).join("") : h;
  if (full.length !== 6 || /[^0-9a-f]/i.test(full)) return fallback;
  const n = parseInt(full, 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}
const lumaOf = (c: RGB) => (0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]) / 255;
const mix = (a: RGB, b: RGB, t: number): RGB => [
  a[0] + (b[0] - a[0]) * t,
  a[1] + (b[1] - a[1]) * t,
  a[2] + (b[2] - a[2]) * t,
];
const css = (c: RGB) => `rgb(${Math.round(c[0])}, ${Math.round(c[1])}, ${Math.round(c[2])})`;
const rgba = (c: RGB, a: number) =>
  `rgba(${Math.round(c[0])}, ${Math.round(c[1])}, ${Math.round(c[2])}, ${n3(a)})`;

/**
 * Real copy, because a statement block is a rag and a set of sentence lengths
 * as much as it is words: where a clause lands decides what a line-by-line
 * reveal actually reveals. The seed chooses one; a caller's own copy replaces it
 * outright.
 */
const PASSAGES: { eyebrow: string; text: string; footer: string }[] = [
  {
    eyebrow: "Plate one",
    text:
      "A page is assembled, not poured. Every line takes its place, holds the measure, " +
      "and waits for the one below it to arrive.",
    footer: "Composing room, 6:40 a.m.",
  },
  {
    eyebrow: "The brief",
    text:
      "Say the true thing first. Put the proof underneath it. Leave the rest of the page " +
      "for the reader to fill in.",
    footer: "Pinned above the desk",
  },
  {
    eyebrow: "On arrival",
    text:
      "Nothing should appear before it is needed, and nothing that has arrived should ask " +
      "to be looked at twice.",
    footer: "House rules, no. 4",
  },
  {
    eyebrow: "Measure",
    text:
      "Give a sentence the room it needs and no more. The white around it is doing half " +
      "the work and charging nothing for it.",
    footer: "Notes on setting",
  },
  {
    eyebrow: "The last pass",
    text:
      "Read it once for sense, once for sound, and once for the line that could be struck " +
      "out entirely. Then strike it out.",
    footer: "Before it goes to press",
  },
];

/** how much of the block's assembly one line's own lift occupies */
const WIN = 0.42;
/** ...and where the last line has finished, leaving a settled beat at the end */
const LAST = 0.93;

const SANS =
  'ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, Helvetica, Arial, "Helvetica Neue", sans-serif';
const MONO = 'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace';
const SERIF = 'Georgia, "Iowan Old Style", "Times New Roman", Times, serif';

/**
 * Greedy wrap to a character measure. Deterministic and metric-free on purpose:
 * it runs identically in Node and in the browser, which is what keeps the server
 * HTML and the first client render byte-identical. The type is sized in the
 * host's own container units against the same measure, so the count holds at
 * every width.
 */
function wrapWords(text: string, maxChars: number): string[] {
  const words = text.split(/\s+/).filter(Boolean);
  const lines: string[] = [];
  let cur = "";
  for (const w of words) {
    if (!cur) cur = w;
    else if (cur.length + 1 + w.length <= maxChars) cur = `${cur} ${w}`;
    else {
      lines.push(cur);
      cur = w;
    }
  }
  if (cur) lines.push(cur);
  // a last line holding one short word reads as a mistake in the setting, and
  // in a line-by-line reveal it is the line everything else waits for
  if (lines.length > 1) {
    const last = lines[lines.length - 1];
    const prev = lines[lines.length - 2];
    const cut = prev.lastIndexOf(" ");
    if (last.length < Math.round(maxChars * 0.32) && cut > 0) {
      lines[lines.length - 2] = prev.slice(0, cut);
      lines[lines.length - 1] = `${prev.slice(cut + 1)} ${last}`;
    }
  }
  return lines;
}

interface Row {
  text: string;
  /** where this line's own lift opens, in the block's 0..1 assembly */
  start: number;
  /** the whole lift, as a percentage of the line's own height — always past its slot */
  travel: number;
  /** a fraction of a degree of set, unwound as it lands */
  rot: number;
  size: string;
  lineHeight: number;
  weight: number;
  family: string;
  color: string;
  tracking: string;
  upper: boolean;
  italic: boolean;
  /** em of the block size, so the whole stack scales as one thing */
  padB: number;
  mt: number;
  mb: number;
  /** the slot rule under a display line runs the measure; under a note it is shorter */
  ruleWidth: string;
}

interface Scene {
  rows: Row[];
  size: string;
  ground: string;
  rule: string;
  accent: string;
  tick: string;
}

export default function LineByLineReveal({
  palette,
  density = 0.5,
  speed = 1,
  seed = 7,
  intensity = 0.85,
  text,
  eyebrow,
  footer,
  as = "div",
  className,
  style,
}: LineByLineRevealProps) {
  const Block = as;
  const hostRef = useRef<HTMLDivElement | null>(null);

  const groundHex = palette?.background ?? "#0A0F0D";
  // deliberately NOT defaulted here: the ink's fallback depends on the ground,
  // which is only known inside the memo. A literal default at this point parses
  // cleanly and so pre-empts the per-ground fallback below — leaving near-white
  // type on cream for any palette that ships a background without a textPrimary.
  const inkHex: string | null = palette?.textPrimary ?? null;
  const brandHex = palette?.brand ?? palette?.cta ?? "#D8FD91";
  const ctaHex = palette?.cta ?? brandHex;
  const extrasKey = (palette?.extras ?? []).map((e) => e.value).join("|");
  // the SHAPE of `text` is part of the key: "one two", ["one two"] and
  // ["one", "two"] must not collapse to the same string, or switching between
  // them at runtime returns a stale scene from the memo below and silently
  // ignores the caller's own line breaks.
  const copyKey = JSON.stringify(text ?? null);

  // The whole setting is a pure function of the seed and the props, so the
  // server and the client render byte-identical markup — no effect, no
  // measurement, no suppressHydrationWarning.
  const scene = useMemo<Scene>(() => {
    const rand = prng(((seed | 0) * 2654435761) >>> 0);
    const d = clamp01(density);
    const it = clamp01(intensity);
    const maxChars = Math.round(26 + 14 * d);

    const custom = Array.isArray(text) ? text.length > 0 : typeof text === "string" && text.trim().length > 0;
    const pick = PASSAGES[Math.floor(rand() * PASSAGES.length) % PASSAGES.length];

    const body: string[] = [];
    if (Array.isArray(text)) {
      // pre-broken lines are the caller's own breaks; only an over-long one is
      // re-wrapped, and only so it cannot push the block out of its box
      for (const raw of text) {
        const t = String(raw ?? "").trim();
        if (!t) continue;
        if (t.length <= maxChars) body.push(t);
        else body.push(...wrapWords(t, maxChars));
      }
    } else {
      body.push(...wrapWords(custom ? String(text) : pick.text, maxChars));
    }
    // an empty or whitespace-only `text` falls all the way back to the passage —
    // wrapped, never sliced, so the block never opens on a word cut in half
    if (!body.length) body.push(...wrapWords(pick.text, maxChars));

    const bg = toRgb(groundHex, [10, 15, 13]);
    // 13 of the 17 directions ship a LIGHT ground. Nothing here is authored as
    // "light on dark"; it is ink on paper, and which way that runs is read off
    // the ground.
    const onLight = lumaOf(bg) > 0.5;
    const ink = toRgb(inkHex, onLight ? [46, 36, 31] : [244, 244, 244]);
    // the note under the block: quieter than the statement, still comfortably
    // past 4.5:1 on both grounds
    const quiet = mix(bg, ink, onLight ? 0.8 : 0.74);

    // The accent is filtered by the ground before it is chosen, so a pale yellow
    // CTA never becomes an invisible mark on cream and a near-black extra never
    // disappears into a dark page; then it is carried toward the ink on a light
    // ground, which keeps the direction's hue while giving small type and a
    // hairline enough weight to be read.
    let accent: RGB | null = null;
    for (const hex of [brandHex, ctaHex, ...(palette?.extras ?? []).map((e) => e.value)]) {
      const c = toRgb(hex, bg);
      const l = lumaOf(c);
      if (onLight ? l > 0.66 : l < 0.34) continue;
      accent = c;
      break;
    }
    if (!accent) accent = mix(bg, ink, onLight ? 0.82 : 0.9);
    // Two strengths of the same accent, because one of them is TYPE. The marks
    // can sit at the direction's own value; the eyebrow is 17px of tracked
    // mono, and a warm tan straight off a cream palette lands at 3.9:1 there —
    // so the text version is carried further toward the ink until it clears
    // 4.5:1, keeping the hue and gaining the weight small type needs.
    const mark = onLight ? mix(accent, ink, 0.3) : accent;
    const markText = onLight ? mix(accent, ink, 0.52) : accent;

    const eb = eyebrow ?? (custom ? "" : pick.eyebrow);
    const ft = footer ?? (custom ? "" : pick.footer);

    const rows: Row[] = [];
    // the stagger, the lift and the settle are resolved in one pass below, once
    // the whole stack is known
    const push = (r: Omit<Row, "start" | "travel" | "rot">) =>
      rows.push({ ...r, start: 0, travel: 0, rot: 0 });

    if (eb) {
      push({
        text: eb,
        size: "0.245em",
        lineHeight: 1.5,
        weight: 600,
        family: MONO,
        color: css(markText),
        tracking: "0.2em",
        upper: true,
        italic: false,
        padB: 0.1,
        mt: 0,
        mb: 0.62,
        ruleWidth: "38%",
      });
    }
    for (let i = 0; i < body.length; i++) {
      push({
        text: body[i],
        size: "1em",
        lineHeight: 1.08,
        weight: 600,
        family: SANS,
        color: css(ink),
        tracking: "-0.021em",
        upper: false,
        italic: false,
        padB: 0.11,
        mt: 0,
        mb: i === body.length - 1 ? 0 : 0.12,
        ruleWidth: "100%",
      });
    }
    if (ft) {
      push({
        text: ft,
        size: "0.32em",
        lineHeight: 1.5,
        weight: 400,
        family: SERIF,
        color: css(quiet),
        tracking: "0.004em",
        upper: false,
        italic: true,
        padB: 0.1,
        mt: 0.72,
        mb: 0,
        ruleWidth: "52%",
      });
    }

    // The stagger: tight enough that the block assembles inside one reading
    // window rather than taking the whole section, with a settled beat at the
    // end. The seed only jitters it — reading order is never shuffled, because
    // a line arriving out of order is a different (and worse) effect.
    const step = rows.length > 1 ? (LAST - WIN) / (rows.length - 1) : 0;
    for (let i = 0; i < rows.length; i++) {
      const r = rows[i];
      const em = Number(r.size.replace("em", ""));
      const inner = em * r.lineHeight;
      // the lift has to clear the slot, and the slot is the line plus its own
      // bottom padding — expressed as a percentage of the LINE, since that is
      // what a transform percentage resolves against
      const clear = ((inner + r.padB) / inner) * 100;
      r.start = n3(Math.max(0, i * step + (rand() - 0.5) * 0.02));
      r.travel = n3(clear + 4 + 9 * it + rand() * 5);
      r.rot = n3((rand() - 0.5) * 1.1 * (0.4 + 0.6 * it));
    }

    // The block is the smaller of two fits against its own box: as wide as the
    // longest line may run, and never deeper than 84% of the host. Both are
    // measured against the HOST's container units rather than the viewport,
    // because the module has to compose the same way in a half-width section as
    // it does full bleed.
    //
    // The width term is estimated from the longest line's CHARACTER COUNT and a
    // measured advance for the system sans at this weight (0.435em is what a
    // lowercase-heavy English line actually averages — the 0.5em rule of thumb
    // sets the block a fifth too small and leaves it floating in its box).
    const units = rows.reduce((sum, r) => {
      const em = Number(r.size.replace("em", ""));
      return sum + em * r.lineHeight + r.padB + r.mt + r.mb;
    }, 0);
    const longest = body.reduce((m, l) => Math.max(m, l.length), 1);
    const wFit = 76 / (longest * 0.435);
    const hFit = 84 / Math.max(1, units);

    return {
      rows,
      // the floor is the one thing that can defeat the width fit, so it is set
      // low enough that no section a page can hand this module reaches it: at
      // 11px the default measure fits a host about 160px wide, and the wrapper's
      // max-width catches anything narrower still.
      size: `clamp(11px, min(${n3(wFit)}cqw, ${n3(hFit)}cqh), 92px)`,
      ground: css(bg),
      // The slot edge: quiet while a line is still in it, gone the moment the
      // line has landed. Lighter on paper than on a dark ground — a dark hairline
      // sitting under a line that has just landed reads for a beat as a RULE
      // under the text rather than the edge it came out of, and on cream that
      // misreading is much easier to make than it is in reverse.
      rule: rgba(
        mix(bg, ink, onLight ? 0.26 : 0.38),
        onLight ? 0.34 + 0.28 * it : 0.55 + 0.45 * it,
      ),
      accent: rgba(mark, 0.55 + 0.45 * it),
      tick: css(mark),
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [density, intensity, seed, groundHex, inkHex, brandHex, ctaHex, extrasKey, copyKey, eyebrow, footer]);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const sp = clamp01(speed);
    const reduceQuery = window.matchMedia("(prefers-reduced-motion: reduce)");
    let reduced = reduceQuery.matches;
    let raf = 0;
    let bound = false;

    // ONE write per frame, on the host. Every line's window, its ease, its lift
    // and every rule's opacity is a static expression in the markup that reads
    // this number, so the browser never re-serialises thirty style attributes
    // and the markup React hydrates is byte-identical to what it rendered.
    function apply(p: number) {
      host?.style.setProperty("--p", p.toFixed(4));
    }

    function measure() {
      raf = 0;
      if (!host) return;
      const r = host.getBoundingClientRect();
      const vh = window.innerHeight || 900;
      // The block starts setting itself once its head is up past two thirds of
      // the viewport and is fully set by the time that head reaches the top —
      // which is to say, it assembles across exactly the stretch of scroll where
      // the whole block is on screen, rather than spending its first third
      // below the fold. Denominator floored so a very short block still gets a
      // travel rather than an instant flip.
      const denom = Math.max(200, r.height * 0.62 + vh * 0.25);
      const raw = clamp01((vh * 0.72 - r.top) / denom);
      // Rest is the FINISHED block, so speed = 0 is exactly the reduced-motion
      // still and the server frame is never a half-set page.
      apply(reduced || sp === 0 ? 1 : 1 + (raw - 1) * sp);
    }

    function onScroll() {
      if (!raf) raf = requestAnimationFrame(measure);
    }

    function bind() {
      if (bound) return;
      bound = true;
      // capture, because a scroll inside a nested scroller never bubbles to
      // window — and removed with the same flag, or the pair does not match
      window.addEventListener("scroll", onScroll, { passive: true, capture: true });
      window.addEventListener("resize", onScroll, { passive: true });
      onScroll();
    }
    function unbind() {
      if (!bound) return;
      bound = false;
      window.removeEventListener("scroll", onScroll, { capture: true });
      window.removeEventListener("resize", onScroll);
      if (raf) {
        cancelAnimationFrame(raf);
        raf = 0;
      }
    }

    const io = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) bind();
        else unbind();
      },
      { threshold: 0 },
    );
    io.observe(host);

    const onReduce = () => {
      reduced = reduceQuery.matches;
      measure();
    };
    reduceQuery.addEventListener("change", onReduce);

    measure();

    return () => {
      unbind();
      io.disconnect();
      reduceQuery.removeEventListener("change", onReduce);
    };
  }, [scene, speed]);

  const hostStyle: Style = {
    position: "absolute",
    inset: 0,
    overflow: "hidden",
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    background: scene.ground,
    // The query container is the module's own box, and the type is sized off it
    // on the CHILD — an element cannot query the container it establishes, so a
    // cqw font-size written here would silently measure the viewport instead and
    // set a narrow section at full-page size.
    containerType: "size",
    // the server frame, and every frame before hydration, is the finished block
    "--p": "1",
    ...style,
  };

  return (
    <div ref={hostRef} className={className} style={hostStyle}>
      <Block
        style={{
          width: "max-content",
          // the guard, not the common case: the size floor below can outrun the
          // width fit in a very narrow host, and a centred max-content box would
          // then be clipped at BOTH ends. Allowed to shrink to its box instead,
          // so every line still starts at the same left edge and the rag is all
          // that can be lost.
          maxWidth: "100%",
          minWidth: 0,
          margin: 0,
          fontFamily: SANS,
          fontSize: scene.size,
          // the whole stack settles the last few pixels as the last line lands —
          // one transform for the block, on top of the per-line lift
          transform: "translate3d(0, calc((1 - var(--p, 1)) * 11px), 0)",
        }}
      >
        {scene.rows.map((row, i) => {
          // presence as a per-line window on ONE published number, then an
          // ease-out cubic. It saturates: every line but the one crossing its
          // slot edge computes the same value frame after frame and never
          // repaints.
          const slot: Style = {
            position: "relative",
            display: "block",
            paddingBottom: `${row.padB}em`,
            marginTop: row.mt ? `${row.mt}em` : undefined,
            marginBottom: row.mb ? `${row.mb}em` : undefined,
            // clip, not hide: `overflow: hidden` would make every line a scroll
            // container. The negative top inset leaves the top edge open, so a
            // landed ascender is never shaved, while the bottom edge stays
            // exactly on the hairline the line rises out of.
            clipPath: "inset(-32% 0px 0px 0px)",
            "--l": `clamp(0, calc((var(--p, 1) - ${row.start}) / ${WIN}), 1)`,
            "--e": "calc(1 - (1 - var(--l)) * (1 - var(--l)) * (1 - var(--l)))",
          };
          return (
            <span key={i} style={slot}>
              {/* the slot edge — the thing the line comes out of. Present while
                  the line is still on its way, gone once it has landed, so the
                  finished block is type and nothing else. */}
              <span
                style={{
                  position: "absolute",
                  left: 0,
                  bottom: 0,
                  width: row.ruleWidth,
                  height: "1px",
                  background: scene.rule,
                  opacity: "calc(1 - var(--e, 1))",
                }}
              />
              {/* the guide mark at the head of an empty slot, so a block that
                  has not started reads as a page waiting to be set rather than
                  as a page that failed to load */}
              <span
                style={{
                  position: "absolute",
                  left: 0,
                  bottom: 0,
                  width: "0.17em",
                  height: "max(2px, 0.032em)",
                  background: scene.tick,
                  opacity: "calc(1 - var(--e, 1))",
                }}
              />
              {/* ...and the accent riding that edge while the line crosses it:
                  it grows with the lift and is spent the moment the line is
                  set. One scaleX, one opacity. */}
              <span
                style={{
                  position: "absolute",
                  left: 0,
                  bottom: 0,
                  width: row.ruleWidth,
                  height: "max(2px, 0.032em)",
                  background: scene.accent,
                  transformOrigin: "0% 50%",
                  // no willChange here: a 2px bar's scaleX/opacity pair is
                  // already compositor-friendly, the clamp saturates so it stops
                  // changing the moment its line has landed, and one permanently
                  // promoted layer per row is enough — two is a layer the block
                  // never gives back.
                  transform: "scaleX(var(--e, 1))",
                  opacity: "clamp(0, calc(4.4 * var(--e, 1) * (1 - var(--e, 1))), 1)",
                }}
              />
              {/* the line itself: real text, in the document, in reading order,
                  selectable, and legible from the instant it clears the edge */}
              <span
                style={{
                  display: "block",
                  whiteSpace: "nowrap",
                  fontFamily: row.family,
                  fontSize: row.size,
                  lineHeight: row.lineHeight,
                  fontWeight: row.weight,
                  fontStyle: row.italic ? "italic" : "normal",
                  letterSpacing: row.tracking,
                  textTransform: row.upper ? "uppercase" : "none",
                  color: row.color,
                  transformOrigin: "50% 100%",
                  willChange: "transform",
                  transform:
                    `translate3d(0, calc((1 - var(--e, 1)) * ${row.travel}%), 0) ` +
                    `rotate(calc((1 - var(--e, 1)) * ${row.rot}deg))`,
                }}
              >
                {/* the trailing space is deliberate: it makes the separation
                    between one line and the next EXPLICIT in the text content
                    rather than inferred from `display: block`, so no accname or
                    serialisation path can run "once for" into "sound, and". With
                    `white-space: nowrap` on a left-aligned line it is trimmed at
                    the ragged end and occupies nothing. */}
                {`${row.text} `}
              </span>
            </span>
          );
        })}
      </Block>
    </div>
  );
}
