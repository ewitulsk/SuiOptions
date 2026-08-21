import { AbsoluteFill, interpolate, useCurrentFrame } from "remotion";

type Line = {
  from: [number, number];
  to: [number, number];
  color: string;
  start?: number;
  duration?: number;
};

// Full-screen SVG overlay that draws connecting wires between actors.
export const Wires: React.FC<{ lines: Line[] }> = ({ lines }) => {
  const frame = useCurrentFrame();
  return (
    <AbsoluteFill>
      <svg width="1920" height="1080" style={{ position: "absolute" }}>
        {lines.map((l, i) => {
          const start = l.start ?? 0;
          const dur = l.duration ?? 20;
          const p = interpolate(frame, [start, start + dur], [0, 1], {
            extrapolateLeft: "clamp",
            extrapolateRight: "clamp",
          });
          const x2 = l.from[0] + (l.to[0] - l.from[0]) * p;
          const y2 = l.from[1] + (l.to[1] - l.from[1]) * p;
          return (
            <line
              key={i}
              x1={l.from[0]}
              y1={l.from[1]}
              x2={x2}
              y2={y2}
              stroke={l.color}
              strokeWidth={3}
              strokeDasharray="2 10"
              strokeLinecap="round"
              opacity={0.55}
            />
          );
        })}
      </svg>
    </AbsoluteFill>
  );
};
