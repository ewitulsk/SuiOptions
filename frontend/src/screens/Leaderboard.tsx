// Public leaderboard tab: ranked accounts by points earned from configured
// on-chain events (go-backend leaderboard service, api/leaderboard.ts).
//
// Filters: time window (all/30d/7d/24h), points source, wallet search. The
// connected wallet's position — and any searched wallet's — renders as a
// pinned card with its ranked neighbors; neighbors stay in that card and are
// never spliced into the paged table below. Rows expand into a lazy
// per-source breakdown of where the account's points came from.

import { Fragment, useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";

import {
  LeaderboardUnavailableError,
  useLeaderboard,
  useLeaderboardBreakdown,
  useLeaderboardRank,
  useLeaderboardSources,
  type LeaderboardEntry,
  type LeaderboardSource,
  type LeaderboardWindow,
} from "../api/leaderboard";
import { Address } from "../components/Address";
import { WaveLoader } from "../components/WaveLoader";
import { useSegmentPill } from "../lib/useSegmentPill";

const PAGE = 50;

const WINDOWS: { key: LeaderboardWindow; label: string }[] = [
  { key: "all", label: "All" },
  { key: "30d", label: "30d" },
  { key: "7d", label: "7d" },
  { key: "24h", label: "24h" },
];

/** Committed wallet-search values: 0x plus 1–64 hex chars. */
const ADDRESS_RE = /^0x[0-9a-fA-F]{1,64}$/;

// Column template shared by the head and rows (vault-table's default grid is
// the covered-call 5-column layout, so override inline).
const GRID: React.CSSProperties = {
  gridTemplateColumns: "0.5fr 1.8fr 1fr 0.8fr 28px",
};

function RangePills({
  window: win,
  setWindow,
}: {
  window: LeaderboardWindow;
  setWindow: (w: LeaderboardWindow) => void;
}) {
  const { ref, geom: pill, animated } = useSegmentPill(win);
  return (
    <div className="act-filters ana-ranges" role="tablist" ref={ref}>
      <span
        className="act-filter__pill"
        aria-hidden
        style={{
          transform: `translate(${pill.left}px, ${pill.top}px)`,
          width: pill.width,
          height: pill.height,
          opacity: pill.ready ? 1 : 0,
          transition: animated ? undefined : "none",
        }}
      />
      {WINDOWS.map((w) => (
        <button
          key={w.key}
          role="tab"
          aria-selected={win === w.key}
          className={"act-filter" + (win === w.key ? " is-active" : "")}
          onClick={() => setWindow(w.key)}
        >
          {w.label}
        </button>
      ))}
    </div>
  );
}

function SourceSelect({
  sources,
  source,
  setSource,
}: {
  sources: LeaderboardSource[];
  source: string;
  setSource: (s: string) => void;
}) {
  return (
    <select
      className="lb-select"
      aria-label="Points source"
      value={source}
      onChange={(e) => setSource(e.target.value)}
    >
      <option value="">All sources</option>
      {sources.map((s) => (
        <option key={s.source} value={s.source}>
          {s.label ?? s.source}
        </option>
      ))}
    </select>
  );
}

function SearchBox({
  committed,
  onCommit,
  onClear,
}: {
  committed: string | null;
  onCommit: (addr: string) => void;
  onClear: () => void;
}) {
  const [draft, setDraft] = useState(committed ?? "");
  const trimmed = draft.trim();
  const valid = ADDRESS_RE.test(trimmed);
  return (
    <div className="lb-search">
      <input
        className="lb-search__input"
        placeholder="Search wallet 0x…"
        aria-label="Search wallet"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && valid) onCommit(trimmed);
        }}
      />
      {(draft !== "" || committed !== null) && (
        <button
          type="button"
          className="lb-search__clear"
          aria-label="Clear search"
          onClick={() => {
            setDraft("");
            onClear();
          }}
        >
          ×
        </button>
      )}
    </div>
  );
}

/** Pinned position card: big rank + points for one wallet, with its ranked
 * neighbors. Rendered for the connected wallet always and for committed
 * searches — neighbors live here, never spliced into the paged table. */
