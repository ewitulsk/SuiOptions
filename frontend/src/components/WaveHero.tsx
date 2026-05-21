// Layered SVG waves — three overlapping sinusoids at different speeds.
// The svg viewBox is 2× wider than visible so we can translateX(-50%) loop seamlessly.
export function WaveHero() {
  return (
    <div className="wave-hero">
      <svg viewBox="0 0 2400 84" preserveAspectRatio="none">
        <path
          className="wave-back"
          d="M0,52 C200,72 400,32 600,52 C800,72 1000,32 1200,52 C1400,72 1600,32 1800,52 C2000,72 2200,32 2400,52 L2400,84 L0,84 Z"
          fill="#A4D5F7"
          opacity="0.45"
        />
        <path
          className="wave-mid"
          d="M0,58 C200,40 400,76 600,58 C800,40 1000,76 1200,58 C1400,40 1600,76 1800,58 C2000,40 2200,76 2400,58 L2400,84 L0,84 Z"
          fill="#7BBFEF"
          opacity="0.55"
        />
        <path
          className="wave-front"
          d="M0,64 C200,80 400,48 600,64 C800,80 1000,48 1200,64 C1400,80 1600,48 1800,64 C2000,80 2200,48 2400,64 L2400,84 L0,84 Z"
          fill="#4DA2FF"
          opacity="0.7"
        />
      </svg>
    </div>
  );
}
