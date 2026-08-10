// Assembles the taker-side swap PTB from a /v1/routes quote: a chain of
// settlement::fill_limit_order(_reverse) calls per path/hop/leg, one strict
// assert_coin_min slippage guard on the joined output, residual change back
// to the sender. Walks plan.paths (the flat ptbSkeleton lacks the path
// boundaries needed to thread hop outputs).

import { Transaction, coinWithBalance } from "@mysten/sui/transactions";
import type { TransactionObjectArgument } from "@mysten/sui/transactions";
import { bcs } from "@mysten/sui/bcs";
import { SUI_CLOCK_OBJECT_ID } from "@mysten/sui/utils";

import type { Market, RouteResponse } from "../api/orderbook";
import { toBigint } from "../format";
import { orderBytes, prefixedSignature, publicKeyBytes } from "./orderBcs";

const VecU8 = bcs.vector(bcs.u8());

export function buildRouteFillTx(
  quote: RouteResponse,
  markets: Map<string, Market>,
  opts: {
    sender: string;
    fromType: string;
    toType: string;
    /** User's slippage floor — replaces the server's zero-tolerance min. */
    minOut: bigint;
    packageId: string;
  },
): Transaction {
  const tx = new Transaction();
  tx.setSender(opts.sender);

  const pathOutputs: TransactionObjectArgument[] = [];
  // Change/dust coins (input or intermediate types) returned to the sender.
  const residuals: TransactionObjectArgument[] = [];

  for (const path of quote.plan.paths) {
    // The SDK resolves owned coins (incl. gas splitting for SUI) at build time.
    let hopInput: TransactionObjectArgument = tx.add(
      coinWithBalance({ type: opts.fromType, balance: toBigint(path.input) }),
    );

    path.hops.forEach((hop, hopIdx) => {
      const market = markets.get(path.markets[hopIdx]);
      if (!market) throw new Error(`quote references unknown market ${path.markets[hopIdx]}`);
      // Paying quote into an ask => fill_limit_order; paying base into a bid
      // => reverse (same rule the server uses for its ptbSkeleton).
      const hopFrom = path.tokens[hopIdx];
      const forward = hopFrom === market.quote;

      let takerCoin = hopInput;
      const outCoins: TransactionObjectArgument[] = [];
      for (const leg of hop) {
        const order = quote.orders[leg.digest];
        if (!order) throw new Error(`quote is missing the signed order for leg ${leg.digest}`);
        const res = tx.moveCall({
          target: `${opts.packageId}::settlement::${forward ? "fill_limit_order" : "fill_limit_order_reverse"}`,
          typeArguments: [market.base, market.quote],
          arguments: [
            tx.object(order.registryId),
            tx.object(order.makerManagerId),
            tx.pure(VecU8.serialize(orderBytes(order))),
            tx.pure(VecU8.serialize(prefixedSignature(order))),
            tx.pure(VecU8.serialize(publicKeyBytes(order))),
            takerCoin,
            tx.pure.u64(toBigint(leg.amountIn)),
            tx.pure.u64(0n), // intra-route: single strict guard at the end
            tx.object(SUI_CLOCK_OBJECT_ID),
          ],
        });
        outCoins.push(res[0]);
        // Remaining input funds the next leg; after the last leg it's dust.
        takerCoin = res[1];
      }
      residuals.push(takerCoin);

      if (outCoins.length > 1) tx.mergeCoins(outCoins[0], outCoins.slice(1));
      hopInput = outCoins[0];
    });

    pathOutputs.push(hopInput);
  }

  if (pathOutputs.length === 0) throw new Error("quote has no routable paths");
  if (pathOutputs.length > 1) tx.mergeCoins(pathOutputs[0], pathOutputs.slice(1));
  const output = pathOutputs[0];

  tx.moveCall({
    target: `${opts.packageId}::settlement::assert_coin_min`,
    typeArguments: [opts.toType],
    arguments: [output, tx.pure.u64(opts.minOut)],
  });

  tx.transferObjects([output, ...residuals], opts.sender);
  return tx;
}
