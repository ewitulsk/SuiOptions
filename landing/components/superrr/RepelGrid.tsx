"use client";

import { useEffect, useRef, type CSSProperties } from "react";

/**
 * RepelGrid — a ruled sheet of marks that swells away from the reader's cursor.
 *
 * Motion class 4 (pointer-reactive). At rest it is a still, evenly ruled field
 * of small marks: a printed grid, not a swarm. Where the cursor goes the sheet
 * opens — the marks spread apart, grow and take the direction's accent — and a
 * ring of slightly tightened, quieter marks closes around the outside of the
 * swell. Move on and the whole neighbourhood springs back with a little
 * overshoot, the way a stretched sheet settles rather than snapping.
 *
 * ── the field ──────────────────────────────────────────────────────────────
 * The textbook repel is `normalize(rest − pointer) * strength * falloff(d)`,
 * which has a discontinuity at the cursor (the direction flips across it) and
 * so tears a hard-edged hole in a regular grid. The Gaussian bulge is the same
 * gesture written so that the singularity cancels:
 *
 *     e   = rest − pointer,  u = |e|² / σ²
 *     q   = (A/σ)·√e · exp(−u/2)
 *     disp = e · q
 *
 * because |e|·(1/|e|) falls straight out of it — no normalize, no sqrt, no
 * epsilon, and displacement → 0 smoothly at the cursor itself. |disp| peaks at
 * exactly A, at |e| = σ. Near the cursor the map is a pure local magnification
 * of 1 + q₀; at |e| = √3·σ the radial derivative dips below 1 and the grid
 * tightens into a ring. That ring is what makes it read as ONE deforming sheet
 * rather than as dots individually running away.
 *
 * Two clamps keep the grid legible, both of them required rather than tasteful:
 * A ≤ 2/3 of the cell pitch, so no mark travels near a neighbour's cell; and
 * A/σ ≤ 1/√e·(1/0.736) ≈ 0.8, below which dr′/dr stays positive everywhere and
 * rows mathematically cannot fold through each other. Defaults sit at A/σ ≈ 0.3:
 * the core opens to 1.50× its resting spacing and the tightest ring closes to
 * 0.63×, which is a sheet you can see deforming rather than dots getting bigger.
 *
 * The same q also gives the local area dilation analytically —
 * J = (1 + q(1−u))(1 + q) — which is what drives each mark's size and tone.
 * Size and colour are the honest geometry of the deformation, not a second
 * effect layered on top: marks are bigger where the sheet is stretched thin and
 * smaller where it is compressed.
 *
 * ── what this shares, and what it does not ─────────────────────────────────
 * None of the above is this module's own: `pointer/CursorFieldWarp` runs the
 * same Gaussian bulge, sign-flipped — it pulls a sampling coordinate inward
 * where this pushes marks outward, J = (1−q)(1−q(1−u)) against this one's
 * (1+q(1−u))(1+q) — and drives its tone from the same analytic dilation. The
 * algebra is shared and should be read as shared; what differs is the gesture.
 * There, one eased pointer bows a continuous WebGL line screen across the whole
 * frame, and the read is line COVERAGE on hairlines. Here the sheet is ~600
 * countable discrete marks over a field that stays perfectly still except
 * locally, and every mark carries its own spring in one of three seeded
 * stiffness tiers: a fast sweep leaves a wake of marks still coming home behind
 * the cursor, and the sheet overshoots and settles rather than arriving. A
 * size-and-tone ladder on discrete objects is a different picture from coverage
 * on a continuous ruling, and the two do not read as the same module.
 *
 * ── the return ─────────────────────────────────────────────────────────────
 * Every mark carries its own damped spring (ζ = 0.66, slightly under-damped so
 * the sheet settles instead of arriving) stepped by Ryan Juckett's closed-form
 * solution rather than by an integrator. That form is exact for any dt, so the
 * settle takes the same wall-clock time at 60Hz and 144Hz, and it cannot go
 * unstable on a long frame the way semi-implicit Euler can.
 *   - https://www.ryanjuckett.com/damped-springs/
 *
 * Springs are per-mark rather than one eased pointer, because the two are not
 * the same picture: a mark the cursor swept past two hundred milliseconds ago is
 * still coming home while the one under the cursor now is being pushed out, so a
 * fast sweep leaves a wake in the sheet. Stiffness comes in three seeded tiers,
 * so the sheet does not relax as one rigid plate.
 *
 * The reference this was checked against, for grid construction, influence
 * radius and the mouse-out reset, is a plain dot-grid repeller:
 *   - https://github.com/sathishk-dev/interactive-dot-grid/blob/main/src/index.ts
 *     (spacing 22, radiusEffect 220, dotMin 3 → dotMax 12 — a 4× size range,
 *     which is why the size response here is loud rather than polite. Its
 *     per-frame `smoothing` lerp is the one thing deliberately NOT copied: a
 *     raw lerp settles twice as fast on a 120Hz display as on a 60Hz one.)
 *
 * ── contract notes worth copying ───────────────────────────────────────────
 * The RAF loop is DEMAND-DRIVEN. It starts on a pointer event and stops itself
 * the moment every spring is inside 0.08px of its target — including when the
 * cursor parks over the sheet, in which case the deformation stays and the loop
 * still stops. An idling pointer loop is a permanent tax on every other module
 * sharing the frame.
 *
 * The half of that which is easy to get wrong is the restart. A stopped loop
 * over a still-bulged sheet has to be woken by everything that can invalidate
 * it and not only by a pointer move: the cursor leaving the window, a finger
 * lifting, the page scrolling the sheet out from under a parked cursor, the
 * element re-entering the viewport mid-flight. `restFieldLive` is the one bit
 * of state that separates "far cursor, flat sheet, nothing to do" from "far
 * cursor, sheet still deformed, spring it back" — without it a far pointer is
 * indistinguishable from a settled one and the sheet keeps a frozen dent.
 *
 * Under `prefers-reduced-motion` the springs are bypassed, not the feature: the
 * sheet snaps to the cursor and never animates on its own. Direct manipulation
 * is the reader's own movement.
 *
 * With no pointer at all — a phone — the rest state is the whole composed
 * picture, and a touch drives it through the same pointer events. A finger is
 * not a cursor, though: it has no leave and no blur, so the lift is what
 * releases it, or the sheet keeps a dent under the last tap forever.
 *
 * Both grounds from the first frame. luma(background) picks the direction of
 * the ladder: lighter-than-ground marks that warm toward the accent on a dark
 * page, ink on paper that darkens and saturates toward a warm accent ink on a
 * light one — never an additive glow that vanishes on cream.
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

export interface RepelGridProps {
  /** the direction's colour tokens; every field is read defensively */
  palette?: Palette;
  /** 0..1 — how fine the grid is; low is a coarse rule, high is close-ruled */
  density?: number;
  /** 0..1 — how quickly the sheet answers and settles. 0 is slow and heavy. */
  speed?: number;
  /** integer — the only entropy source; sets the pitch, the grid's phase, the mark sizes and the spring tiers */
  seed?: number;
  /** 0..1 — how far the sheet opens, and how loud the swell reads */
  intensity?: number;
  /** 0..1 — how wide the cursor's influence is, as a fraction of the short edge */
  reach?: number;
  className?: string;
  style?: CSSProperties;
}

