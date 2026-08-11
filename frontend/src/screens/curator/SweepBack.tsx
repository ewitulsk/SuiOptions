// In-app sweep-back (SO-305), replacing the copy-paste recipe: (i) co-sign
// the Bluefin withdraw payload (funds can only land at the parent address),
// then (ii) co-sign a parent-address return_external Sui tx paying the
// vault. Both legs run through the FROST ceremony; the second reduces the
// vault's outstanding exposure.

import { useState } from "react";
import { useCurrentAccount, useSignPersonalMessage } from "@mysten/dapp-kit";

import type { TradingVaultDetail } from "../../api/tradingVaults";
import { tokenForCoinType } from "../../api/tradingVaults";
import {
  bluefinLogin,
  fetchBluefinExchangeInfo,
  loginPayload,
  submitWithdraw,
  toE9,
  withdrawPayload,
} from "../../api/bluefin";
import { Address } from "../../components/Address";
import { runSignCeremony } from "../../frost/ceremony";
import { useSuiGrpcClient } from "../../lib/suiGrpc";
import { buildParentSweepTxBytes, executeParentTx } from "../../tx/bluefinParent";
import type { UnlockedShare } from "../../state/curatorBluefin";
import { CeremonyStatus, useCeremony } from "./ceremonyUi";

export function SweepBack({
  vault,
  symbol,
  decimals,
  share,
}: {
  vault: TradingVaultDetail;
  symbol: string;
  decimals: number | null;
  share: UnlockedShare;
}) {
  const client = useSuiGrpcClient();
  const account = useCurrentAccount();
  const { mutateAsync: signPersonalMessage } = useSignPersonalMessage();
  const { state, run, busy } = useCeremony();
  const [amount, setAmount] = useState("");
  const [digest, setDigest] = useState<string | null>(null);

  const assetSymbol = (tokenForCoinType(vault.accountingAsset)?.ticker ?? "USDC").replace(/^t/, "");

  const onSweep = async () => {
    if (decimals == null || !account) return;
    const amountNum = Number(amount) || 0;
    if (amountNum <= 0) return;
    const amountRaw = BigInt(Math.round(amountNum * 10 ** decimals));

    const d = await run(async (onProgress) => {
      onProgress("Reading Bluefin contract config…");
      const info = await fetchBluefinExchangeInfo();

      // Login (curator wallet) → JWT to submit the withdraw.
      onProgress("Signing in to Bluefin…");
      const login = loginPayload(account.address);
      const loginSig = await signPersonalMessage({ message: login.bytes });
      const tokens = await bluefinLogin(login.json, loginSig.signature);

      // Withdraw: parent co-signs (funds forced to the parent address).
      onProgress("Co-signing Bluefin withdraw…");
      const w = withdrawPayload({
        edsId: info.contractsConfig.edsId,
        assetSymbol,
        parentAddress: share.parentAddress,
        amountE9: toE9(amountNum),
      });
      const wSig = await runSignCeremony({
        vaultId: vault.vaultId,
        payloadKind: "withdraw",
        payloadBytes: w.bytes,
        keyPackageB64: share.keyPackageB64,
        publicKeyPackageB64: share.publicKeyPackageB64,
        groupPublicKeyHex: share.groupPublicKeyHex,
        onProgress,
      });
      onProgress("Submitting withdraw…");
      await submitWithdraw(tokens.accessToken, {
        assetSymbol,
        accountAddress: share.parentAddress,
        amountE9: toE9(amountNum),
        edsId: info.contractsConfig.edsId,
        salt: w.salt,
        signedAtMillis: w.signedAtMillis,
        signatureB64: wSig.suiSignatureB64,
      });

      // Sweep: parent co-signs return_external paying the vault.
      onProgress("Building sweep transaction…");
      const txBytes = await buildParentSweepTxBytes({
        client,
        parentAddress: share.parentAddress,
        vaultId: vault.vaultId,
        depositCoinType: vault.accountingAsset,
        amountRaw,
      });
      const sweepSig = await runSignCeremony({
        vaultId: vault.vaultId,
        payloadKind: "sui_tx",
        payloadBytes: txBytes,
        keyPackageB64: share.keyPackageB64,
        publicKeyPackageB64: share.publicKeyPackageB64,
        groupPublicKeyHex: share.groupPublicKeyHex,
        onProgress,
      });
      onProgress("Executing sweep to vault…");
      return executeParentTx(client, txBytes, sweepSig.suiSignatureB64);
    }, "Swept back to the vault — exposure reduced.");
    if (d) setDigest(d);
  };

  const exposureNum =
    decimals != null ? Number(vault.externalExposure) / 10 ** decimals : null;

  return (
    <div style={{ marginTop: 12 }}>
      <div className="vault-card__head" style={{ fontSize: 13 }}>
        Sweep back to vault
      </div>
      <div className="vault-prose__muted" style={{ fontSize: 12, marginBottom: 8 }}>
        Withdraw {symbol} from Bluefin (funds can only return to the parent
        address) and return it to the vault. Both legs are co-signed.
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
      <div className="vault-invest__bal">
        outstanding exposure {exposureNum != null ? exposureNum.toLocaleString() : vault.externalExposure} {symbol}
      </div>
      <button className="vault-invest__cta" disabled={busy || decimals == null || !account} onClick={onSweep}>
        {busy ? "Sweeping…" : `Withdraw + return ${symbol}`}
      </button>
      <CeremonyStatus state={state} />
      {digest && (
        <div className="vault-prose__muted" style={{ fontSize: 11, marginTop: 6 }}>
          Sweep digest <Address value={digest} label="Sweep digest" />
        </div>
      )}
    </div>
  );
}
