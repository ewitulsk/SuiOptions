// Standalone position page (SO-418), route
// `/vaults/:vaultId/positions/:positionId`: renders ANY `VaultPosition` by
// id — positions are freely transferable, so this is the due-diligence page
// a prospective secondary buyer opens before purchase. Same card as the
// wallet panel, plus the vault context and the value-vs-basis disclosure
// the contract plan requires the UI to show before a sale (§2.4).

import { Link, useParams } from "react-router-dom";

import { tokenForCoinType } from "../api/tradingVaults";
import { usePositionDetail, useTradingVault } from "../api/useTradingVaults";
import { Address } from "../components/Address";
import { VaultPositionCard } from "../components/VaultPositionCard";
import { TRADING_VAULT_PACKAGE_ID } from "../config";
import { formatPrice } from "../format";
import { VaultStateBadges, shortHex } from "./TradingVaults";

export function VaultPositionDetailScreen() {
  const { vaultId, positionId } = useParams<{ vaultId: string; positionId: string }>();
  const positionQ = usePositionDetail(positionId ?? null);
  const vaultQ = useTradingVault(vaultId ?? null);

  if (!TRADING_VAULT_PACKAGE_ID) {
    return (
      <div style={{ position: "relative", minHeight: "100%" }}>
        <div className="app__wrap">
          <div className="dash-empty">
            <div className="dash-empty__title">trading vaults unavailable.</div>
            <div className="dash-empty__sub">
              No trading-vault deployment exists on this network.
            </div>
          </div>
        </div>
      </div>
    );
  }

  const vault = vaultQ.data ?? null;
  const position = positionQ.data ?? null;
  const token = vault != null ? tokenForCoinType(vault.accountingAsset) : null;
  const symbol = vault != null ? token?.ticker ?? shortHex(vault.accountingAsset) : "";
  const decimals = token?.decimals ?? null;
  const wrongVault =
    position != null && vaultId != null && position.vaultId !== vaultId ? position.vaultId : null;

  return (
    <div style={{ position: "relative", minHeight: "100%" }}>
      <div className="app__wrap">
        <div className="vault-detail__bar">
          <Link className="vault-back" to={`/vaults/${vaultId ?? ""}`}>
            ← Vault
          </Link>
        </div>
        <div className="vault-browser__head">
          <span className="vault-head__badge">Vault position</span>
          <span className="vault-head__tag">
            A transferable claim NFT on a curated trading vault — verify its
            economics before buying it second-hand.
          </span>
        </div>

        {positionQ.isLoading && <div className="vault-note">Loading position…</div>}
        {positionQ.isError && (
          <div className="dash-alert" role="alert">
            {positionQ.error.message}
          </div>
        )}
        {wrongVault != null && (
          <div className="dash-alert" role="alert">
            This position belongs to a different vault:{" "}
            <Link to={`/vaults/${wrongVault}/positions/${position?.positionId}`}>
              view it there
            </Link>
            .
          </div>
        )}

        {position != null && (
          <div className="vault-grid">
            <div className="vault-grid__main">
              <VaultPositionCard position={position} symbol={symbol} decimals={decimals}>
                <div className="vault-kv" style={{ marginTop: 8 }}>
                  <div className="vault-kv__row">
                    <span>Current owner</span>
                    {position.owner != null ? (
                      <Address value={position.owner} label="Owner" />
                    ) : (
                      <span>not wallet-held</span>
                    )}
                  </div>
                  <div className="vault-kv__row">
                    <span>Vault</span>
                    <Link to={`/vaults/${position.vaultId}`}>
                      {shortHex(position.vaultId)}
                    </Link>
                  </div>
                </div>
              </VaultPositionCard>

              {/* §2.4 pre-purchase disclosure: value vs basis, in the open. */}
              <div className="dash-alert" role="note">
                <strong>Buyer beware:</strong> performance fees crystallize on
                exit against THIS position's recorded cost basis — a buyer{" "}
                <strong>inherits the embedded fee liability</strong> shown
                above; paying a market price for the NFT does not reset its
                on-chain basis. Junior positions of a wiped generation are
                permanently worthless.{" "}
                <a
                  href="https://github.com/ewitulsk/SuiOptions/blob/staging/docs/trading-vault-v2/disclosures.md"
                  target="_blank"
                  rel="noreferrer"
                >
                  Full terms &amp; risk disclosures
                </a>
                {vault != null && <> (terms v{vault.termsVersion})</>}.
              </div>
            </div>
            <div className="vault-grid__side">
              {vault != null && (
                <div className="vault-card">
                  <div className="vault-card__head">Vault context</div>
                  <div className="vault-kv">
                    <div className="vault-kv__row">
                      <span>State</span>
                      <VaultStateBadges vault={vault} />
                    </div>
                    <div className="vault-kv__row">
                      <span>Accounting asset</span>
                      <span>{symbol}</span>
                    </div>
                    <div className="vault-kv__row">
                      <span>Curator fee</span>
                      <span>{(vault.curatorFeeBps / 100).toFixed(2)}% of profit</span>
                    </div>
                    {vault.capitalStructure != null && (
                      <>
                        <div className="vault-kv__row">
                          <span>Structure</span>
                          <span>senior / junior</span>
                        </div>
                        <div className="vault-kv__row">
                          <span>Junior buffer</span>
                          <span>
                            {vault.juniorBufferBps != null
                              ? `${(vault.juniorBufferBps / 100).toFixed(2)}%`
                              : "—"}
                          </span>
                        </div>
                        <div className="vault-kv__row">
                          <span>Active junior generation</span>
                          <span>{vault.activeJuniorGeneration}</span>
                        </div>
                      </>
                    )}
                    {position.estimatedValueRaw != null && decimals != null && (
                      <div className="vault-kv__row">
                        <span>Est. position value</span>
                        <span>
                          {formatPrice(Number(position.estimatedValueRaw) / 10 ** decimals, {
                            grouping: true,
                          })}{" "}
                          {symbol}
                        </span>
                      </div>
                    )}
                  </div>
                  <div className="vault-card__foot vault-prose__muted">
                    Estimates use the latest capital sync — final value
                    crystallizes only at exit.
                  </div>
                </div>
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