type RGB = [number, number, number];

/** #rgb / #rrggbb → 0..255 triple; anything unparseable falls back */
function parseHex(hex: string | null | undefined, fallback: RGB): RGB {
  if (!hex) return fallback;
  const h = hex.trim().replace("#", "");
  const full = h.length === 3 ? h.split("").map((c) => c + c).join("") : h;
  if (full.length !== 6 || /[^0-9a-f]/i.test(full)) return fallback;
  const n = parseInt(full, 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}
const mix = (a: RGB, b: RGB, t: number): RGB => [
  a[0] + (b[0] - a[0]) * t,
  a[1] + (b[1] - a[1]) * t,
  a[2] + (b[2] - a[2]) * t,
];
const luma = (c: RGB) => (0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]) / 255;
/**
 * Push a colour away from its own luma. Every mark is laid down as a solid mix
 * with the ground rather than at an alpha, and a mix toward cream keeps the
 * ground's chroma as well as its value — so a warm accent arrives grey unless
 * the pigment is over-saturated on the way in.
 */
const saturate = (c: RGB, k: number): RGB => {
  const l = luma(c) * 255;
  return [
    Math.min(255, Math.max(0, l + (c[0] - l) * k)),
    Math.min(255, Math.max(0, l + (c[1] - l) * k)),
    Math.min(255, Math.max(0, l + (c[2] - l) * k)),
  ];
};
/** integers only — the string has to be byte-identical on the server and the client */
const css = (c: RGB) => `rgb(${Math.round(c[0])}, ${Math.round(c[1])}, ${Math.round(c[2])})`;
const cssA = (c: RGB, a: number) => `rgba(${Math.round(c[0])}, ${Math.round(c[1])}, ${Math.round(c[2])}, ${a})`;
const clamp01 = (v: number) => (Number.isFinite(v) ? Math.min(Math.max(v, 0), 1) : 0.5);

