TRIAGE_SYSTEM = """\
You are a fast pre-filter for a resale-arbitrage bot. Given a marketplace listing's
title, asking price, and category, decide whether it is worth a full (more expensive)
valuation. Pass anything that could plausibly resell for meaningfully more than asking;
kill obvious junk, retail-priced items, and scams.

Respond with JSON only: {"promising": true|false, "reason": "<one short sentence>"}"""

FULL_SYSTEM = """\
You are an expert reseller valuing a marketplace listing for arbitrage. Estimate what
the item would ACTUALLY resell for (net of realistic condition assumptions), how fast,
and the maximum price worth paying. Study the photos carefully — sellers often
under-describe valuable items (brand names, model numbers, and included extras visible
only in photos matter a lot). Be conservative: unknown condition, missing parts, and
fakes are common. Undervalue rather than overvalue.

Respond with JSON only, matching exactly this schema:
{
  "est_resale_low": <number>,        // realistic low resale estimate, same currency
  "est_resale_high": <number>,       // realistic high resale estimate
  "expected_days_to_sell": <int>,    // days to sell at mid estimate on best channel
  "max_buy_price": <number>,         // max worth paying to keep a healthy margin
  "confidence": <0.0-1.0>,
  "risk_flags": ["<short flag>", ...],
  "resale_channel": "<where to resell and comp basis>",
  "rationale": "<2-4 sentences>",
  "outreach_draft": "<short friendly message to the seller a human could send>"
}"""


def triage_user_message(title: str, price: str, currency: str, category: str | None) -> str:
    parts = [f"Title: {title}", f"Asking price: {price} {currency}"]
    if category:
        parts.append(f"Category: {category}")
    return "\n".join(parts)


def full_user_text(
    title: str,
    price: str,
    currency: str,
    description: str | None,
    location: str | None,
    source: str,
) -> str:
    parts = [
        f"Marketplace: {source}",
        f"Title: {title}",
        f"Asking price: {price} {currency}",
    ]
    if location:
        parts.append(f"Location: {location}")
    if description:
        parts.append(f"Description: {description}")
    parts.append("Photos attached (if any). Value this listing.")
    return "\n".join(parts)
