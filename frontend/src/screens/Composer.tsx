import { useMemo, useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";
import { useComposerState } from "../state/composer";
import { midFromBook, poolRefFor, useOrderBook } from "../api/deepbook";
import { OptionMetricsPanel } from "../components/OptionMetricsPanel";
import { BucketBar } from "../components/BucketBar";
import { StrikeTiles } from "../components/StrikeTiles";
import { BucketList } from "../components/BucketList";
import { AmountInput } from "../components/AmountInput";
import { Tideline } from "../components/Tideline";
import { WriterPanels, TraderPanels } from "../components/Panels";
import { ChartPanel } from "../components/ChartPanel";
import { TradePanel } from "../components/TradePanel";
import { Orderbook } from "../components/Orderbook";
import { QuoteFeed } from "../components/QuoteFeed";
import { ConfirmModal } from "../components/ConfirmModal";
import { Toast } from "../components/Toast";
import { formatPrice } from "../format";
import type { View } from "../types";
import type { ComposerState } from "../state/composer";

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
  const [feedOpen, setFeedOpen] = useState(false);
  // SO-170: on /buy, switch the whole lower area between buying on DeepBook
  // (chart + order book + trade form) and minting from the market makers (RFQ).
  const [buyMode, setBuyMode] = useState<"deepbook" | "mm">("deepbook");

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
        : `Buy call · pay ${formatPrice(s.bestPremium)} USDC →`;

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

        <div className="question">
          {s.view === "writer" ? (
            <>
              What strike are you happy to <b>sell</b> {assetLabel(s)} at on {expiryLabel(s)}?
              <span className="qsub">
                Pick a strike. You earn the premium upfront either way.
              </span>
            </>
          ) : (
            <>
              What strike do you want the right to <b>buy</b> {assetLabel(s)} at, before {expiryLabel(s)}?
              <span className="qsub">
                Pick a strike from the left. Trade it on DeepBook, or mint a fresh one at the MMs' quote.
              </span>
            </>
          )}
        </div>

        {s.view === "trader"
          ? renderTrader()
          : renderWriter()}
      </div>

      {/* Writer keeps the floating quote-feed overlay; the trader feed is docked
          into the RFQ section below (SO-170). */}
      {s.view === "writer" &&
        (feedOpen ? (
          <QuoteFeed quotes={s.quotes} view={s.view} onClose={() => setFeedOpen(false)} />
        ) : (
          <button className="feed-toggle" onClick={() => setFeedOpen(true)}>
            {s.quotes.length} MM quote{s.quotes.length === 1 ? "" : "s"} live
          </button>
        ))}

      {s.confirmStage && (
        <ConfirmModal
          stage={s.confirmStage}
          summary={s.confirmSummary}
          view={s.view}
          onClose={s.closeConfirm}
        />
      )}
      {s.toast && <Toast message={s.toast.message} variant={s.toast.variant} />}
    </div>
  );

  // ---- Trader (/buy): toggle between DeepBook and Market-Maker buy paths ----
  function renderTrader() {
    const live = s.apiBucket?.deepbook_pool_id && s.series;

    const buckets = (
      <aside className="buy-grid__buckets">
        {s.bucketsLoading ? (
          <div className="composer-status">loading strikes from indexer…</div>
        ) : s.bucketsEmpty ? (
          <div className="composer-status">
            no buckets yet — the option-scheduler hasn't created any for this series
          </div>
        ) : (
          <BucketList
            strikes={s.strikes}
            selectedIdx={s.selectedIdx}
            onSelect={s.setSelectedIdx}
            view={s.view}
          />
        )}
      </aside>
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

    const toggle = (
      <div className="buy-toggle" role="tablist">
        {(["deepbook", "mm"] as const).map((m) => (
          <button
            key={m}
            role="tab"
            aria-selected={buyMode === m}
            className={"buy-toggle__btn" + (buyMode === m ? " is-active" : "")}
            onClick={() => setBuyMode(m)}
          >
            {m === "deepbook" ? "Trade on DeepBook" : "Buy from market makers"}
          </button>
        ))}
      </div>
    );

    return (
      <>
        {toggle}

        {buyMode === "deepbook" ? (
          <div className="buy-grid">
            {buckets}
            <main className="buy-grid__center">
              {live ? (
                <>
                  {chart}
                  <OptionMetricsPanel
                    key={`metrics-${s.apiBucket!.bucket_id}`}
                    bucket={s.apiBucket!}
                    series={s.series!}
                    spot={s.spot}
                    mid={mid}
                    wallet={s.address}
                  />
                  <div className="buy-grid__trade">
                    <TradePanel
                      key={`trade-${s.apiBucket!.bucket_id}`}
                      bucket={s.apiBucket!}
                      series={s.series!}
                    />
                  </div>
                </>
              ) : (
                chart
              )}
            </main>
            <aside className="buy-grid__book">
              {live && <Orderbook bucket={s.apiBucket!} series={s.series!} />}
            </aside>
          </div>
        ) : (
          <>
            {/* Keep the 3-column grid (empty right rail) so the chart's middle
                column is the same width as DeepBook mode and doesn't resize when
                toggling. */}
            <div className="buy-grid">
              {buckets}
              <main className="buy-grid__center">{chart}</main>
              <aside className="buy-grid__book" />
            </div>

            <section className="rfq">
              <div className="rfq__body">
                <div className="rfq__main">
                  <AmountInput
                    amount={s.amount}
                    setAmount={s.setAmount}
                    view={s.view}
                    assetSymbol={s.selectedAsset}
                    btcBalance={s.btcBalance}
                    usdcBalance={s.usdcBalance}
                    spot={s.spot}
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

                <div className="rfq__feed">
                  <QuoteFeed quotes={s.quotes} view={s.view} docked />
                </div>
              </div>
            </section>
          </>
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
          assetSymbol={s.selectedAsset}
          btcBalance={s.btcBalance}
          usdcBalance={s.usdcBalance}
          spot={s.spot}
          settlementSymbol={s.series?.settlement_symbol ?? "USDC"}
          error={
            s.insufficientBtc
              ? `INSUFFICIENT ${s.selectedAsset ?? ""} BALANCE`.replace(/\s+/g, " ").trim()
              : ""
          }
        />

        <Tideline bucket={s.bucket} amount={s.amount} assetSymbol={s.selectedAsset} />

        <WriterPanels
          premium={s.bestPremium}
          premiumLoading={s.premiumLoading}
          amount={s.amount}
          strike={s.selected.strike}
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
