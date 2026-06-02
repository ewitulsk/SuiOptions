import { interpolate, spring } from "remotion";

// Springy 0->1 entrance value, delayed by `delay` frames.
export const rise = (frame: number, fps: number, delay = 0, damping = 200) =>
  spring({
    frame: frame - delay,
    fps,
    config: { damping, mass: 0.8, stiffness: 120 },
  });

// Linear fade between two frames, clamped.
export const fade = (frame: number, from: number, to: number) =>
  interpolate(frame, [from, to], [0, 1], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
  });

// Fade in then out: 1 inside [inStart..outStart], 0 outside the ramps.
export const pulse = (
  frame: number,
  inStart: number,
  inEnd: number,
  outStart: number,
  outEnd: number,
) =>
  interpolate(frame, [inStart, inEnd, outStart, outEnd], [0, 1, 1, 0], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
  });
