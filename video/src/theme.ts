// Shared visual language for the explainer video.
// Palette leans on the Sui blue with a deep-navy backdrop.

export const colors = {
  bg: "#0A1628",
  bgDeep: "#060E1C",
  panel: "#0F2138",
  panelLight: "#16304F",
  stroke: "#23456E",
  text: "#E6EDF6",
  muted: "#8AA1BE",
  sui: "#4DA2FF",
  teal: "#5EEAD4",
  amber: "#FBBF24",
  green: "#34D399",
  red: "#F87171",
  violet: "#A78BFA",
};

// Role -> accent color, used consistently across scenes.
export const roleColor = {
  writer: colors.teal,
  service: colors.sui,
  mm: colors.violet,
  chain: colors.amber,
} as const;

export const fonts = {
  sans:
    '"Inter", -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif',
  mono: '"SFMono-Regular", "JetBrains Mono", Menlo, Consolas, monospace',
};
