// ⚠️ STAGING-ONLY TESTING CARD (SO-311) — see `src/bluefinTest.ts` for the
// provenance of the embedded account. Rendered only when
// `BLUEFIN_TEST_ENABLED` and the vault's deposit asset IS Bluefin's staging
// USDC, so it does not exist in mainnet or prod builds. ⚠️
//
// One click pulls test USDC out of Bluefin's published staging account into
// the curator's wallet, then runs the normal deposit into the vault — the
// only way to get a Bluefin-USDC vault funded without a faucet.

import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useCurrentAccount } from "@mysten/dapp-kit";
import { SUI_DECIMALS, SUI_TYPE_ARG } from "@mysten/sui/utils";

import type { TradingVaultDetail } from "../../api/tradingVaults";
import { useCoinBalance } from "../../api/useCoinBalance";
import {
  BLUEFIN_TEST_ACCOUNT,
  BLUEFIN_TEST_MAX_PULL,
  BLUEFIN_TEST_USDC,
  transferBluefinTestUsdc,
} from "../../bluefinTest";
import { formatPrice } from "../../format";
import { useSuiGrpcClient } from "../../lib/suiGrpc";
import { buildTradingVaultDepositTx } from "../../tx/tradingVault";
import { useSubmitTransaction } from "../../tx/submit";
import { CeremonyStatus, useCeremony } from "./ceremonyUi";

export function BluefinTestFunds({
  vault,
  cfgId,
}: {
  vault: TradingVaultDetail;
  cfgId: string | null;
}) {
  const account = useCurrentAccount();
  const address = account?.address ?? null;
  const client = useSuiGrpcClient();
  const submit = useSubmitTransaction();
  const qc = useQueryClient();
  const { state, run, busy } = useCeremony();
  const [amount, setAmount] = useState("10000");

  // The shared account's remaining funds, so exhaustion is visible before the
  // pull fails.
  const usdcQ = useCoinBalance(BLUEFIN_TEST_ACCOUNT, BLUEFIN_TEST_USDC.coinType);
  const gasQ = useCoinBalance(BLUEFIN_TEST_ACCOUNT, SUI_TYPE_ARG);

  const amountNum = Number(amount) || 0;
  const amountRaw = BigInt(Math.round(amountNum * 10 ** BLUEFIN_TEST_USDC.decimals));
  const accountRaw = usdcQ.data != null ? BigInt(usdcQ.data) : null;

  const blocked = !address
    ? "Connect a wallet — the funds land there before being deposited."
    : !cfgId
      ? "Protocol config unavailable for this deployment."
      : amountNum <= 0
        ? "Enter an amount to pull."
        : amountNum > BLUEFIN_TEST_MAX_PULL
          ? `Max ${formatPrice(BLUEFIN_TEST_MAX_PULL, { grouping: true })} per pull — the account is shared.`
          : accountRaw != null && amountRaw > accountRaw
            ? "The test account no longer holds that much USDC."
            : undefined;

  const onPull = () => {
    if (blocked || !address || !cfgId) return;
    void run(async (onProgress) => {
      onProgress(`Sending ${amount} USDC from the Bluefin test account…`);
      await transferBluefinTestUsdc(client, address, amountRaw);

      onProgress("Transfer confirmed — depositing into the vault…");
      const build = () =>
        buildTradingVaultDepositTx({
          vaultId: vault.vaultId,
          protocolConfigId: cfgId,
          depositCoinType: vault.depositAsset,
          amountRaw,
        });
      try {
        await submit(build());
      } catch (err) {
        // `useSubmitTransaction` deliberately never retries a failed
        // sponsorship wallet-paid. For this testing card it should just work,
        // so retry once — narrowly, so a user rejection isn't re-prompted.
        if (!/sponsorship failed/i.test(err instanceof Error ? err.message : String(err))) throw err;
        onProgress("Gas sponsorship unavailable — retrying wallet-paid…");
        await submit(build(), { sponsor: false });
      }

      qc.invalidateQueries({ queryKey: ["trading-vaults"] });
      qc.invalidateQueries({ queryKey: ["trading-vault"] });
      qc.invalidateQueries({ queryKey: ["coin-balance"] });
    }, `Pulled ${amount} USDC from the Bluefin test account and deposited it.`);
  };

  return (
    <div style={{ marginTop: 12 }}>
      <div className="vault-card__head" style={{ fontSize: 13 }}>
        Testing · Bluefin staging funds
      </div>
      <div className="vault-invest__field" style={{ marginTop: 8 }}>
        <input
          className="amount__input"
          type="number"
          min="0"
          max={BLUEFIN_TEST_MAX_PULL}
          value={amount}
          onChange={(e) => setAmount(e.target.value)}
        />
        <span className="vault-invest__unit">USDC</span>
      </div>
      <div className="vault-invest__bal">
        test account {BLUEFIN_TEST_ACCOUNT.slice(0, 8)}… holds{" "}
        {accountRaw != null
          ? `${formatPrice(Number(accountRaw) / 10 ** BLUEFIN_TEST_USDC.decimals, { grouping: true })} USDC`
          : "—"}{" "}
        ·{" "}
        {gasQ.data != null
          ? `${formatPrice(Number(gasQ.data) / 10 ** SUI_DECIMALS, { grouping: true })} SUI gas`
          : "— SUI gas"}
      </div>
      <button
        className="vault-invest__cta"
        disabled={busy || !!blocked}
        onClick={onPull}
        title={blocked}
      >
        {busy ? "Pulling…" : "Pull Bluefin test funds"}
      </button>
      <CeremonyStatus state={state} />
      <div className="vault-card__foot vault-prose__muted">
        Staging only. Moves test USDC from Bluefin's publicly published staging
        account (it pays its own gas) into your wallet, then deposits it here.
        Max {formatPrice(BLUEFIN_TEST_MAX_PULL, { grouping: true })} per pull —
        the account is shared with everyone testing against Bluefin staging.
        {blocked && ` ${blocked}`}
      </div>
    </div>
  );
}
