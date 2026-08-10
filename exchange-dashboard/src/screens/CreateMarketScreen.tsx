import { useMemo, useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";

import { EXCHANGE_PACKAGE_ID, explorerObjectUrl, explorerTxUrl } from "../config";
import { canonicalizeType, shortId } from "../format";
import { useAdminCap } from "../hooks/useAdminCap";
import { useCoinMetadataMap } from "../hooks/useCoinMetadata";
import { useSuiGrpcClient } from "../lib/suiGrpc";
import { buildCreateMarketTx, resolveRegistryId } from "../tx/createMarket";
import { useSubmitTransaction } from "../tx/submit";

// Mirror of registry.move's MAX_FEE_BPS — create_market aborts above this.
const MAX_FEE_BPS = 50n;

type Created = {
  digest: string;
  registryId: string;
  snippet: string;
};

function parseU64(s: string): bigint | null {
  if (!/^\d+$/.test(s.trim())) return null;
  const v = BigInt(s.trim());
  return v <= 0xffff_ffff_ffff_ffffn ? v : null;
}

export function CreateMarketScreen() {
  const account = useCurrentAccount();
  const client = useSuiGrpcClient();
  const submit = useSubmitTransaction();
  const adminCapQuery = useAdminCap();
  const adminCapId = adminCapQuery.data ?? null;

  const [symbol, setSymbol] = useState("");
  const [base, setBase] = useState("");
  const [quote, setQuote] = useState("");
  const [tickSize, setTickSize] = useState("");
  const [minSize, setMinSize] = useState("");
  const [lotSize, setLotSize] = useState("");
  const [feeBps, setFeeBps] = useState("0");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [created, setCreated] = useState<Created | null>(null);

  const baseCanon = canonicalizeType(base);
  const quoteCanon = canonicalizeType(quote);
  const metaTypes = useMemo(
    () => [baseCanon, quoteCanon].filter((t): t is string => !!t),
    [baseCanon, quoteCanon],
  );
  const metaMap = useCoinMetadataMap(metaTypes).data ?? {};

  const tick = parseU64(tickSize);
  const min = parseU64(minSize);
  const lot = parseU64(lotSize);
  const fee = parseU64(feeBps);

  // Client-side mirror of registry.move's create_market guards, so failures
  // are pre-empted with friendly messages instead of Move aborts.
  const fieldErrors: Record<string, string | null> = {
    symbol: symbol && !/^[A-Z0-9_\-/]+$/i.test(symbol) ? "letters/digits only" : null,
    base: base && !baseCanon ? "expected 0x…::module::Name" : null,
    quote:
      quote && !quoteCanon
        ? "expected 0x…::module::Name"
        : baseCanon && quoteCanon && baseCanon === quoteCanon
          ? "base and quote must differ"
          : null,
    tickSize: tickSize && (tick === null || tick === 0n) ? "must be a positive integer" : null,
    minSize: minSize && (min === null || min === 0n) ? "must be a positive integer" : null,
    lotSize: lotSize && (lot === null || lot === 0n) ? "must be a positive integer" : null,
    feeBps:
      feeBps && (fee === null || fee > MAX_FEE_BPS) ? `0–${MAX_FEE_BPS} (on-chain ceiling)` : null,
  };

  const complete =
    !!symbol && !!baseCanon && !!quoteCanon && !!tick && !!min && !!lot && fee !== null;
  const valid = complete && Object.values(fieldErrors).every((e) => !e);
  const canSubmit = valid && !!account && !!adminCapId && !!EXCHANGE_PACKAGE_ID && !busy;

  async function onCreate() {
    if (!canSubmit || !baseCanon || !quoteCanon) return;
    setBusy(true);
    setError(null);
    setCreated(null);
    try {
      const tx = buildCreateMarketTx({
        packageId: EXCHANGE_PACKAGE_ID!,
        adminCapId: adminCapId!,
        base: baseCanon,
        quote: quoteCanon,
        tickSize: tick!,
        minSize: min!,
        feeBps: fee!,
      });
      const digest = await submit(tx);
      const registryId = await resolveRegistryId(client, digest);
      const snippet = JSON.stringify(
        {
          [symbol.toUpperCase()]: {
            registryId,
            base: baseCanon,
            quote: quoteCanon,
            tickSize: Number(tick),
            minSize: Number(min),
            lotSize: Number(lot),
            feeBps: Number(fee),
          },
        },
        null,
        2,
      )
        // strip the outer braces so it pastes directly into the markets map
        .replace(/^\{\n/, "")
        .replace(/\n\}$/, "")
        .replace(/^ {2}/gm, "");
      setCreated({ digest, registryId, snippet });
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  const metaLine = (canon: string | null) => {
    if (!canon) return null;
    const meta = metaMap[canon];
    return (
      <div className="hint">
        {meta
          ? `${meta.symbol} — ${meta.decimals} decimals`
          : "no on-chain coin metadata found for this type"}
      </div>
    );
  };

  return (
    <div className="card wide">
      <h1>Create Market</h1>

      {!EXCHANGE_PACKAGE_ID ? (
        <div className="banner warn">
          VITE_EXCHANGE_PACKAGE_ID is not set — no exchange deployment configured.
        </div>
      ) : !account ? (
        <div className="banner info">Connect the wallet that holds the exchange AdminCap.</div>
      ) : adminCapQuery.isLoading ? (
        <div className="banner info">Checking for the exchange AdminCap…</div>
      ) : !adminCapId ? (
        <div className="banner warn">
          The connected wallet does not hold the exchange AdminCap — market creation is
          admin-gated (contracts/exchange admin::AdminCap, transferred to the deployer at
          publish).
        </div>
      ) : (
        <div className="banner info">
          AdminCap <span className="mono">{shortId(adminCapId)}</span> found — this wallet can
          create markets.
        </div>
      )}

      <div className="field">
        <label>Symbol (off-chain market key, e.g. TBTC/TUSDC)</label>
        <input value={symbol} onChange={(e) => setSymbol(e.target.value)} placeholder="BASE/QUOTE" />
        {fieldErrors.symbol && <div className="error">{fieldErrors.symbol}</div>}
      </div>

      <div className="field">
        <label>Base coin type</label>
        <input
          className="mono"
          value={base}
          onChange={(e) => setBase(e.target.value)}
          placeholder="0x…::tbtc::TBTC"
        />
        {fieldErrors.base && <div className="error">{fieldErrors.base}</div>}
        {metaLine(baseCanon)}
      </div>

      <div className="field">
        <label>Quote coin type</label>
        <input
          className="mono"
          value={quote}
          onChange={(e) => setQuote(e.target.value)}
          placeholder="0x…::tusdc::TUSDC"
        />
        {fieldErrors.quote && <div className="error">{fieldErrors.quote}</div>}
        {metaLine(quoteCanon)}
      </div>

      <div className="field">
        <label>Tick size (quote atomic units per price step — on-chain)</label>
        <input className="mono" value={tickSize} onChange={(e) => setTickSize(e.target.value)} placeholder="1000" />
        {fieldErrors.tickSize && <div className="error">{fieldErrors.tickSize}</div>}
      </div>

      <div className="field">
        <label>Min size (base atomic units — on-chain)</label>
        <input className="mono" value={minSize} onChange={(e) => setMinSize(e.target.value)} placeholder="100000" />
        {fieldErrors.minSize && <div className="error">{fieldErrors.minSize}</div>}
      </div>

      <div className="field">
        <label>Lot size (base atomic units — off-chain only, used by the matching engine)</label>
        <input className="mono" value={lotSize} onChange={(e) => setLotSize(e.target.value)} placeholder="1000" />
        {fieldErrors.lotSize && <div className="error">{fieldErrors.lotSize}</div>}
      </div>

      <div className="field">
        <label>Fee (bps, 0–50)</label>
        <input className="mono" value={feeBps} onChange={(e) => setFeeBps(e.target.value)} />
        {fieldErrors.feeBps && <div className="error">{fieldErrors.feeBps}</div>}
        <div className="hint">All sizes are atomic units (respecting each coin's decimals), not whole tokens.</div>
      </div>

      <button className="primary" disabled={!canSubmit} onClick={onCreate}>
        {busy ? "Creating…" : "Create market"}
      </button>

      {error && (
        <div className="banner error" style={{ marginTop: 12 }}>
          {error}
        </div>
      )}

      {created && (
        <div style={{ marginTop: 16 }}>
          <div className="banner success">
            Market created —{" "}
            <a href={explorerObjectUrl(created.registryId)} target="_blank" rel="noreferrer">
              registry {shortId(created.registryId)}
            </a>{" "}
            (
            <a href={explorerTxUrl(created.digest)} target="_blank" rel="noreferrer">
              tx
            </a>
            )
          </div>
          <p className="dim" style={{ fontSize: 13 }}>
            The orderbook service loads its market list from{" "}
            <span className="mono">rust-backend/deployments.json</span> at boot. To activate this
            market, paste the snippet under{" "}
            <span className="mono">&lt;env&gt;.package_info.exchange.markets</span> and redeploy
            the orderbook service:
          </p>
          <div className="snippet">{created.snippet}</div>
          <button
            className="copy-btn"
            onClick={() => void navigator.clipboard.writeText(created.snippet)}
          >
            Copy snippet
          </button>
        </div>
      )}
    </div>
  );
}
