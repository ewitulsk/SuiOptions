// "Create trading venue" card (SO-154).
//
// Shown on the Buy screen when the selected bucket has no DeepBook pool yet.
// Anyone can create the venue, but the contract burns a fixed fee of
// 500 DEEP — the real DeepBook testnet coin — from the connected wallet, so
// the button gates on that balance and asks for an explicit confirm before
// submitting. Gas rides through the sponsored-tx flow like every other PTB.

import { useState } from "react";
import { useCurrentAccount } from "@mysten/dapp-kit";
import { useQueryClient } from "@tanstack/react-query";

import type { Bucket, Series } from "../api/client";
import { useCoinBalance } from "../api/useCoinBalance";
import { DEEPBOOK_POOL_CREATION_FEE, DEEP_COIN_TYPE } from "../config";
import {
  buildCreateVenueTx,
  deepbookConfigured,
  deriveVenueParams,
} from "../tx/deepbook";
import { useSubmitTransaction } from "../tx/submit";

const DEEP_DECIMALS = 6;

function fmtDeep(raw: bigint): string {
  return (Number(raw) / 10 ** DEEP_DECIMALS).toLocaleString("en-US", {
    maximumFractionDigits: 1,
  });
}

type Props = {
  bucket: Bucket;
  series: Series;
};

type Stage = "idle" | "confirm" | "submitting" | "pending";

/**
 * Renders nothing when there's nothing to do (pool exists, DeepBook not
 * deployed on this network, or the series is missing decimals). The parent
 * keys this component by bucket id so stage resets on strike change.
 */
export function CreateVenueCard({ bucket, series }: Props) {
  const account = useCurrentAccount();
  const submitTx = useSubmitTransaction();
  const queryClient = useQueryClient();
  const [stage, setStage] = useState<Stage>("idle");
  const [error, setError] = useState<string | null>(null);

  const deepBal = useCoinBalance(account?.address ?? null, DEEP_COIN_TYPE ?? null);

  if (!deepbookConfigured()) return null;
  if (bucket.deepbook_pool_id) return null;
  if (series.asset_decimals === null || series.settlement_decimals === null) return null;

  const fee = DEEPBOOK_POOL_CREATION_FEE!;
  const balance = BigInt(deepBal.data ?? "0");
  const enough = balance >= fee;

  const disabledReason = !account
    ? "connect a wallet to create the venue"
    : !enough
      ? `needs ${fmtDeep(fee)} DEEP · you have ${fmtDeep(balance)}`
      : null;

  const create = async () => {
    setError(null);
    setStage("submitting");
    try {
      const tx = buildCreateVenueTx({
        callCoinType: bucket.call_coin_type,
        settlementCoinType: series.settlement_coin_type,
        venue: deriveVenueParams(series.asset_decimals!, series.settlement_decimals!),
      });
      await submitTx(tx);
      // Success: the pool exists on-chain; the indexer picks the PoolCreated
      // event up within a few checkpoints and /buckets (already polled every
      // 5s) flips deepbook_pool_id — at which point the parent unmounts us.
      setStage("pending");
      queryClient.invalidateQueries({ queryKey: ["buckets"] });
    } catch (e) {
      // Includes the lost race where someone else listed the venue first
      // (the Move call aborts); the next /buckets poll resolves either way.
      setError(e instanceof Error ? e.message : String(e));
      setStage("idle");
      queryClient.invalidateQueries({ queryKey: ["buckets"] });
    }
  };

  return (
    <div className="panel" style={{ marginTop: 12 }}>
      <div className="panel__head">
        <span className="panel__head-dot"></span>trading venue
      </div>
      {stage === "pending" ? (
        <div className="panel__sub">
          Venue listed on DeepBook — waiting for the indexer to confirm…
        </div>
      ) : (
        <>
          <div className="panel__sub" style={{ marginBottom: 10 }}>
            This option has no DeepBook market yet. Create one to enable
            on-chain trading of this strike (one-time fee: {fmtDeep(fee)} DEEP,
            paid by your wallet).
          </div>
          {stage === "confirm" ? (
            <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
              <button
                className="cta"
                style={{ width: "auto", padding: "10px 18px" }}
                onClick={create}
              >
                Confirm · spend {fmtDeep(fee)} DEEP
              </button>
              <button
                className="cta"
                style={{ width: "auto", padding: "10px 18px", opacity: 0.7 }}
                onClick={() => setStage("idle")}
              >
                Cancel
              </button>
            </div>
          ) : (
            <button
              className="cta"
              style={{ width: "auto", padding: "10px 18px" }}
              disabled={disabledReason !== null || stage === "submitting"}
              onClick={() => setStage("confirm")}
            >
              {stage === "submitting"
                ? "Listing venue…"
                : (disabledReason ?? "Create trading venue")}
            </button>
          )}
          {error && (
            <div className="panel__sub" style={{ marginTop: 8, color: "var(--aqua-red, #d33)" }}>
              venue creation failed: {error}
            </div>
          )}
        </>
      )}
    </div>
  );
}
