import logging

import httpx

from app.config import get_settings
from app.db.models import Listing, Valuation

logger = logging.getLogger(__name__)


def send_deal_alert(listing: Listing, valuation: Valuation) -> None:
    """Post a deal alert to the configured Discord webhook. Raises on failure so
    the caller can record alert status."""
    webhook = get_settings().discord_webhook_url
    if not webhook:
        raise RuntimeError("DISCORD_WEBHOOK_URL not configured")

    margin_low = float(valuation.est_resale_low) - float(listing.price)
    margin_high = float(valuation.est_resale_high) - float(listing.price)
    embed = {
        "title": listing.title[:250],
        "url": listing.url,
        "color": 0x2ECC71,
        "fields": [
            {"name": "Asking", "value": f"${listing.price}", "inline": True},
            {
                "name": "Est. resale",
                "value": f"${valuation.est_resale_low}–${valuation.est_resale_high}",
                "inline": True,
            },
            {
                "name": "Margin",
                "value": f"${margin_low:.0f}–${margin_high:.0f}",
                "inline": True,
            },
            {"name": "Max buy", "value": f"${valuation.max_buy_price}", "inline": True},
            {"name": "Confidence", "value": f"{valuation.confidence:.0%}", "inline": True},
            {
                "name": "Days to sell",
                "value": str(valuation.expected_days_to_sell or "?"),
                "inline": True,
            },
        ],
    }
    if valuation.risk_flags:
        embed["fields"].append(
            {"name": "Risks", "value": ", ".join(valuation.risk_flags)[:1024], "inline": False}
        )
    if valuation.rationale:
        embed["description"] = valuation.rationale[:2000]
    if listing.photos:
        embed["thumbnail"] = {"url": listing.photos[0]}
    if valuation.outreach_draft:
        embed["fields"].append(
            {
                "name": "Outreach draft (copy & send yourself)",
                "value": valuation.outreach_draft[:1024],
                "inline": False,
            }
        )

    resp = httpx.post(webhook, json={"embeds": [embed]}, timeout=15)
    resp.raise_for_status()
    logger.info("alert sent for listing %s", listing.id)
