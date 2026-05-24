import { useState } from "react";
import { useComposerState } from "../mocks/composer";
import { WaveHero } from "../components/WaveHero";
import { Header } from "../components/Header";
import { BucketBar } from "../components/BucketBar";
import { StrikeTiles } from "../components/StrikeTiles";
import { AmountInput } from "../components/AmountInput";
import { Tideline } from "../components/Tideline";
import { WriterPanels, TraderPanels } from "../components/Panels";
import { QuoteFeed } from "../components/QuoteFeed";
import { ConfirmModal } from "../components/ConfirmModal";
import { Toast } from "../components/Toast";
import { LiveBuckets } from "../components/LiveBuckets";
import type { View } from "../types";
import type { ComposerState } from "../mocks/composer";

function assetLabel(s: ComposerState): string {
  return s.series?.asset_symbol ?? "—";
}

function expiryLabel(s: ComposerState): string {
  if (!s.series) return "—";
  const d = new Date(s.series.expiry_iso);
  if (Number.isNaN(d.getTime())) return s.series.expiry_iso;
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

type Props = {
  initialView: View;
};

export function Composer({ initialView }: Props) {
  const s = useComposerState({ initialView });
  const [feedOpen, setFeedOpen] = useState(false);

  const writerCtaLabel = !s.connected
    ? "Connect to write"
    : s.insufficient
      ? "Insufficient balance"
      : s.quotes.length === 0
        ? "Waiting on MMs…"
        : `Earn ${s.bestPremium.toFixed(2)} USDC upfront →`;

  const traderCtaLabel = !s.connected
    ? "Connect to buy"
    : s.insufficientUsdc
      ? `Insufficient USDC · need ${s.selected.premium.toFixed(2)}`
      : s.quotes.length === 0
        ? "Waiting on MMs…"
        : `Buy call · pay ${s.bestPremium.toFixed(2)} USDC →`;

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <WaveHero />
      <Header />

      <div className="app__wrap">
        <LiveBuckets />

        <BucketBar
          symbol={s.selectedAsset}
          capPct={43}
          assets={s.assets}
          selectedAsset={s.selectedAsset}
          onSelectAsset={s.selectAsset}
          expiries={s.expiries}
          selectedExpiryMs={s.selectedExpiryMs}
          onSelectExpiry={s.selectExpiry}
          settlementSymbol={s.series?.settlement_symbol ?? "USDC"}
        />

        <div className="question">
          {s.view === "writer" ? (
            <>
              What strike are you happy to <b>sell</b> {assetLabel(s)} at on {expiryLabel(s)}?
              <span className="qsub">
                Pick a tile. You earn the premium upfront either way.
              </span>
            </>
          ) : (
            <>
              What strike do you want the right to <b>buy</b> {assetLabel(s)} at, before {expiryLabel(s)}?
              <span className="qsub">
                Pick a tile. You pay the premium upfront, exercise anytime.
              </span>
            </>
          )}
        </div>

        {s.bucketsLoading ? (
          <div className="composer-status">loading strikes from indexer…</div>
        ) : s.bucketsEmpty ? (
          <div className="composer-status">
            no buckets available yet — the option-scheduler hasn't created any for this series
          </div>
        ) : (
          <StrikeTiles
            strikes={s.strikes}
            selectedIdx={s.selectedIdx}
            onSelect={s.setSelectedIdx}
            view={s.view}
          />
        )}

        <AmountInput
          amount={s.amount}
          setAmount={s.setAmount}
          view={s.view}
          btcBalance={s.btcBalance}
          usdcBalance={s.usdcBalance}
          error={
            s.view === "writer"
              ? s.insufficientBtc
                ? "INSUFFICIENT BTC BALANCE"
                : ""
              : s.insufficientUsdc
                ? `INSUFFICIENT USDC · NEED ${s.selected.premium.toFixed(2)}`
                : ""
          }
        />

        {s.view === "writer" && <Tideline bucket={s.bucket} amount={s.amount} />}

        {s.view === "writer" ? (
          <WriterPanels
            premium={s.bestPremium}
            amount={s.amount}
            strike={s.selected.strike}
          />
        ) : (
          <TraderPanels
            premium={s.bestPremium}
            amount={s.amount}
            strike={s.selected.strike}
            spot={s.spot}
          />
        )}

        <button
          className="cta"
          onClick={s.submit}
          disabled={
            !s.connected ||
            s.insufficient ||
            s.quotes.length === 0 ||
            s.bucketsLoading ||
            s.bucketsEmpty
          }
        >
          {s.view === "writer" ? writerCtaLabel : traderCtaLabel}
        </button>
      </div>

      {feedOpen ? (
        <QuoteFeed quotes={s.quotes} view={s.view} onClose={() => setFeedOpen(false)} />
      ) : (
        <button className="feed-toggle" onClick={() => setFeedOpen(true)}>
          {s.quotes.length} MM quote{s.quotes.length === 1 ? "" : "s"} live
        </button>
      )}

      {s.confirmStage && (
        <ConfirmModal
          stage={s.confirmStage}
          summary={s.confirmSummary}
          view={s.view}
          onClose={s.closeConfirm}
        />
      )}
      {s.toast && <Toast message={s.toast} />}
    </div>
  );
}