function PositionCard({
  address,
  isYou,
  window: win,
  onGoToPage,
}: {
  address: string;
  isYou: boolean;
  window: LeaderboardWindow;
  onGoToPage: (offset: number) => void;
}) {
  const rankQ = useLeaderboardRank(address, win);
  const title = isYou ? "Your position" : "Search result";
  const short = `${address.slice(0, 6)}…${address.slice(-4)}`;

  if (rankQ.isLoading) {
    return (
      <div className="lb-pos">
        <div className="lb-pos__head">{title}</div>
        <div className="lb-pos__hint">looking up {short}…</div>
      </div>
    );
  }
  if (rankQ.error) {
    return (
      <div className="lb-pos">
        <div className="lb-pos__head">{title}</div>
        <div className="lb-pos__hint">
          {rankQ.error instanceof LeaderboardUnavailableError
            ? "leaderboard temporarily unavailable."
            : `couldn't look up ${short} · ${rankQ.error.message}`}
        </div>
      </div>
    );
  }
  const rank = rankQ.data;
  if (!rank) {
    return (
      <div className="lb-pos">
        <div className="lb-pos__head">{title}</div>
        <div className="lb-pos__hint">
          <Address value={address} /> has no points in this window yet.
        </div>
      </div>
    );
  }

  const topPct = Math.max(1, Math.ceil((rank.rank / Math.max(1, rank.total_accounts)) * 100));
  const page = Math.floor((rank.rank - 1) / PAGE) + 1;
  return (
    <div className="lb-pos">
      <div className="lb-pos__head">
        {title} · <Address value={address} />
        {rank.twitter && <span className="lb-pos__sub">@{rank.twitter}</span>}
      </div>
      <div className="lb-pos__stats">
        <span className="lb-pos__rank">#{rank.rank}</span>
        <span className="lb-pos__points">
          {rank.points.toLocaleString("en-US")}
          <span className="lb-pos__sub"> pts</span>
        </span>
        <span className="lb-pos__pct">top {topPct}%</span>
        <button className="lb-pager__btn" onClick={() => onGoToPage((page - 1) * PAGE)}>
          Go to page {page}
        </button>
      </div>
      {rank.neighbors.length > 0 && (
        <div className="lb-pos__neighbors">
          {rank.neighbors.map((n) => (
            <div
              key={n.account_id}
              className={
                "lb-pos__neighbor" + (n.account_id === rank.account_id ? " lb-row--you" : "")
              }
            >
              <span>#{n.rank}</span>
              <span>
                {n.wallets[0] ? (
                  <Address value={n.wallets[0]} />
                ) : n.twitter ? (
                  `@${n.twitter}`
                ) : (
                  `account ${n.account_id}`
                )}
              </span>
              <span>{n.points.toLocaleString("en-US")} pts</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** Lazy per-source breakdown — mounted (and therefore fetched) only while
 * the row is expanded. */
function BreakdownPanel({
  wallet,
  window: win,
}: {
  wallet: string | null;
  window: LeaderboardWindow;
}) {
  const q = useLeaderboardBreakdown(wallet, win);
  if (!wallet) {
    return <div className="lb-breakdown lb-breakdown--note">no wallet linked to this account.</div>;
  }
  if (q.isLoading) return <div className="lb-breakdown lb-breakdown--note">loading breakdown…</div>;
  if (q.error) {
    return (
      <div className="lb-breakdown lb-breakdown--note">
        couldn't load the breakdown · {q.error.message}
      </div>
    );
  }
  const bd = q.data;
  if (!bd || bd.by_source.length === 0) {
    return <div className="lb-breakdown lb-breakdown--note">no points in this window.</div>;
  }
  return (
    <div className="lb-breakdown">
      {bd.by_source.map((s) => (
        <div className="lb-breakdown__row" key={s.source}>
          <span>{s.label ?? s.source}</span>
          <span>{s.event_count.toLocaleString("en-US")} events</span>
          <span>
            {s.last_event_ms !== null
              ? `last ${new Date(s.last_event_ms).toLocaleDateString()}`
              : "—"}
          </span>
          <span>{s.points.toLocaleString("en-US")} pts</span>
        </div>
      ))}
      <div className="lb-breakdown__row lb-breakdown__total">
        <span>Total</span>
        <span />
        <span />
        <span>{bd.total.toLocaleString("en-US")} pts</span>
      </div>
    </div>
  );
}

function Pager({
  offset,
  total,
  onOffset,
}: {
  offset: number;
  total: number;
  onOffset: (o: number) => void;
}) {
  const page = Math.floor(offset / PAGE) + 1;
  const pages = Math.max(1, Math.ceil(total / PAGE));
  return (
    <div className="lb-pager">
      <button
        className="lb-pager__btn"
        disabled={offset === 0}
        onClick={() => onOffset(Math.max(0, offset - PAGE))}
      >
        ‹ Prev
      </button>
      <span className="lb-pager__info">
        page {page} of {pages}
      </span>
      <button
        className="lb-pager__btn"
        disabled={offset + PAGE >= total}
        onClick={() => onOffset(offset + PAGE)}
      >
        Next ›
      </button>
    </div>
  );
}

function LeaderboardRow({
  entry,
  isYou,
  expanded,
  onToggle,
}: {
  entry: LeaderboardEntry;
  isYou: boolean;
  expanded: boolean;
  onToggle: () => void;
}) {
  return (
    <div
      className={"vault-table__row" + (isYou ? " lb-row--you" : "")}
      style={{ ...GRID, cursor: "pointer", alignItems: "center" }}
      onClick={onToggle}
      role="button"
      tabIndex={0}
      aria-expanded={expanded}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onToggle();
        }
      }}
    >
      <span data-label="Rank" className={entry.rank <= 3 ? "lb-rank--top" : undefined}>
        #{entry.rank}
      </span>
      <span data-label="Account">
        {entry.wallets[0] ? (
          <Address value={entry.wallets[0]} />
        ) : entry.twitter ? (
          `@${entry.twitter}`
        ) : (
          `account ${entry.account_id}`
        )}
        {entry.wallets[0] && entry.twitter && (
          <span className="lb-cell__sub"> @{entry.twitter}</span>
        )}
      </span>
      <span data-label="Points">{entry.points.toLocaleString("en-US")}</span>
      <span data-label="Events">{entry.event_count.toLocaleString("en-US")}</span>
      <span data-label="" aria-hidden>
        {expanded ? "▴" : "▾"}
      </span>
    </div>
  );
}

export function Leaderboard() {
  const account = useCurrentAccount();
  const connected = account?.address ?? null;

  const [window, setWindow] = useState<LeaderboardWindow>("all");
  const [source, setSource] = useState("");
  const [offset, setOffset] = useState(0);
  const [searchAddress, setSearchAddress] = useState<string | null>(null);
  // Accordion: at most one expanded row, keyed by account id.
  const [expanded, setExpanded] = useState<number | null>(null);

  const sourcesQ = useLeaderboardSources();
  const boardQ = useLeaderboard({ window, source, limit: PAGE, offset });

  // Filter changes restart paging (and collapse any open breakdown).
  const changeWindow = (w: LeaderboardWindow) => {
    setWindow(w);
    setOffset(0);
    setExpanded(null);
  };
  const changeSource = (s: string) => {
    setSource(s);
    setOffset(0);
    setExpanded(null);
  };

  const page = boardQ.data;
  const entries = page?.entries ?? [];
  const isYouEntry = (e: LeaderboardEntry) =>
    connected !== null && e.wallets.some((w) => w.toLowerCase() === connected.toLowerCase());
  const searchIsConnected =
    searchAddress !== null &&
    connected !== null &&
    searchAddress.toLowerCase() === connected.toLowerCase();

  return (
    <div data-theme="aqua" style={{ position: "relative", minHeight: "100%" }}>
      <div className="app__wrap">
        <div className="dash-hero">
          <div className="dash-hero__eyebrow">points · ranked by on-chain activity</div>
          <h1 className="dash-hero__title">Leaderboard</h1>
          <div className="dash-hero__addr">
            {page
              ? `${page.total_accounts.toLocaleString("en-US")} accounts · as of ${new Date(page.as_of_ms).toLocaleTimeString()}`
              : "earn points for protocol activity — ranked across every linked identity"}
          </div>
        </div>

        <div className="lb-controls">
          <RangePills window={window} setWindow={changeWindow} />
          <SourceSelect
            sources={sourcesQ.data?.sources ?? []}
            source={source}
            setSource={changeSource}
          />
          <SearchBox
            committed={searchAddress}
            onCommit={(addr) => setSearchAddress(addr)}
            onClear={() => setSearchAddress(null)}
          />
        </div>

        {/* Pinned position cards: the searched wallet (when it isn't the
            connected one) and the connected wallet, always. */}
        {searchAddress !== null && !searchIsConnected && (
          <PositionCard
            address={searchAddress}
            isYou={false}
            window={window}
            onGoToPage={(o) => {
              setOffset(o);
              setExpanded(null);
            }}
          />
        )}
        {connected !== null ? (
          <PositionCard
            address={connected}
            isYou
            window={window}
            onGoToPage={(o) => {
              setOffset(o);
              setExpanded(null);
            }}
          />
        ) : (
          searchAddress === null && (
            <div className="lb-hint">
              Connect a wallet to pin your position — or search any wallet above.
            </div>
          )
        )}

        {boardQ.isLoading ? (
          <div className="lb-loading">
            <WaveLoader />
          </div>
        ) : boardQ.error && !page ? (
          <div className="dash-alert" role="alert">
            {boardQ.error instanceof LeaderboardUnavailableError
              ? "The leaderboard is temporarily unavailable — try again in a bit."
              : `Couldn't load the leaderboard: ${boardQ.error.message}`}
          </div>
        ) : entries.length === 0 ? (
          <div className="dash-empty">
            <div className="dash-empty__title">no points yet.</div>
            <div className="dash-empty__sub">
              No account has earned points for this window and source yet.
            </div>
          </div>
        ) : (
          <div className="panel">
            <div className="panel__head">Rankings</div>
            <div
              className="vault-table vault-table--cards"
              style={{ opacity: boardQ.isPlaceholderData ? 0.6 : undefined }}
            >
              <div className="vault-table__head" style={GRID}>
                <span>Rank</span>
                <span>Account</span>
                <span>Points</span>
                <span>Events</span>
                <span />
              </div>
              {entries.map((e) => (
                <Fragment key={e.account_id}>
                  <LeaderboardRow
                    entry={e}
                    isYou={isYouEntry(e)}
                    expanded={expanded === e.account_id}
                    onToggle={() =>
                      setExpanded((cur) => (cur === e.account_id ? null : e.account_id))
                    }
                  />
                  {expanded === e.account_id && (
                    <BreakdownPanel wallet={e.wallets[0] ?? null} window={window} />
                  )}
                </Fragment>
              ))}
            </div>
            <Pager offset={offset} total={page?.total_accounts ?? 0} onOffset={setOffset} />
          </div>
        )}
      </div>
    </div>
  );
}
