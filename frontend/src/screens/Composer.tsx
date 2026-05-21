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
import type { View } from "../types";

type Props = {
  initialView: View;
  onNavigate: (target: string) => void;
};

export function Composer({ initialView, onNavigate }: Props) {
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
      <Header
        screen="composer"
        view={s.view}
        setView={s.setView}
        connected={s.connected}
        onConnect={() => s.setConnected((c) => !c)}
        onNavigate={onNavigate}
      />

      <div className="app__wrap">
        <BucketBar spot={s.spot} capPct={43} />

        <div className="question">
          {s.view === "writer" ? (
            <>
              What strike are you happy to <b>sell</b> BTC at on Jun 26?
              <span className="qsub">
                Pick a tile. You earn the premium upfront either way.
              </span>
            </>
          ) : (
            <>
              What strike do you want the right to <b>buy</b> BTC at, before Jun 26?
              <span className="qsub">
                Pick a tile. You pay the premium upfront, exercise anytime.
              </span>
            </>
          )}
        </div>

        <StrikeTiles
          strikes={s.strikes}
          selectedIdx={s.selectedIdx}
          onSelect={s.setSelectedIdx}
          view={s.view}
        />

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
          disabled={!s.connected || s.insufficient || s.quotes.length === 0}
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
