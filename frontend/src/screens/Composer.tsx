import { useMemo, useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";
import { useComposerState } from "../state/composer";
import { midFromBook, poolRefFor, useOrderBook } from "../api/deepbook";
import { BuyDetailTabs, type DetailTab } from "../components/BuyDetailTabs";
import { BuyModeToggle, type BuyMode } from "../components/BuyModeToggle";
import { OptionTypeToggle } from "../components/OptionTypeToggle";
import { BucketBar } from "../components/BucketBar";
import { StrikeTiles } from "../components/StrikeTiles";
import { ChainTable } from "../components/ChainTable";
import { AmountInput } from "../components/AmountInput";
import { QueueWave } from "../components/QueueWave";
import { WriterPanels, TraderPanels } from "../components/Panels";
import { ChartPanel } from "../components/ChartPanel";
import { TradePanel } from "../components/TradePanel";
import { QuoteFeed } from "../components/QuoteFeed";
import { ConfirmModal } from "../components/ConfirmModal";
import { Toast } from "../components/Toast";
import { formatPrice } from "../format";
import type { View } from "../types";
import type { ComposerState } from "../state/composer";

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
  const account = useCurrentAccount();
  // Top-of-book mid (settlement per option), shared one source with the order
  // book rail (same `["deepbook-book", poolId]` query). Feeds the metrics panel
  // and `/options/metrics` `mark`. `null` until a strike with a two-sided book.
  const poolRef = useMemo(
    () => (s.apiBucket && s.series ? poolRefFor(s.apiBucket, s.series) : null),
    [s.apiBucket, s.series],
  );
  const book = useOrderBook(poolRef, account?.address ?? null);
  const mid = midFromBook(book.data);
  // SO-170: on /buy, switch the whole lower area between buying on DeepBook
  // (chart + order book + trade form) and minting from the market makers (RFQ).
  const [buyMode, setBuyMode] = useState<BuyMode>("deepbook");
  // Lifted out of BuyDetailTabs so the active tab persists as the user clicks
  // around the strike grid (the panel is keyed per-bucket and remounts).
  const [detailTab, setDetailTab] = useState<DetailTab>("greeks");

  const writerCtaLabel = !s.connected
    ? "Connect to write"
    : s.insufficient
      ? "Insufficient balance"
      : s.quotes.length === 0
        ? "Waiting on MMs…"
        : `Earn ${formatPrice(s.bestPremium)} USDC upfront →`;

  const traderCtaLabel = !s.connected
    ? "Connect to buy"
    : s.insufficientUsdc
      ? `Insufficient USDC · need ${formatPrice(s.bestPremium)}`
      : s.quotes.length === 0
        ? "Waiting on MMs…"
        : `Buy ${s.optionType} · pay ${formatPrice(s.bestPremium)} USDC →`;

  const ctaDisabled =
    !s.connected ||
    s.insufficient ||
    s.quotes.length === 0 ||
    s.bucketsLoading ||
    s.bucketsEmpty ||
    s.confirmStage === "signing" ||
    s.confirmStage === "broadcast";

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <div className={"app__wrap" + (s.view === "trader" ? " app__wrap--buy" : "")}>
        <BucketBar
          symbol={s.selectedAsset}
          assets={s.assets}
          selectedAsset={s.selectedAsset}
          onSelectAsset={s.selectAsset}
          expiries={s.expiries}
          selectedExpiryMs={s.selectedExpiryMs}
          onSelectExpiry={s.selectExpiry}
          settlementSymbol={s.series?.settlement_symbol ?? "USDC"}
        />

        <OptionTypeToggle optionType={s.optionType} onChange={s.setOptionType} />

        <div className="question">
          {s.view === "writer" ? (
            s.optionType === "put" ? (
              <>
                <span className="question__eyebrow">Cash-Secured Puts</span>
                Get paid to buy {s.selectedAsset ?? "crypto"} at your price.
                <span className="qsub">
                  Post USDC collateral, earn premium upfront — buy only if it dips to strike
                </span>
              </>
            ) : (
              <>
                <span className="question__eyebrow">Covered Calls</span>
                Write Options, instantly earn premiums.
                <span className="qsub">Pick an asset, expiry, and strike price</span>
              </>
            )
          ) : (
            <>
              <span className="question__eyebrow">Options Trading</span>
              Buy and sell Options.
              <span className="qsub">Pick an asset, expiry, and strike price</span>
            </>
          )}
        </div>

        {s.view === "trader"
          ? renderTrader()
          : renderWriter()}
      </div>

      {s.confirmStage && (
        <ConfirmModal
          stage={s.confirmStage}
          summary={s.confirmSummary}
          view={s.view}
          optionType={s.optionType}
          onClose={s.closeConfirm}
        />
      )}
      {s.toast && <Toast message={s.toast.message} variant={s.toast.variant} />}
    </div>
  );

  // ---- Trader (/buy): toggle between DeepBook and Market-Maker buy paths ----
  function renderTrader() {
    const live = s.apiBucket?.deepbook_pool_id && s.series;

    const chainInner = s.bucketsLoading ? (
      <div className="composer-status">loading strikes from indexer…</div>
    ) : s.bucketsEmpty ? (
      <div className="composer-status">
        {s.canCreateStrikes
          ? "no strikes yet for this series — writers can create one from the Earn tab"
          : "no buckets yet — the option-scheduler hasn't created any for this series"}
      </div>
    ) : (
      <ChainTable
        buckets={s.apiBuckets}
        strikes={s.strikes}
        series={s.series!}
        spot={s.spot}
        selectedIdx={s.selectedIdx}
        onSelect={s.setSelectedIdx}
      />
    );

    const chart = live ? (
      <ChartPanel
        key={`chart-${s.apiBucket!.bucket_id}`}
        poolId={s.apiBucket!.deepbook_pool_id!}
        strike={s.apiBucket!.strike}
        settlementSymbol={s.series!.settlement_symbol}
      />
    ) : (
      !s.bucketsLoading &&
      !s.bucketsEmpty && (
        <div className="composer-status">select a strike to see its market</div>
      )
    );

    return (
      <>
        <BuyModeToggle mode={buyMode} onChange={setBuyMode} />

        {buyMode === "deepbook" ? (
          <div className="buy-grid buy-grid--deepbook">
            <aside className="buy-grid__buckets">
              {chainInner}
              {live && (
                <BuyDetailTabs
                  key={`detail-${s.apiBucket!.bucket_id}`}
                  bucket={s.apiBucket!}
                  series={s.series!}
                  spot={s.spot}
                  mid={mid}
                  wallet={s.address}
                  tab={detailTab}
                  onTabChange={setDetailTab}
                />
              )}
            </aside>
            <main className="buy-grid__center">
              {live ? (
                <>
                  {chart}
                  <TradePanel
                    key={`trade-${s.apiBucket!.bucket_id}`}
                    bucket={s.apiBucket!}
                    series={s.series!}
                  />
                </>
              ) : (
                chart
              )}
            </main>
          </div>
        ) : (
          <div className="buy-grid">
            <aside className="buy-grid__buckets">{chainInner}</aside>
            <main className="buy-grid__center">
              {/* Quote, premium, and the buy button come first so they're
                  visible without scrolling; the chart drops below the
                  actionable content (SO-225). */}
              <div className="rfq__main">
                <AmountInput
                  amount={s.amount}
                  setAmount={s.setAmount}
                  view={s.view}
                  optionType={s.optionType}
                  assetSymbol={s.selectedAsset}
                  btcBalance={s.btcBalance}
                  usdcBalance={s.usdcBalance}
                  spot={s.spot}
                  strike={s.selected.strike}
                  settlementSymbol={s.series?.settlement_symbol ?? "USDC"}
                  error={
                    s.insufficientUsdc
                      ? `INSUFFICIENT USDC · NEED ${formatPrice(s.bestPremium)}`
                      : ""
                  }
                />

                {s.spotUnavailable && (
                  <div className="composer-status">
                    spot price unavailable — live feed not yet connected for {s.selectedAsset}
                  </div>
                )}

                <TraderPanels
                  premium={s.bestPremium}
                  premiumLoading={s.premiumLoading}
                  amount={s.amount}
                  strike={s.selected.strike}
                  spot={s.spot}
                  assetSymbol={s.selectedAsset}
                  expiryLabel={expiryLabel(s)}
                />

                <button className="cta" onClick={s.submit} disabled={ctaDisabled}>
                  {traderCtaLabel}
                </button>
              </div>
            </main>
            <aside className="buy-grid__ticket">
              <QuoteFeed quotes={s.quotes} view={s.view} docked />
            </aside>
          </div>
        )}
      </>
    );
  }

  // ---- Writer (/earn): unchanged vertical stack ----------------------------
  function renderWriter() {
    return (
      <>
        {s.bucketsLoading ? (
          <div className="composer-status">loading strikes from indexer…</div>
        ) : s.bucketsEmpty ? (
          <div className="composer-status">
            {s.canCreateStrikes
              ? "no strikes yet for this series — create the first one below"
              : "no buckets available yet — the option-scheduler hasn't created any for this series"}
          </div>
        ) : (
          <StrikeTiles
            strikes={s.strikes}
            selectedIdx={s.selectedIdx}
            onSelect={s.setSelectedIdx}
            view={s.view}
          />
        )}
        {s.canCreateStrikes && (
          <form
            className="custom-strike"
            onSubmit={(e) => {
              e.preventDefault();
              void s.createCustomStrike();
            }}
          >
            <input
              className="custom-strike__input"
              placeholder="custom strike (USD)"
              inputMode="decimal"
              value={s.customStrike}
              onChange={(e) => s.setCustomStrike(e.target.value)}
              disabled={s.creatingBucket}
            />
            <button
              className="custom-strike__btn"
              type="submit"
              disabled={s.creatingBucket || !s.customStrike.trim()}
            >
              {s.creatingBucket ? "creating…" : "create strike"}
            </button>
          </form>
        )}

        <AmountInput
          amount={s.amount}
          setAmount={s.setAmount}
          view={s.view}
          optionType={s.optionType}
          assetSymbol={s.selectedAsset}
          btcBalance={s.btcBalance}
          usdcBalance={s.usdcBalance}
          spot={s.spot}
          strike={s.selected.strike}
          settlementSymbol={s.series?.settlement_symbol ?? "USDC"}
          error={
            !s.insufficientBtc
              ? ""
              : s.optionType === "put"
                ? `INSUFFICIENT USDC · NEED ${formatPrice(s.putCollateral)} COLLATERAL`
                : `INSUFFICIENT ${s.selectedAsset ?? ""} BALANCE`.replace(/\s+/g, " ").trim()
          }
        />

        <QueueWave bucket={s.bucket} amount={s.amount} assetSymbol={s.selectedAsset} />

        <WriterPanels
          premium={s.bestPremium}
          premiumLoading={s.premiumLoading}
          amount={s.amount}
          strike={s.selected.strike}
          optionType={s.optionType}
          assetSymbol={s.selectedAsset}
          expiryLabel={expiryLabel(s)}
        />

        <button className="cta" onClick={s.submit} disabled={ctaDisabled}>
          {writerCtaLabel}
        </button>
      </>
    );
  }
}
