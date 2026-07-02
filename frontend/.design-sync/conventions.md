# Tideline ("Aqua") — how to build with this design system

Tideline is the UI of a Sui on-chain options protocol. Its look is **"Aqua"**: frosted glass on a deep underwater gradient, Sora for display, JetBrains Mono for numerals. Components are real React parts that assume the app's runtime context — wire it up exactly as below or they render unstyled or throw.

## 1. Wrapping & setup (required)

Every screen must mount inside the provider stack **and** under a `data-theme="aqua"` element, with the mode set on `<html>`:

```tsx
// document.documentElement.dataset.mode = "dark"   // "dark" (default) | "light"
<QueryClientProvider client={queryClient}>
  <SuiClientProvider networks={networks} defaultNetwork="testnet">
    <WalletProvider>
      <BrowserRouter>
        <div data-theme="aqua">{/* your screen */}</div>
      </BrowserRouter>
    </WalletProvider>
  </SuiClientProvider>
</QueryClientProvider>
```

- **`data-theme="aqua"` is mandatory** — every color/spacing token is scoped under `[data-theme="aqua"]`. Omit it and nothing styles. Dark-mode values are scoped under `html[data-mode="dark"] [data-theme="aqua"]`, so the mode attribute goes on `<html>`.
- The providers are **required by hook-driven components** (Header, TradePanel, Orderbook, OpenOrdersSection, IndexerProgressBar, LiveBuckets, SessionMenu): `@tanstack/react-query` (QueryClientProvider), `@mysten/dapp-kit` (SuiClientProvider + WalletProvider, which itself must sit under QueryClientProvider), and `react-router-dom` (a Router). Missing any → that component throws.
- `window.Tideline.DSProvider` in this bundle is exactly this stack (dark mode + the aqua wrapper) — use it as the reference if you need a quick correct root.

## 2. Styling idiom

Components are **pre-styled via BEM-style classes** scoped under `[data-theme="aqua"]`, driven by `--aqua-*` CSS custom properties. **You do not add classes to library components — you compose them by passing props.** Class families you'll see (owned by the components): `header`, `panel`, `amount`, `modal`, `tile`/`tiles`, `chain`, `tradepanel`, `orderbook`, `moneyness`, `rangebar`, `cursorbar`, `tideline`, `feed`/`qrow`, `cta`, `toast`, `buy`(-grid), `bbar`, `dtabs`, `wallet`, `vault`, `dash`, `pos`.

For **your own layout glue**, style with the design tokens via `var(--aqua-*)` — never hardcode hex:

- Ink/text: `--aqua-ink-1` … `--aqua-ink-4` (primary → faint)
- Surfaces: `--aqua-glass`, `--aqua-glass-2`, `--aqua-glass-3`, `--aqua-solid-surface`
- Lines/dividers: `--aqua-line`, `--aqua-line-2`
- Brand: `--aqua-sui`, `--aqua-sui-deep`, `--aqua-teal`, `--aqua-accent`
- Semantic: `--aqua-success`, `--aqua-coral`, `--aqua-up`, `--aqua-down`
- Strike/moneyness tier ramp: `--aqua-t0` (hot/ITM) … `--aqua-t5` (cool/OTM)
- Background gradient stops: `--aqua-bg-top` / `--aqua-bg-mid` / `--aqua-bg-bot`

Fonts: **Sora** (display, UI), **JetBrains Mono** (numbers, tickers, labels).

## 3. Where the truth lives

- `_ds_bundle.css` (imported by `styles.css`) is the **full Aqua stylesheet** — every class and `--aqua-*` token is defined there. Read it before writing any styling.
- Each component's API is its `components/<group>/<Name>/<Name>.d.ts`; usage notes are in the sibling `<Name>.prompt.md`.

## 4. Idiomatic example

```tsx
import { StrikeTiles, AmountInput } from "tideline-frontend";

// inside the data-theme="aqua" + provider root from §1
<div style={{ display: "grid", gap: "var(--aqua-line)", color: "var(--aqua-ink-1)" }}>
  <StrikeTiles strikes={strikes} selectedIdx={2} onSelect={setIdx} view="writer" />
  <AmountInput
    amount={amount} setAmount={setAmount} view="writer"
    assetSymbol="TBTC" btcBalance={1.2} usdcBalance={5000}
    spot={96000} settlementSymbol="USDC" error=""
  />
</div>
```

Component data (strikes, buckets, positions, quotes) follows the shapes in each component's `.d.ts`. Many components also read live data through the providers above; pass realistic props for static composition.
