# SuiOptions — Write-Flow Explainer Video

A [Remotion](https://www.remotion.dev/) project that animates the **write flow**
of the SuiOptions covered-call protocol: how a retail writer sells a covered call
to a market maker, from RFQ to on-chain fill, ending with the protocol's
pooled-bucket / FIFO-cursor settlement model.

Source of truth for the content: [`../options-protocol-spec.md`](../options-protocol-spec.md).

## Output

- Composition id: `WriteFlowExplainer`
- 1920×1080, 30 fps, ~52 s (1575 frames)
- Rendered to `out/write-flow.mp4`

## Scenes

| # | Scene | What it shows |
|---|-------|---------------|
| 00 | Title | Intro to the write flow |
| 01 | The cast | Writer · Quoting Service · Trader MMs · Sui contracts (§1.2) |
| 02 | Request for quote | `RFQRequest` → service fans out `RFQBroadcast` to MMs (§5.8) |
| 03 | Quotes & reservation | MMs return signed quotes; service validates, reserves, sorts (§5.4, §5.5) |
| 04 | The fill | `execute_write` verifies, skims fee, pays premium, mints Position + CallOption (§3.3.4) |
| 05 | The cursor model | Shared bucket, FIFO `exercise_cursor`, redeem math (§2.2) |
| 06 | Recap | The four steps end to end |

## Develop

```bash
npm install
npm run dev      # opens Remotion Studio (live preview / scrubbing)
```

## Render

```bash
npm run render   # -> out/write-flow.mp4
# equivalent: npx remotion render src/index.ts WriteFlowExplainer out/write-flow.mp4
```

## Structure

```
src/
├── index.ts            registerRoot
├── Root.tsx            <Composition> registration
├── ExplainerVideo.tsx  scene timeline (Series)
├── theme.ts            colors / fonts
├── layout.ts           shared actor positions
├── anim.ts             spring/fade helpers
├── components/         Background, Actor, MessagePacket, Wire, Caption
└── scenes/             one file per scene
```
