import { TokenLogo } from "tideline-frontend";

// The token-info catalog is absent in preview, so findToken returns nothing and
// TokenLogo renders the call site's `fallback` node — the real fallback path.
// These mirror the asset glyphs used by the position cards.

const btcGlyph = <span className="asset-glyph asset-glyph--btc">₿</span>;
const suiGlyph = <span className="asset-glyph asset-glyph--sui">≈</span>;
const usdcGlyph = <span className="asset-glyph">U</span>;

// BTC ticker → bitcoin glyph fallback.
export const BtcFallback = () => (
  <TokenLogo symbol="BTC" className="asset-glyph" fallback={btcGlyph} />
);

// SUI ticker → wave glyph fallback.
export const SuiFallback = () => (
  <TokenLogo symbol="SUI" className="asset-glyph" fallback={suiGlyph} />
);

// Unknown symbol → initial-letter fallback.
export const InitialFallback = () => (
  <TokenLogo symbol="USDC" className="asset-glyph" fallback={usdcGlyph} />
);

// Small sizing class — the fallback respects each call site's icon dimensions.
export const SmallFallback = () => (
  <TokenLogo
    symbol="TBTC"
    className="asset-glyph asset-glyph--sm"
    fallback={<span className="asset-glyph asset-glyph--sm asset-glyph--btc">₿</span>}
  />
);
