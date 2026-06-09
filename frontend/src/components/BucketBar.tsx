import { useEffect, useRef, useState } from "react";
import { usePythPrice } from "../api/usePythPrice";
import type { AssetOption, ExpiryOption } from "../state/composer";

type Props = {
  symbol: string | null | undefined;
  capPct: number;
  assets: AssetOption[];
  selectedAsset: string | null;
  onSelectAsset: (symbol: string) => void;
  expiries: ExpiryOption[];
  selectedExpiryMs: number | null;
  onSelectExpiry: (ms: number) => void;
  settlementSymbol: string;
};

export function BucketBar({
  symbol,
  capPct,
  assets,
  selectedAsset,
  onSelectAsset,
  expiries,
  selectedExpiryMs,
  onSelectExpiry,
  settlementSymbol,
}: Props) {
  const r = 12;
  const c = 2 * Math.PI * r;
  const live = usePythPrice(symbol);
  const priceLabel = live
    ? `$${live.price.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`
    : "—";

  const assetLabel = selectedAsset ?? "—";
  const expiryLabel =
    selectedExpiryMs !== null ? formatExpiryShort(selectedExpiryMs) : "—";

  return (
    <div className="bbar">
      <PickerCell
        title="Asset"
        label={assetLabel}
        icon={<AssetIcon symbol={selectedAsset} />}
        disabled={assets.length === 0}
        renderMenu={(close) => (
          <ul className="bbar-menu__list" role="listbox" aria-label="Asset">
            {assets.map((a) => {
              const active = a.symbol === selectedAsset;
              return (
                <li key={a.symbol}>
                  <button
                    type="button"
                    className={"bbar-menu__item" + (active ? " is-active" : "")}
                    role="option"
                    aria-selected={active}
                    onClick={() => {
                      onSelectAsset(a.symbol);
                      close();
                    }}
                  >
                    <span className="bbar-menu__item-label">{a.symbol}</span>
                    {active && (
                      <span className="bbar-menu__item-check" aria-hidden>
                        ✓
                      </span>
                    )}
                  </button>
                </li>
              );
            })}
          </ul>
        )}
      />

      <div className="bbar__sel">
        <span className="bbar__sel-label">Covered call</span>
      </div>

      <PickerCell
        title="Expiry"
        label={expiryLabel}
        labelMono
        disabled={expiries.length === 0}
        renderMenu={(close) => (
          <ul className="bbar-menu__list" role="listbox" aria-label="Expiry">
            {expiries.map((e) => {
              const active = e.ms === selectedExpiryMs;
              return (
                <li key={e.ms}>
                  <button
                    type="button"
                    className={"bbar-menu__item" + (active ? " is-active" : "")}
                    role="option"
                    aria-selected={active}
                    onClick={() => {
                      onSelectExpiry(e.ms);
                      close();
                    }}
                  >
                    <span className="bbar-menu__item-label bbar__sel-label--mono">
                      {formatExpiryShort(e.ms)}
                    </span>
                    <span className="bbar-menu__item-sub">
                      {formatExpiryFull(e.ms)}
                    </span>
                    {active && (
                      <span className="bbar-menu__item-check" aria-hidden>
                        ✓
                      </span>
                    )}
                  </button>
                </li>
              );
            })}
          </ul>
        )}
      />

      <div className="bbar__sel" style={{ borderRight: "none" }}>
        <span className="bbar__sel-label--mono" style={{ fontSize: 10, color: "var(--aqua-ink-3)" }}>
          settled in
        </span>
        <span className="bbar__sel-label">{settlementSymbol}</span>
      </div>
      <div className="bbar__spacer"></div>
      <div className="bbar__price">
        <div>
          <div className="bbar__price-val">{priceLabel}</div>
          <div className="bbar__price-tick">{live ? "spot live · pyth" : "connecting…"}</div>
        </div>
        <div className="bbar__cap">
          <div className="bbar__cap-ring">
            <svg width="32" height="32" viewBox="0 0 32 32">
              <circle cx="16" cy="16" r={r} fill="none" stroke="rgba(11,37,69,0.10)" strokeWidth="3" />
              <circle
                cx="16"
                cy="16"
                r={r}
                fill="none"
                stroke="var(--aqua-sui)"
                strokeWidth="3"
                strokeDasharray={`${(c * capPct) / 100} ${c}`}
                strokeLinecap="round"
              />
            </svg>
          </div>
          <div className="bbar__cap-text">
            <b>{capPct}%</b>
            <br />
            of cap
          </div>
        </div>
      </div>
    </div>
  );
}

type PickerCellProps = {
  title: string;
  label: string;
  labelMono?: boolean;
  icon?: React.ReactNode;
  disabled?: boolean;
  renderMenu: (close: () => void) => React.ReactNode;
};

function PickerCell({ title, label, labelMono, icon, disabled, renderMenu }: PickerCellProps) {
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onClick = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    window.addEventListener("mousedown", onClick);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onClick);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div className="bbar__sel bbar__sel--picker" ref={wrapRef}>
      <button
        type="button"
        className="bbar__sel-btn"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={title}
        disabled={disabled}
        onClick={() => setOpen((o) => !o)}
      >
        {icon}
        <span className={"bbar__sel-label" + (labelMono ? " bbar__sel-label--mono" : "")}>
          {label}
        </span>
        <span className="bbar__caret" aria-hidden>▾</span>
      </button>
      {open && <div className="bbar-menu" role="presentation">{renderMenu(() => setOpen(false))}</div>}
    </div>
  );
}

function AssetIcon({ symbol }: { symbol: string | null }) {
  if (!symbol) return null;
  const isBtc = /btc/i.test(symbol);
  const initial = symbol.replace(/[^A-Za-z0-9]/g, "").charAt(0).toUpperCase() || "?";
  return (
    <span className={"bbar__sel-icon" + (isBtc ? "" : " bbar__sel-icon--generic")}>
      {isBtc ? "₿" : initial}
    </span>
  );
}

function formatExpiryShort(ms: number): string {
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return "—";
  const month = d.toLocaleDateString("en-US", { month: "short" }).toUpperCase();
  const day = d.getDate();
  return `${month}_${day}`;
}

function formatExpiryFull(ms: number): string {
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    timeZoneName: "short",
  });
}
