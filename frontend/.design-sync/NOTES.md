# design-sync notes — tideline-frontend

Repo-specific gotchas for future syncs. The DS package is `frontend/` (run all design-sync commands from there).

## Source shape
- This is a **Vite app, not a library** — there is no `dist/` library entry and no `exports` in `package.json`. The converter runs in **synth-entry mode**: `cfg.entry = "__nodist__.js"` (a deliberately nonexistent path) forces `resolveDistEntry(soft)→null` so it synthesizes an entry from `src/`, while the PKG_DIR walk-up still lands on `frontend/package.json`.
- `cfg.srcDir = "src/components"` scopes discovery to the 26 component files (yields 28 components — a couple of files export more than one). Bundling still follows imports out to `../api`, `../state`, `../config`, `../session`, etc.

## Bundle size — the WalletConnect stub (load-bearing)
- The full app dependency graph is ~12 MB of input → 5.7 MB IIFE, **over claude.ai/design's 5 MB hard cap**.
- The entire overage is the WalletConnect stack — `@reown/appkit*` (~5 MB), `@walletconnect/utils` (1.5 MB), `viem` (1.2 MB), `poseidon-lite` (0.6 MB) — pulled in solely by `session/wallets.ts`'s dynamic `import("@walletconnect/ethereum-provider")`. `session/store.ts` imports `wallets.ts` statically, and `store.ts` is reached by Header, TradePanel, OpenOrdersSection, SessionMenu — so the stack rides in even without SessionMenu.
- **Fix:** `.design-sync/tsconfig.dssync.json` (set as `cfg.tsconfig`) aliases `@walletconnect/ethereum-provider` → `.design-sync/wc-stub.ts` via `compilerOptions.paths`. The converter's `tsconfigPathsPlugin` reads paths from that file **literally (does NOT follow `extends`)**, so the alias must live directly in it. Bundle drops to ~1.65 MB. The real package is uninstalled — the stub makes it unnecessary, and the WC connect path only runs on user click, never in a static preview.
- The two stub files (`wc-stub.ts`, `tsconfig.dssync.json`) are committed durable inputs. Do not delete them or the bundle goes back over 5 MB.

## Providers / theme (for previews — TBD)
- App provider stack (`src/main.tsx`): PostHogProvider → QueryClientProvider → SuiClientProvider → WalletProvider → BrowserRouter.
- Theme is attribute-driven: `data-mode` (`light`/`dark`, default dark) on `<html>` via `src/theme.ts`; **`aqua.css` tokens are scoped under `[data-theme="aqua"]`**, set on a wrapper `<div>` in `App.tsx`. Previews need that wrapper or every `--aqua-*` token is unresolved → unstyled.
- Token catalog (`config.ts` `findToken`) is populated by async `initConfig()` (network fetch from token-info) — not available in headless preview; author previews with explicit props instead.
- Fonts: Sora + JetBrains Mono are loaded from Google Fonts via a `<link>` in `index.html` (not shipped). Expect `[FONT_MISSING]` — resolve via remote `@import` or `runtimeFontPrefixes`.

## Previews (authored §4)
- **Provider** (`.design-sync/ds-provider.tsx`, cfg.provider=DSProvider, cfg.extraEntries): wraps every preview in QueryClientProvider → SuiClientProvider → WalletProvider → MemoryRouter → `<div data-theme="aqua">`. Renders in the app's **dark** mode (set at module load + a `useLayoutEffect`). The wrapper has `transform: translateZ(0)` so `position:fixed`/sticky bits (toast, header, modal scrims) are contained in-card instead of escaping to the viewport.
  - **Consequence for modals**: the transform makes the modal's `position:fixed; inset:0` scrim size to the wrapper, not the viewport. ActionModal/ConfirmModal previews therefore wrap the modal in a tall in-flow `Frame` spacer (860 / 680px) so the wrapper grows and the centered modal isn't clipped. Don't remove the Frame.
- **Authored previews**: 27 components in `.design-sync/previews/`, all graded good. Import the component from `"tideline-frontend"` (shimmed to `window.Tideline`); pass realistic props — no provider/theme/data-theme in the preview itself.
- **SessionMenu**: floor card (no preview). It returns `null` unless `useSession().handle` is set and the session store has no injection seam, so it can't render in a static preview without seeding the store from the provider. Left as the deliberate floor baseline.
- **Backend-gated states (graded good as legitimate states, not failures)**: ChartPanel ("No quotes yet" chrome), IndexerProgressBar ("indexer status unavailable"), LiveBuckets ("failed to fetch"), TradePanel (BalanceManager setup card), Orderbook ("book is empty"), OpenOrdersSection ("enable trading" prompt), ConnectMenu (disconnected connect pill). Their populated states need live api-service / DeepBook / a connected wallet, which previews don't have. To upgrade them later, seed react-query in DSProvider with `setQueryData` for keys like `["deepbook-book", poolId]`, the `/indexer/progress` + `/buckets` fetches, and `useBars` — but that's a provider change and re-invalidates ALL grades.
- **Pyth SSE gotcha**: `usePythPrice`/`usePythPrices` open a persistent Hermes EventSource; with a resolvable symbol the capture's networkidle `page.goto` times out (20s). Keep Pyth-driven previews on `symbol={null}` (BucketBar does this).
- **cfg.overrides**: StrikeTiles `cardMode:column` (wide tile row); Header `cardMode:single` (full-width nav); Toast `cardMode:single`+primaryStory Success (fixed-position escapes the grid); ActionModal/ConfirmModal `cardMode:single`+viewport (tall modals).

## Re-sync risks (watch-list)
- **dtsPropsFor in config are hand-inlined snapshots** of `src/types.ts` + `src/api/client.ts` shapes (Strike, Quote, Bucket, Series, OwnedPosition, WrittenPosition, ConfirmSummary, DashboardModal). If those domain types change, update `cfg.dtsPropsFor` — the synth-entry DTS extractor only produces `[key:string]:unknown`, so these are maintained by hand.
- **Fonts** are a pinned Google Fonts woff2 snapshot (Sora + JetBrains Mono, latin/latin-ext) under `.design-sync/fonts/`. Re-fetch if the brand font set changes.
- **WC stub** must stay (`wc-stub.ts` + `tsconfig.dssync.json`) or the bundle exceeds 5 MB again.
- Backend-gated previews are tied to current component data-fetching code; if a component switches from internal fetch to props, its preview can be upgraded to a populated state.

## Target project
- Synced into the pre-existing hand-built project `019e2eaa-b787-7142-9424-594bf76b8d24` ("Tideline — Sui Options Design System") at the user's explicit request (atomic path, non-empty target). It already contains a hand-curated `ui_kits/`, `preview/`, `assets/`, `colors_and_type.css`, `SKILL.md` — those are NOT converter output; the delete list must be confirmed with the user before reconciliation.
