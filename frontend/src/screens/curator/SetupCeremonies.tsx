// The two ceremony-signed setup steps (SO-305), split out of SetupWizard:
//   d. deposit  — co-sign a parent-address deposit_to_asset_bank Sui tx
//   e. authorize — co-sign the Bluefin login, then authorize_account of the
//      curator's trading wallet (after which trading needs no ceremonies).

import { useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";

import type { TradingVaultDetail } from "../../api/tradingVaults";
import { tokenForCoinType } from "../../api/tradingVaults";
import {
  authorizePayload,
  bluefinLogin,
  fetchBluefinExchangeInfo,
  loginPayload,
  submitAuthorize,
  type BluefinExchangeInfo,
} from "../../api/bluefin";
import { runSignCeremony } from "../../frost/ceremony";
import { useSuiGrpcClient } from "../../lib/suiGrpc";
import {
  buildParentDepositTxBytes,
  executeParentTx,
} from "../../tx/bluefinParent";
import type { UnlockedShare } from "../../state/curatorBluefin";
import { CeremonyStatus, useCeremony } from "./ceremonyUi";
import { curatorFieldStyle } from "./styles";

/** The venue collateral asset for a vault: matched from Bluefin's asset
 * list by the vault's deposit-asset symbol. */
function venueAsset(info: BluefinExchangeInfo, vault: TradingVaultDetail) {
  const token = tokenForCoinType(vault.depositAsset);
  const symbol = token?.ticker?.replace(/^t/, "") ?? "USDC"; // TUSDC → USDC
  return info.assets.find((a) => a.symbol === symbol) ?? info.assets[0] ?? null;
}

export function DepositStep({
  vault,
  symbol,
  decimals,
  share,
  onDeposited,
}: {
  vault: TradingVaultDetail;
  symbol: string;
  decimals: number | null;
  share: UnlockedShare;
  onDeposited: () => void;
}) {
  const client = useSuiGrpcClient();
  const { state, run, busy } = useCeremony();
  const [amount, setAmount] = useState("");
  const [digest, setDigest] = useState<string | null>(null);

  const onDeposit = async () => {
    if (decimals == null) return;
    const amountRaw = BigInt(Math.round((Number(amount) || 0) * 10 ** decimals));
    if (amountRaw <= 0n) return;
    const d = await run(async (onProgress) => {
      onProgress("Reading Bluefin contract config…");
      const info = await fetchBluefinExchangeInfo();
      const asset = venueAsset(info, vault);
      if (!asset) throw new Error("no Bluefin collateral asset for this vault");
      onProgress("Building deposit transaction…");
      const txBytes = await buildParentDepositTxBytes({
        client,
        parentAddress: share.parentAddress,
        bluefinPackageId: info.contractsConfig.currentContractAddress,
        edsId: info.contractsConfig.edsId,
        assetSymbol: asset.symbol,
        coinType: asset.assetType,
        amountRaw,
      });
      const sig = await runSignCeremony({
        vaultId: vault.vaultId,
        payloadKind: "sui_tx",
        payloadBytes: txBytes,
        keyPackageB64: share.keyPackageB64,
        publicKeyPackageB64: share.publicKeyPackageB64,
        groupPublicKeyHex: share.groupPublicKeyHex,
        onProgress,
      });
      onProgress("Executing deposit…");
      return executeParentTx(client, txBytes, sig.suiSignatureB64);
    }, "Deposit submitted.");
    if (d) setDigest(d);
  };

  return (
    <div>
      <div className="vault-prose__muted" style={{ fontSize: 12, marginBottom: 8 }}>
        Deposit the parent-held {symbol} into Bluefin's AssetBank. This is a
        Sui transaction FROM the parent account, co-signed by you and the
        protocol signer. It materializes the Bluefin account.
      </div>
      <div className="vault-invest__field" style={{ marginBottom: 8 }}>
        <input
          className="amount__input"
          type="number"
          min="0"
          placeholder="0.0"
          value={amount}
          onChange={(e) => setAmount(e.target.value)}
        />
        <span className="vault-invest__unit">{symbol}</span>
      </div>
      <button className="vault-invest__cta" disabled={busy || decimals == null} onClick={onDeposit}>
        {busy ? "Depositing…" : `Deposit ${symbol} into Bluefin`}
      </button>
      <CeremonyStatus state={state} />
      {digest && (
        <button className="vault-invest__cta" style={{ marginTop: 8 }} onClick={onDeposited}>
          Continue to authorize trading
        </button>
      )}
    </div>
  );
}

export function AuthorizeStep({
  vault,
  share,
  onAuthorized,
}: {
  vault: TradingVaultDetail;
  share: UnlockedShare;
  onAuthorized: () => void;
}) {
  const account = useCurrentAccount();
  const { state, run, busy } = useCeremony();
  const [curatorWallet, setCuratorWallet] = useState(account?.address ?? "");
  const [done, setDone] = useState(false);

  const onAuthorize = async () => {
    if (!curatorWallet) return;
    const ok = await run(async (onProgress) => {
      onProgress("Reading Bluefin contract config…");
      const info = await fetchBluefinExchangeInfo();

      // Login: the parent co-signs its own JWT auth payload.
      onProgress("Co-signing Bluefin login…");
      const login = loginPayload(share.parentAddress);
      const loginSig = await runSignCeremony({
        vaultId: vault.vaultId,
        payloadKind: "login",
        payloadBytes: login.bytes,
        keyPackageB64: share.keyPackageB64,
        publicKeyPackageB64: share.publicKeyPackageB64,
        groupPublicKeyHex: share.groupPublicKeyHex,
        onProgress,
      });
      const tokens = await bluefinLogin(login.json, loginSig.suiSignatureB64);

      // Authorize: the parent co-signs an authorize_account of the curator
      // wallet (hedge-signer policy admits only the configured curator).
      onProgress("Co-signing authorize_account…");
      const auth = authorizePayload({
        idsId: info.contractsConfig.idsId,
        parentAddress: share.parentAddress,
        userAddress: curatorWallet,
        authorize: true,
      });
      const authSig = await runSignCeremony({
        vaultId: vault.vaultId,
        payloadKind: "authorize_account",
        payloadBytes: auth.bytes,
        keyPackageB64: share.keyPackageB64,
        publicKeyPackageB64: share.publicKeyPackageB64,
        groupPublicKeyHex: share.groupPublicKeyHex,
        onProgress,
      });
      onProgress("Submitting authorization…");
      await submitAuthorize(tokens.accessToken, {
        accountAddress: share.parentAddress,
        authorizedAccountAddress: curatorWallet,
        idsId: info.contractsConfig.idsId,
        salt: auth.salt,
        signedAtMillis: auth.signedAtMillis,
        signatureB64: authSig.suiSignatureB64,
      });
      return true;
    }, "Curator wallet authorized. Trading needs no more ceremonies.");
    if (ok) setDone(true);
  };

  return (
    <div>
      <div className="vault-prose__muted" style={{ fontSize: 12, marginBottom: 8 }}>
        Authorize your day-to-day wallet as the parent account's trader. After
        this, placing and cancelling orders uses only your wallet — no
        ceremonies. Only the vault's configured curator wallet is accepted.
      </div>
      <label style={{ fontSize: 11, opacity: 0.8, display: "block", marginBottom: 8 }}>
        Curator trading wallet
        <input
          style={curatorFieldStyle}
          value={curatorWallet}
          onChange={(e) => setCuratorWallet(e.target.value)}
          placeholder="0x…"
        />
      </label>
      <button className="vault-invest__cta" disabled={busy || !curatorWallet} onClick={onAuthorize}>
        {busy ? "Authorizing…" : "Authorize trading wallet"}
      </button>
      <CeremonyStatus state={state} />
      {done && (
        <button className="vault-invest__cta" style={{ marginTop: 8 }} onClick={onAuthorized}>
          Finish setup
        </button>
      )}
    </div>
  );
}