/** mulberry32 — seed is the only entropy source in this module */
function mulberry32(a: number) {
  let s = a >>> 0;
  return function next(): number {
    s = (s + 0x6d2b79f5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const SQRT_E = 1.6487212707001282;
/**
 * Backing store scale. The marks are small filled rects, so this trades
 * crispness against fill area directly; 0.66 keeps a 6-device-pixel mark
 * resolving as a mark after the CSS upscale and holds the software rasterizer
 * comfortably inside its floor.
 */
const RENDER_SCALE = 0.66;
/** one cell of overscan on every side, so the sheet runs off the frame */
const OVERSCAN = 1;
/** tone bands — colour varies across the swell without a state change per mark */
const BANDS = 6;
/**
 * Band edges on the normalised swell. 0 is exactly the rest value, so the bulk
 * of the sheet sits in band 1 and the compression ring gets a band of its own
 * below it: the ring is the evidence that this is a sheet and not a scatter.
 */
const BAND_EDGE = [-0.04, 0.055, 0.2, 0.42, 0.68];
/** three seeded stiffness tiers, so the sheet does not relax as one rigid plate */
const TIERS = 3;
const TIER_MUL = [0.86, 1.0, 1.18];
/** slightly under-damped: the sheet settles rather than arriving */
const ZETA = 0.66;
/** the damped angular frequency's share of ω — two constants, so fold them once */
const ZETA_ALPHA = Math.sqrt(1 - ZETA * ZETA);
/** a mark is at rest once it is inside this many device px of its target */
const SETTLE_PX = 0.08;
const SETTLE_VEL = 2.5;
/** beyond this many σ² the Gaussian is under 2e-5 — skip the mark entirely */
const CUTOFF_U = 22;
/** gradient wash stops; a linear alpha ramp leaves a visible contour line */
const STOPS = 7;

export default function RepelGrid({
  palette,
  density = 0.55,
  speed = 0.5,
  seed = 4,
  intensity = 0.85,
  reach = 0.55,
  className,
  style,
}: RepelGridProps) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);

  const it = clamp01(intensity);
  const ground = parseHex(palette?.background, [10, 15, 13]);
  const onLight = luma(ground) > 0.5;
  const ink = parseHex(palette?.textPrimary, onLight ? [46, 36, 31] : [244, 244, 244]);
  const quiet = parseHex(palette?.textSecondary, onLight ? [138, 134, 126] : [154, 166, 162]);
  const accent = parseHex(palette?.brand ?? palette?.cta, onLight ? [199, 154, 126] : [216, 253, 145]);

  // The tonal ladder, decided once here so the loop never touches globalAlpha
  // and never mixes a colour. Direction follows the ground: marks get lighter
  // than the ground in air, darker than it on paper. The accent is carried in
  // both directions — on cream it is pulled toward ink and over-saturated on
  // the way in, or it arrives as a grey-brown smudge.
  // On paper the resting pigment carries a fifth of the direction's accent.
  // Ink mixed straight into cream comes back a neutral grey and takes the
  // warmth of the ground with it; a warm ground wants a warm rule.
  const rest = onLight ? mix(mix(quiet, ink, 0.55), accent, 0.22) : mix(quiet, ink, 0.45);
  const hotPigment = onLight ? saturate(mix(accent, ink, 0.42), 1.55) : mix(accent, ink, 0.1);
  const midPigment = onLight ? saturate(mix(accent, ink, 0.6), 1.35) : mix(accent, ink, 0.24);
  // Band 0 is the compression ring, and it took two passes to get the SIGN of
  // it right. Quieted below the resting tone — the obvious reading of "less
  // important than the swell" — it stopped looking like a ring of tightened
  // marks and started looking like a hole erased around the swell, which turns
  // the module into a spotlight over a grid, the one thing it must not be. The
  // sheet is the answer: where it bunches there is MORE of it per unit area, so
  // the ring carries ~18% more ink than the resting field and shrinks a tenth,
  // and the eye reads what it is actually looking at — closer spacing.
  const inkRest = onLight ? 0.53 + 0.17 * it : 0.42 + 0.14 * it;
  const bandCols: RGB[] = onLight
    ? [
        mix(ground, rest, inkRest * 1.18),
        mix(ground, rest, inkRest),
        mix(ground, mix(rest, midPigment, 0.45), 0.62 + 0.16 * it),
        mix(ground, mix(rest, midPigment, 0.78), 0.74 + 0.16 * it),
        mix(ground, midPigment, 0.86 + 0.12 * it),
        mix(ground, hotPigment, 0.94 + 0.06 * it),
      ]
    : [
        mix(ground, rest, inkRest * 1.18),
        mix(ground, rest, inkRest),
        mix(ground, mix(rest, midPigment, 0.45), 0.56 + 0.16 * it),
        mix(ground, mix(rest, midPigment, 0.78), 0.7 + 0.17 * it),
        mix(ground, midPigment, 0.84 + 0.13 * it),
        mix(ground, hotPigment, 0.93 + 0.07 * it),
      ];
  const bandCss = bandCols.map(css).join("|");

  // Grid pitch in CSS px, an integer so the resting rule lands cleanly. It sets
  // the scale of everything: the swell's reach is measured in cells, and its
  // peak displacement is capped at two thirds of a cell, so a finer grid buys
  // more marks at the cost of a smaller, weaker lens. 48px at the default is
  // the coarsest reading that still fills a hero with countable rows.
  //
  // The seed opens the pitch by up to 11% and never closes it, so two seeds are
  // two sheets — a different rule count across the frame — rather than one sheet
  // translated by the cell phase. Coarser only: density owns how fine the rule
  // gets, and the argument above for 48 as the floor still has to hold.
  const pitchJit = (Math.imul((seed | 0) ^ 0x2545f491, 0x9e3779b1) >>> 0) % 1024;
  const pitch = Math.max(26, Math.round((62 - 26 * clamp01(density)) * (1 + 0.13 * (pitchJit / 1024))));

  // The ground wash, drawn twice on purpose: once as CSS so the section carries
  // the direction's colour from the server render and before the first frame,
  // and once into an offscreen canvas the loop blits, because the canvas is
  // opaque. An opaque canvas is the single largest performance decision here —
  // a full-bleed transparent layer has to be alpha-composited against the page
  // every frame, which on a software rasterizer costs more than everything
  // drawn on top of it.
  // The wash is deliberately shallow. A 40% fall to black across the dark
  // ground left a corner the grid could not be seen in at all, and the cursor
  // walks through every corner — a backdrop with a dead quarter is a backdrop
  // that fails wherever the reader happens to be.
  const lift = mix(ground, accent, onLight ? 0.1 : 0.075);
  const deep = onLight ? mix(ground, ink, 0.075) : mix(ground, [0, 0, 0], 0.2);
  const rampStops = (c: RGB, from: number, to: number) => {
    const parts: string[] = [];
    for (let k = 0; k < STOPS; k++) {
      const t = k / (STOPS - 1);
      const a = 1 - t * t * (3 - 2 * t);
      parts.push(`${cssA(c, Number(a.toFixed(3)))} ${Math.round(from + (to - from) * t)}%`);
    }
    return parts.join(", ");
  };
  const cGround = css(ground);
  const background =
    `radial-gradient(116% 82% at 22% 2%, ${rampStops(lift, 0, 78)}),` +
    `radial-gradient(108% 80% at 88% 104%, ${rampStops(deep, 0, 76)}),` +
    `${cGround}`;
  const liftKey = css(lift);
  const deepKey = css(deep);

  useEffect(() => {
    const host = hostRef.current;
    const canvas = canvasRef.current;
    if (!host || !canvas) return;
    // opaque: no per-frame alpha composite of a full-bleed layer
    const g = canvas.getContext("2d", { alpha: false });
    // no 2d context: the CSS wash underneath is the whole fallback, so just stop
    if (!g) return;

    const cols6 = bandCss.split("|");
    const sp = clamp01(speed);
    const iv = clamp01(intensity);
    const rc = clamp01(reach);

    // ---- seeded arrangement ----------------------------------------------
    const s32 = (Math.imul((seed | 0) ^ 0x9e3779b9, 2654435761) >>> 0) ^ 0x85ebca6b;
    const rnd = mulberry32(s32);
    // where the rule falls inside a cell is as much the arrangement as how it bends
    const phaseX = rnd();
    const phaseY = rnd();
    // how uneven the printing is, per seed: a tight sheet reads as machine-ruled,
    // a loose one as pressed by hand. Half the seed's identity is here and the
    // other half is in the pitch.
    const jitSpan = 0.1 + 0.09 * rnd();

    // ---- geometry, reallocated only when the grid size changes ------------
    let cols = 0;
    let rows = 0;
    let n = 0;
    let rx = new Float32Array(0); // rest position, device px
    let ry = new Float32Array(0);
    let ox = new Float32Array(0); // displacement from rest
    let oy = new Float32Array(0);
    let vx = new Float32Array(0); // ...and its velocity, device px/s
    let vy = new Float32Array(0);
    let sw = new Float32Array(0); // normalised swell, and its velocity
    let sv = new Float32Array(0);
    let jit = new Float32Array(0); // seeded per-mark size
    let tier = new Uint8Array(0);
    let half = new Float32Array(0); // draw scratch: half-extent this frame
    let band = new Uint8Array(0);

    let w = 0;
    let h = 0;
    let scale = 0;
    let pitchDev = 0;
    let originX = 0;
    let originY = 0;
    let invS2 = 1;
    let kq = 0;
    let invWMax = 1;
    let baseHalf = 1;
    let sizeGain = 1;
    let sigma = 1;

    // ---- the ground, pre-rendered once per size ---------------------------
    let bg: HTMLCanvasElement | null = null;
    function buildGround() {
      const c = bg ?? document.createElement("canvas");
      bg = c;
      c.width = w;
      c.height = h;
      const bc = c.getContext("2d", { alpha: false });
      if (!bc) return;
      bc.fillStyle = cGround;
      bc.fillRect(0, 0, w, h);
      const r = Math.max(w, h);
      const paint = (col: string, cx: number, cy: number, rad: number) => {
        const grad = bc.createRadialGradient(cx, cy, 0, cx, cy, rad);
        for (let k = 0; k < STOPS; k++) {
          const t = k / (STOPS - 1);
          const a = 1 - t * t * (3 - 2 * t);
          grad.addColorStop(t, `rgba(${col}, ${a.toFixed(3)})`);
        }
        bc.fillStyle = grad;
        bc.fillRect(0, 0, w, h);
      };
      const rgbOf = (s: string) => s.slice(4, -1);
      paint(rgbOf(liftKey), w * 0.22, h * 0.02, r * 0.9);
      paint(rgbOf(deepKey), w * 0.88, h * 1.04, r * 0.88);
    }

    // ---- coronas for the two hottest bands --------------------------------
    // MDN is explicit about both halves of this: cache repeated drawing to an
    // offscreen canvas, and never scale in drawImage. Only the top two bands
    // get one — a couple of dozen marks at most — because a blit apiece for the
    // whole sheet costs more than every batched fill put together.
    let corona: (HTMLCanvasElement | null)[] = [null, null];
    const coronaHalf = [0, 0];
    function buildCoronas() {
      corona = [null, null];
      for (let i = 0; i < 2; i++) {
        const r = baseHalf * (i === 0 ? 3.0 : 4.1);
        const size = Math.ceil(r) * 2 + 2;
        if (size < 4) continue;
        const c = document.createElement("canvas");
        c.width = size;
        c.height = size;
        const sc = c.getContext("2d");
        if (!sc) continue;
        const m = size / 2;
        const rgbTxt = cols6[i === 0 ? 4 : 5].slice(4, -1);
        const grad = sc.createRadialGradient(m, m, 0, m, m, r);
        const peak = (onLight ? 0.2 : 0.3) * (0.55 + 0.45 * iv);
        for (let k = 0; k <= 6; k++) {
          const u = k / 6;
          grad.addColorStop(u, `rgba(${rgbTxt}, ${(Math.pow(Math.max(0, 1 - u * u), 2.2) * peak).toFixed(4)})`);
        }
        sc.fillStyle = grad;
        sc.fillRect(0, 0, size, size);
        corona[i] = c;
        coronaHalf[i] = size >> 1;
      }
    }

    function rebuild() {
      const prevPitch = pitchDev;
      const prevCols = cols;
      pitchDev = pitch * scale;
      cols = Math.ceil(w / pitchDev) + 1 + OVERSCAN * 2;
      rows = Math.ceil(h / pitchDev) + 1 + OVERSCAN * 2;
      const nn = cols * rows;
      // Dragging a window between monitors changes devicePixelRatio, which
      // rebuilds the whole backing store at a new scale while the lattice keeps
      // exactly the same shape. Zeroing the springs there would pop the sheet
      // flat mid-flight, so the live displacement is carried across and rescaled
      // by the pitch ratio instead — it is stored in device px, and so is its
      // velocity. Only a genuinely different lattice starts from rest.
      if (nn !== n) {
        n = nn;
        rx = new Float32Array(n);
        ry = new Float32Array(n);
        ox = new Float32Array(n);
        oy = new Float32Array(n);
        vx = new Float32Array(n);
        vy = new Float32Array(n);
        sw = new Float32Array(n);
        sv = new Float32Array(n);
        jit = new Float32Array(n);
        tier = new Uint8Array(n);
        half = new Float32Array(n);
        band = new Uint8Array(n);
      } else if (cols !== prevCols) {
        // same count, different shape — every index now means a different cell
        ox.fill(0);
        oy.fill(0);
        vx.fill(0);
        vy.fill(0);
        sw.fill(0);
        sv.fill(0);
      } else if (prevPitch > 0 && pitchDev !== prevPitch) {
        const ds = pitchDev / prevPitch;
        for (let k = 0; k < n; k++) {
          ox[k] *= ds;
          oy[k] *= ds;
          vx[k] *= ds;
          vy[k] *= ds;
        }
      }
      originX = (phaseX - OVERSCAN) * pitchDev;
      originY = (phaseY - OVERSCAN) * pitchDev;
      // the seeded arrangement is rebuilt from a fresh stream so the same seed
      // gives the same sheet whatever order the resizes happened in
      const r2 = mulberry32((s32 ^ 0x27d4eb2d) >>> 0);
      for (let j = 0; j < rows; j++) {
        const y = originY + j * pitchDev;
        for (let i = 0; i < cols; i++) {
          const k = j * cols + i;
          rx[k] = originX + i * pitchDev;
          ry[k] = y;
          jit[k] = 1 - jitSpan + 2 * jitSpan * r2();
          tier[k] = (r2() * TIERS) | 0;
        }
      }

      // ---- the field's two constants -----------------------------------
      // σ is the reach: the swell peaks at σ from the cursor and the tightened
      // ring sits at √3σ. It is measured in CELLS, not in pixels, and that is
      // the one number that had to be argued for rather than picked. Peak
      // displacement is capped at a fraction of the pitch, so a lens spread
      // over more cells is necessarily a gentler one — at 2.2 cells the core
      // opens to 1.50x its resting spacing and the ring closes to 0.63x, a
      // sheet unmistakably deforming; at a sixth of the frame (~6 cells) the
      // same clamp leaves a 1.1x swell nobody can see, and all that is left is
      // dots getting bigger, which is a different and much cheaper effect.
      const shortDev = Math.min(w, h);
      sigma = Math.min(
        Math.max(pitchDev * (1.5 + 1.2 * rc), shortDev * 0.055),
        shortDev * 0.26,
      );
      // A is the peak displacement. Two clamps, both structural: never more
      // than 2/3 of a cell (a mark must not arrive in a neighbour's cell), and
      // never more than 0.34σ (dr'/dr = 1 − 0.736·A/σ·√e stays well positive,
      // so rows cannot fold through each other).
      const A = Math.min(0.66 * pitchDev, 0.34 * sigma) * (0.42 + 0.58 * iv);
      invS2 = 1 / (sigma * sigma);
      kq = (A / sigma) * SQRT_E;
      // dilation at the cursor itself, used to normalise the swell so the tone
      // bands keep their share of the sheet at any intensity
      invWMax = 1 / Math.max(1e-4, (1 + kq) * (1 + kq) - 1);
      baseHalf = Math.max(0.9, pitchDev * 0.095);
      sizeGain = 0.5 + 0.62 * iv;

      buildCoronas();
      buildGround();
    }

    let dprSeen = 0;
    function resize(): boolean {
      const rect = host!.getBoundingClientRect();
      const dpr = window.devicePixelRatio || 1;
      dprSeen = dpr;
      const s = Math.min(dpr, 2) * RENDER_SCALE;
      const nw = Math.max(1, Math.round(rect.width * s));
      const nh = Math.max(1, Math.round(rect.height * s));
      if (nw === w && nh === h && s === scale) return false;
      w = nw;
      h = nh;
      scale = s;
      canvas!.width = w;
      canvas!.height = h;
      rebuild();
      return true;
    }
    resize();

    // ---- pointer, in backing-store pixels ---------------------------------
    // parked far outside the sheet: the rest state is a still, composed grid,
    // which is also the whole picture on a device that has no cursor
    const AWAY = -1e5;
    let ptrX = AWAY;
    let ptrY = AWAY;
    let clientX = AWAY;
    let clientY = AWAY;
    let hasPointer = false;

    /** resolve the last client position against a freshly measured box */
    function syncPointer() {
      if (!hasPointer) {
        ptrX = AWAY;
        ptrY = AWAY;
        return;
      }
      const r = host!.getBoundingClientRect();
      ptrX = (clientX - r.left) * scale;
      ptrY = (clientY - r.top) * scale;
    }

    /**
     * Is the cursor close enough to be deforming the sheet at all? The field is
     * under 2e-5 past the cutoff, and 5σ is outside it with room to spare. Read
     * against the backing store rather than a fresh rect, because every caller
     * has just run syncPointer() and one forced layout per event is the budget.
     */
    function fieldLive(): boolean {
      if (!hasPointer) return false;
      const lim = 5 * sigma;
      return (
        Math.max(0, Math.max(-ptrX, ptrX - w)) <= lim &&
        Math.max(0, Math.max(-ptrY, ptrY - h)) <= lim
      );
    }

    // ---- the springs ------------------------------------------------------
    // Juckett's closed form, one coefficient set per stiffness tier per frame.
    // Exact for any dt, so the settle takes the same wall-clock time at 60Hz and
    // 144Hz, and a 50ms hitch cannot blow it up the way an integrator would.
    const omega = 2 * Math.PI * (1.5 + 2.8 * sp);
    const pp = new Float64Array(TIERS);
    const pv = new Float64Array(TIERS);
    const vp = new Float64Array(TIERS);
    const vv = new Float64Array(TIERS);
    function coefs(dt: number) {
      for (let t = 0; t < TIERS; t++) {
        const om = omega * TIER_MUL[t];
        const oz = om * ZETA;
        const al = om * ZETA_ALPHA;
        const ex = Math.exp(-oz * dt);
        const ia = 1 / al;
        const es = ex * Math.sin(al * dt);
        const ec = ex * Math.cos(al * dt);
        const eos = oz * es * ia;
        pp[t] = ec + eos;
        pv[t] = es * ia;
        vp[t] = -es * al - oz * eos;
        vv[t] = ec - eos;
      }
    }

    /** advance every mark one frame; returns true once the whole sheet is at rest */
    function step(dt: number): boolean {
      coefs(dt);
      const wpx = baseHalf * sizeGain;
      let maxErr = 0;
      let maxVel = 0;
      for (let k = 0; k < n; k++) {
        const ex = rx[k] - ptrX;
        const ey = ry[k] - ptrY;
        const u = (ex * ex + ey * ey) * invS2;
        let tx = 0;
        let ty = 0;
        let tw = 0;
        if (u < CUTOFF_U) {
          const q = kq * Math.exp(-0.5 * u);
          tx = ex * q;
          ty = ey * q;
          // local area dilation of the same map — positive under the cursor
          // where the sheet is stretched, negative in the ring where it tightens
          tw = ((1 + q * (1 - u)) * (1 + q) - 1) * invWMax;
        }
        const t = tier[k];
        const a = pp[t];
        const b = pv[t];
        const c = vp[t];
        const d = vv[t];

        let o = ox[k] - tx;
        let v = vx[k];
        ox[k] = o * a + v * b + tx;
        vx[k] = o * c + v * d;

        o = oy[k] - ty;
        v = vy[k];
        oy[k] = o * a + v * b + ty;
        vy[k] = o * c + v * d;

        o = sw[k] - tw;
        v = sv[k];
        sw[k] = o * a + v * b + tw;
        sv[k] = o * c + v * d;

        const e =
          Math.abs(ox[k] - tx) + Math.abs(oy[k] - ty) + Math.abs(sw[k] - tw) * wpx;
        if (e > maxErr) maxErr = e;
        const s = Math.abs(vx[k]) + Math.abs(vy[k]) + Math.abs(sv[k]) * wpx;
        if (s > maxVel) maxVel = s;
      }
      return maxErr < SETTLE_PX && maxVel < SETTLE_VEL;
    }

    /** put every mark exactly on its target with no velocity */
    function snap() {
      for (let k = 0; k < n; k++) {
        const ex = rx[k] - ptrX;
        const ey = ry[k] - ptrY;
        const u = (ex * ex + ey * ey) * invS2;
        if (u < CUTOFF_U) {
          const q = kq * Math.exp(-0.5 * u);
          ox[k] = ex * q;
          oy[k] = ey * q;
          sw[k] = ((1 + q * (1 - u)) * (1 + q) - 1) * invWMax;
        } else {
          ox[k] = 0;
          oy[k] = 0;
          sw[k] = 0;
        }
        vx[k] = 0;
        vy[k] = 0;
        sv[k] = 0;
      }
    }

    // ---- drawing ----------------------------------------------------------
    function draw() {
      if (bg) g!.drawImage(bg, 0, 0);
      else {
        g!.fillStyle = cGround;
        g!.fillRect(0, 0, w, h);
      }
      if (!n) return;

      const hi = baseHalf * 2.9;
      const lo = baseHalf * 0.45;
      for (let k = 0; k < n; k++) {
        const s = sw[k];
        const hh = baseHalf * (1 + sizeGain * s) * jit[k];
        half[k] = hh < lo ? lo : hh > hi ? hi : hh;
        let b = 0;
        while (b < BANDS - 1 && s >= BAND_EDGE[b]) b++;
        band[k] = b;
      }

      // coronas first, so every mark sits on top of its own bloom
      for (let i = 0; i < 2; i++) {
        const spr = corona[i];
        if (!spr) continue;
        const want = i === 0 ? 4 : 5;
        const ch = coronaHalf[i];
        for (let k = 0; k < n; k++) {
          if (band[k] !== want) continue;
          g!.drawImage(spr, Math.round(rx[k] + ox[k]) - ch, Math.round(ry[k] + oy[k]) - ch);
        }
      }

      // one path and one fill per band — six draw calls for the whole sheet
      for (let b = 0; b < BANDS; b++) {
        g!.fillStyle = cols6[b];
        g!.beginPath();
        let any = false;
        for (let k = 0; k < n; k++) {
          if (band[k] !== b) continue;
          const s = half[k];
          g!.rect(rx[k] + ox[k] - s, ry[k] + oy[k] - s, s * 2, s * 2);
          any = true;
        }
        if (any) g!.fill();
      }
    }

    // ---- demand-driven loop ------------------------------------------------
    let raf = 0;
    let last = 0;
    let visible = false;
    let reduced = false;
    let atRest = true;
    // whether the sheet was holding a deformation when it went to rest — O(1)
    // bookkeeping that lets kick() tell "nothing to do" from "still bulged"
    // without scanning 600 marks on every event
    let restFieldLive = false;

    function frame(now: number) {
      raf = 0;
      if (!visible) return;
      const dt = last === 0 ? 1 / 60 : Math.min((now - last) / 1000, 0.05);
      last = now;
      if ((window.devicePixelRatio || 1) !== dprSeen) resize();
      // one rect read per FRAME, never per pointer event: resolving the cursor
      // inside the event handler forces a layout flush on every move
      syncPointer();
      // stop when the sheet is at rest — including with the cursor parked on
      // it, where the deformation stays and the loop still ends. An idling
      // pointer loop is a permanent tax on every other module on the page.
      const settled = step(dt);
      if (settled) snap();
      draw();
      if (settled) {
        atRest = true;
        restFieldLive = fieldLive();
        last = 0;
        return;
      }
      atRest = false;
      raf = requestAnimationFrame(frame);
    }

    function kick() {
      if (!visible) return;
      if (reduced) {
        // keep the feature, drop the easing: the sheet follows the reader's own
        // movement, it just never animates on its own
        syncPointer();
        snap();
        draw();
        atRest = true;
        restFieldLive = fieldLive();
        return;
      }
      if (raf) return;
      if (atRest) {
        syncPointer();
        // A far cursor over a FLAT sheet has nothing to do, and proving it costs
        // a frame. But far is not the same as flat: the sheet stops with the
        // deformation still in it when the cursor parks, so the very event that
        // takes the cursor away — pointerleave, blur, a lifted finger — is the
        // one that has to spring it back. Skipping on distance alone freezes a
        // dent in the sheet with no cursor anywhere near it.
        if (!restFieldLive) {
          const lim = 5 * sigma;
          const cx = Math.max(0, Math.max(-ptrX, ptrX - w));
          const cy = Math.max(0, Math.max(-ptrY, ptrY - h));
          if (cx > lim || cy > lim) return;
        }
      }
      last = 0;
      raf = requestAnimationFrame(frame);
    }

    function onMove(e: PointerEvent) {
      clientX = e.clientX;
      clientY = e.clientY;
      hasPointer = true;
      kick();
    }
    function release() {
      if (!hasPointer) return;
      hasPointer = false;
      kick();
    }
    /**
     * A finger is not a cursor: it has no leave and no blur, so without this the
     * sheet keeps a dent under the last tap for the life of the page. A mouse
     * button going up is not a release — the cursor is still there.
     */
    function onLift(e: PointerEvent) {
      if (e.pointerType !== "mouse") release();
    }

    const reduceQuery = window.matchMedia("(prefers-reduced-motion: reduce)");
    reduced = reduceQuery.matches;
    const onReduce = () => {
      reduced = reduceQuery.matches;
      if (reduced && raf) {
        cancelAnimationFrame(raf);
        raf = 0;
        syncPointer();
        snap();
        draw();
        atRest = true;
        restFieldLive = fieldLive();
      }
    };
    reduceQuery.addEventListener("change", onReduce);

    const io = new IntersectionObserver(
      (entries) => {
        const was = visible;
        visible = entries.some((en) => en.isIntersecting);
        if (!visible) {
          if (raf) {
            cancelAnimationFrame(raf);
            raf = 0;
          }
        } else if (!was) {
          // Coming back has to restart what leaving stopped. A sheet scrolled
          // away mid-flight would otherwise return holding the half-deformed
          // frame it was cancelled on, and a keyboard or scrollbar scroll sends
          // no pointer event afterwards to re-kick it. kick() decides: resume
          // the flight, spring back a bulge the cursor has since left, or do
          // nothing at all.
          kick();
        }
      },
      { threshold: 0 },
    );
    io.observe(host);

    const ro = new ResizeObserver(() => {
      if (resize()) {
        syncPointer();
        snap();
        draw();
        atRest = true;
        restFieldLive = fieldLive();
      }
    });
    ro.observe(host);

    const root = document.documentElement;
    window.addEventListener("pointermove", onMove, { passive: true });
    window.addEventListener("pointerdown", onMove, { passive: true });
    window.addEventListener("pointerup", onLift, { passive: true });
    window.addEventListener("pointercancel", onLift, { passive: true });
    window.addEventListener("blur", release, { passive: true });
    // scrolling slides the sheet out from under a parked cursor, and the loop is
    // stopped by then — nothing else would notice the pointer has moved relative
    // to the grid. Cheap: kick() returns immediately unless the sheet is bulged.
    window.addEventListener("scroll", kick, { passive: true });
    root.addEventListener("pointerleave", release, { passive: true });

    // first paint is synchronous, never via RAF: the determinism pass runs on a
    // frozen clock where a queued frame would never arrive
    draw();

    return () => {
      if (raf) cancelAnimationFrame(raf);
      raf = 0;
      io.disconnect();
      ro.disconnect();
      reduceQuery.removeEventListener("change", onReduce);
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerdown", onMove);
      window.removeEventListener("pointerup", onLift);
      window.removeEventListener("pointercancel", onLift);
      window.removeEventListener("blur", release);
      window.removeEventListener("scroll", kick);
      root.removeEventListener("pointerleave", release);
      corona = [null, null];
      bg = null;
    };
  }, [cGround, bandCss, liftKey, deepKey, onLight, pitch, density, speed, seed, intensity, reach]);

  return (
    <div
      ref={hostRef}
      className={className}
      aria-hidden="true"
      style={{ position: "absolute", inset: 0, overflow: "hidden", background, ...style }}
    >
      <canvas ref={canvasRef} style={{ display: "block", width: "100%", height: "100%" }} />
    </div>
  );
}
