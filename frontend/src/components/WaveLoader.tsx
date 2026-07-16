// Cresting wave line shown in place of the hero premium while a firm RFQ
// quote is in flight. Same ocean vocabulary as the header waves and the
// writer QueueWave: the SVG spans twice the visible window and drifts left by
// half its width (aqua-wave) for a seamless loop, with a gentle vertical swell.

const WIDTH = 576; // viewBox width — two identical halves so -50% loops cleanly
const WAVELENGTH = 96; // ~3 calm crests across the visible window
const AMP = 5;
const BASE = 12; // baseline; viewBox height is BASE * 2

// Periodic over WIDTH and over WIDTH/2, so translateX(-50%) lands crest-on-crest.
function wavePath(): string {
  const half = WAVELENGTH / 2;
  let d = `M 0 ${BASE}`;
  let up = true;
  for (let x = 0; x < WIDTH; x += half) {
    const cy = up ? BASE - AMP : BASE + AMP;
    d += ` Q ${x + half / 2} ${cy} ${x + half} ${BASE}`;
    up = !up;
  }
  return d;
}

const PATH = wavePath();

export function WaveLoader({ className }: { className?: string }) {
  return (
    <span
      className={"wave-loader" + (className ? ` ${className}` : "")}
      role="status"
      aria-label="fetching quote"
    >
      <svg
        className="wave-loader__svg"
        viewBox={`0 0 ${WIDTH} ${BASE * 2}`}
        preserveAspectRatio="none"
        aria-hidden="true"
      >
        <path className="wave-loader__wave wave-loader__wave--back" d={PATH} />
        <path className="wave-loader__wave" d={PATH} />
      </svg>
    </span>
  );
}
