import { findToken } from "../config";

type Props = {
  /** Market symbol — on-chain ticker (`TBTC`) or display alias (`BTC`). */
  symbol: string | null | undefined;
  /** Container class so each call site keeps its existing icon sizing/shape. */
  className: string;
  /** Rendered when token-info serves no logo for the symbol (e.g. native SUI). */
  fallback: React.ReactNode;
};

/**
 * Render the token's logo from the token-info catalog (`logoUri`). Falls back to
 * the call site's existing glyph when the catalog has no logo for the symbol.
 */
export function TokenLogo({ symbol, className, fallback }: Props) {
  const token = findToken(symbol);
  if (!token?.logoUri) return <>{fallback}</>;
  return (
    <span className={className + " token-logo"}>
      <img
        className="token-logo__img"
        src={token.logoUri}
        alt={token.name || symbol || ""}
        title={token.name || undefined}
      />
    </span>
  );
}
