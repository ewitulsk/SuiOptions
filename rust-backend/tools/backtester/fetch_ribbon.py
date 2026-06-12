#!/usr/bin/env python3
"""Scrape Ribbon V2 Theta Vault round history straight from Ethereum logs.

Free public RPC, stdlib only — no Dune, no paid data. Produces
``datasets/ribbon_rounds.csv``: one row per weekly round for T-ETH-C and
T-WBTC-C with the oToken's strike/expiry, collateral locked, the Gnosis
auction premium (when the round sold via Gnosis — Ribbon moved some sales
to Paradigm OTC in late 2022, those rounds have empty premium fields), and
the fees the vault actually charged.

This is the milestone-2 validation target (vault-implementation-guide doc
06 §9.4): the only real-world track record of the exact strategy our vault
runs. Event topic hashes are precomputed keccak256 of the V2 signatures
and were verified against on-chain logs.
"""

import csv
import json
import sys
import time
import urllib.request
from pathlib import Path

# Both verified to serve full 50k-block eth_getLogs ranges keylessly.
RPC_URLS = [
    "https://ethereum-rpc.publicnode.com",
    "https://rpc.mevblocker.io",
]

VAULTS = {
    "0x25751853eab4d0eb3652b5eb6ecb102a2789644b": "T-ETH-C",
    "0x65a833afdc250d9d38f8cd9bc2b1e3132db13b2f": "T-WBTC-C",
}
GNOSIS_EASY_AUCTION = "0x0b7ffc1f4ad541a4ed16b40d8c37f0929158d101"

# keccak256 event signatures (RibbonThetaVault V2 / Gnosis EasyAuction).
T_OPEN_SHORT = "0x045c558fdce4714c5816d53820d27420f4cd860892df203fe636384d8d19aa01"
# OpenShort(address indexed options, uint256 depositAmount, address indexed manager)
T_INIT_AUCTION = "0x95ad3b10488285d6307fda297f633faaf2a0d713c08ebe5f49c1b9255b01d29e"
# InitiateGnosisAuction(address indexed auctioningToken, address indexed
#   biddingToken, uint256 auctionCounter, address indexed manager)
T_COLLECT_FEES = "0x0a242f7ecaf711036ca770774ceffae28e60ef042ac113ddd187f2631db0c006"
# CollectVaultFees(uint256 performanceFee, uint256 vaultFee, uint256 round,
#   address indexed feeRecipient)
T_AUCTION_CLEARED = "0x4d160a2a345f2faeb9ac2e65272820b8ca5473b80aabef416bdf7e07ee7f5910"
# AuctionCleared(uint256 indexed auctionId, uint96 soldAuctioningTokens,
#   uint96 soldBiddingTokens, bytes32 clearingPriceOrder)

# oToken function selectors.
SEL_STRIKE_PRICE = "0xc52987cf"  # strikePrice() → uint256, 1e8 USD
SEL_EXPIRY = "0xade6e2aa"  # expiryTimestamp() → uint256
SEL_IS_PUT = "0xf3c274a6"  # isPut() → bool

# Ribbon V2 vaults deployed Sept 2021; activity wound down through 2023.
FROM_BLOCK = 13_300_000
TO_BLOCK = 18_900_000  # end of 2023
WINDOW = 49_000  # publicnode caps eth_getLogs ranges at 50k blocks

OUT = Path(__file__).parent / "datasets" / "ribbon_rounds.csv"

_rpc_idx = 0


def rpc(method, params, retries=10):
    global _rpc_idx
    payload = json.dumps({"jsonrpc": "2.0", "method": method, "params": params, "id": 1})
    for attempt in range(retries):
        url = RPC_URLS[_rpc_idx % len(RPC_URLS)]
        try:
            req = urllib.request.Request(
                url,
                data=payload.encode(),
                headers={
                    "Content-Type": "application/json",
                    "User-Agent": "suioptions-backtester",
                },
            )
            with urllib.request.urlopen(req, timeout=45) as resp:
                out = json.load(resp)
            if "error" in out:
                raise RuntimeError(out["error"])
            return out["result"]
        except Exception as e:  # noqa: BLE001 — rotate, then come back around
            _rpc_idx += 1
            wait = min(2**attempt, 20)
            print(f"  rpc retry ({e}) in {wait}s", file=sys.stderr)
            time.sleep(wait)
    raise RuntimeError(f"rpc {method} failed after {retries} attempts")


def get_logs(address, topics, from_block, to_block):
    out = []
    start = from_block
    while start <= to_block:
        end = min(start + WINDOW, to_block)
        out.extend(
            rpc(
                "eth_getLogs",
                [
                    {
                        "address": address,
                        "topics": topics,
                        "fromBlock": hex(start),
                        "toBlock": hex(end),
                    }
                ],
            )
        )
        print(f"  blocks {start}-{end}: {len(out)} logs so far", file=sys.stderr)
        start = end + 1
        time.sleep(0.15)
    return out


def topic_to_address(topic):
    return "0x" + topic[-40:]


def word(data, i):
    """i-th 32-byte word of a log's data field, as int."""
    return int(data[2 + 64 * i : 2 + 64 * (i + 1)], 16)


def eth_call(to, selector):
    res = rpc("eth_call", [{"to": to, "data": selector}, "latest"])
    return int(res, 16) if res and res != "0x" else None


def block_timestamps(blocks):
    """blockNumber → unix ts, fetched one block at a time (batch JSON-RPC
    is inconsistently supported across free RPCs)."""
    out = {}
    for i, b in enumerate(sorted(blocks)):
        res = rpc("eth_getBlockByNumber", [hex(b), False])
        out[b] = int(res["timestamp"], 16)
        if i % 25 == 0:
            print(f"  block timestamps {i}/{len(blocks)}", file=sys.stderr)
        time.sleep(0.1)
    return out


def main():
    OUT.parent.mkdir(exist_ok=True)
    vault_addrs = list(VAULTS)

    print("1/4 vault logs (OpenShort / InitiateGnosisAuction / CollectVaultFees)…",
          file=sys.stderr)
    vault_logs = get_logs(
        vault_addrs, [[T_OPEN_SHORT, T_INIT_AUCTION, T_COLLECT_FEES]], FROM_BLOCK, TO_BLOCK
    )

    print("2/4 Gnosis AuctionCleared logs…", file=sys.stderr)
    cleared_logs = get_logs(GNOSIS_EASY_AUCTION, [[T_AUCTION_CLEARED]], FROM_BLOCK, TO_BLOCK)
    cleared = {}
    for l in cleared_logs:
        auction_id = int(l["topics"][1], 16)
        cleared[auction_id] = {
            "otokens_sold_raw": word(l["data"], 0),
            "premium_raw": word(l["data"], 1),
        }

    # Group per vault, ordered by block.
    opens, auctions, fees = {}, {}, {}
    for l in vault_logs:
        vault = l["address"].lower()
        block = int(l["blockNumber"], 16)
        t0 = l["topics"][0]
        if t0 == T_OPEN_SHORT:
            opens.setdefault(vault, []).append(
                {
                    "block": block,
                    "otoken": topic_to_address(l["topics"][1]),
                    "deposit_raw": word(l["data"], 0),
                }
            )
        elif t0 == T_INIT_AUCTION:
            auctions.setdefault(vault, []).append(
                {
                    "block": block,
                    "bidding_token": topic_to_address(l["topics"][2]),
                    "auction_id": word(l["data"], 0),
                }
            )
        elif t0 == T_COLLECT_FEES:
            fees.setdefault(vault, []).append(
                {
                    "block": block,
                    "perf_fee_raw": word(l["data"], 0),
                    "vault_fee_raw": word(l["data"], 1),
                    "round": word(l["data"], 2),
                }
            )

    print("3/4 oToken strike/expiry via eth_call…", file=sys.stderr)
    otokens = {o["otoken"] for rounds in opens.values() for o in rounds}
    details = {}
    for i, ot in enumerate(sorted(otokens)):
        details[ot] = {
            "strike_raw": eth_call(ot, SEL_STRIKE_PRICE),
            "expiry_ts": eth_call(ot, SEL_EXPIRY),
            "is_put": eth_call(ot, SEL_IS_PUT),
        }
        if i % 20 == 0:
            print(f"  otokens {i}/{len(otokens)}", file=sys.stderr)
        time.sleep(0.1)

    blocks = {o["block"] for rounds in opens.values() for o in rounds}
    print(f"4/4 timestamps for {len(blocks)} round-open blocks…", file=sys.stderr)
    ts = block_timestamps(blocks)

    rows = []
    for vault, rounds in opens.items():
        rounds.sort(key=lambda r: r["block"])
        vault_auctions = sorted(auctions.get(vault, []), key=lambda a: a["block"])
        vault_fees = sorted(fees.get(vault, []), key=lambda f: f["block"])
        for i, r in enumerate(rounds):
            next_block = rounds[i + 1]["block"] if i + 1 < len(rounds) else TO_BLOCK
            # The round's auction(s): initiated after this OpenShort, before
            # the next. Sum cleared premiums (re-auctions are rare but real).
            premium_raw = otokens_sold_raw = 0
            bidding_token = ""
            has_auction = False
            for a in vault_auctions:
                if r["block"] <= a["block"] < next_block and a["auction_id"] in cleared:
                    has_auction = True
                    bidding_token = a["bidding_token"]
                    premium_raw += cleared[a["auction_id"]]["premium_raw"]
                    otokens_sold_raw += cleared[a["auction_id"]]["otokens_sold_raw"]
            fee = next(
                (f for f in vault_fees if r["block"] <= f["block"] < next_block), None
            )
            d = details[r["otoken"]]
            rows.append(
                {
                    "vault": VAULTS[vault],
                    "round_open_ts": ts[r["block"]],
                    "block": r["block"],
                    "otoken": r["otoken"],
                    "strike_usd": (d["strike_raw"] or 0) / 1e8,
                    "expiry_ts": d["expiry_ts"] or 0,
                    "is_put": int(bool(d["is_put"])),
                    "collateral_raw": r["deposit_raw"],
                    "premium_raw": premium_raw if has_auction else "",
                    "otokens_sold_raw": otokens_sold_raw if has_auction else "",
                    "bidding_token": bidding_token,
                    "perf_fee_raw": fee["perf_fee_raw"] if fee else "",
                    "vault_fee_raw": fee["vault_fee_raw"] if fee else "",
                    "fee_round": fee["round"] if fee else "",
                }
            )

    rows.sort(key=lambda r: (r["vault"], r["round_open_ts"]))
    with open(OUT, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0]))
        w.writeheader()
        w.writerows(rows)
    print(f"{OUT}: {len(rows)} rounds "
          f"({sum(1 for r in rows if r['premium_raw'] != '')} with Gnosis premiums)")


if __name__ == "__main__":
    main()
