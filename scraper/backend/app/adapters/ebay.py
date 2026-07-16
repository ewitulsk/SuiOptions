import base64
import logging
import time
from datetime import datetime
from decimal import Decimal

import httpx

from app.adapters.base import MarketplaceAdapter, RawListing
from app.config import get_settings
from app.db.models import SavedSearch

logger = logging.getLogger(__name__)

_HOSTS = {
    "production": "https://api.ebay.com",
    "sandbox": "https://api.sandbox.ebay.com",
}


class EbayAdapter(MarketplaceAdapter):
    """eBay Browse API (official, ToS-clean). Uses the client-credentials OAuth
    flow; set EBAY_CLIENT_ID / EBAY_CLIENT_SECRET."""

    source = "ebay"

    _token: str | None = None
    _token_expires_at: float = 0.0

    def __init__(self, client: httpx.Client | None = None):
        settings = get_settings()
        self.host = _HOSTS[settings.ebay_env]
        self.client = client or httpx.Client(timeout=30)

    def _access_token(self) -> str:
        if EbayAdapter._token and time.time() < EbayAdapter._token_expires_at - 60:
            return EbayAdapter._token
        settings = get_settings()
        if not settings.ebay_client_id or not settings.ebay_client_secret:
            raise RuntimeError("EBAY_CLIENT_ID / EBAY_CLIENT_SECRET not configured")
        basic = base64.b64encode(
            f"{settings.ebay_client_id}:{settings.ebay_client_secret}".encode()
        ).decode()
        resp = self.client.post(
            f"{self.host}/identity/v1/oauth2/token",
            headers={
                "Authorization": f"Basic {basic}",
                "Content-Type": "application/x-www-form-urlencoded",
            },
            data={
                "grant_type": "client_credentials",
                "scope": "https://api.ebay.com/oauth/api_scope",
            },
        )
        resp.raise_for_status()
        payload = resp.json()
        EbayAdapter._token = payload["access_token"]
        EbayAdapter._token_expires_at = time.time() + int(payload.get("expires_in", 7200))
        return EbayAdapter._token

    def search(self, saved_search: SavedSearch) -> list[RawListing]:
        params: dict = {
            "q": saved_search.query,
            "sort": "newlyListed",
            "limit": "50",
        }
        if saved_search.category:
            params["category_ids"] = saved_search.category
        filters = ["buyingOptions:{FIXED_PRICE|BEST_OFFER}"]
        if saved_search.min_price is not None or saved_search.max_price is not None:
            lo = saved_search.min_price if saved_search.min_price is not None else ""
            hi = saved_search.max_price if saved_search.max_price is not None else ""
            filters.append(f"price:[{lo}..{hi}]")
            filters.append("priceCurrency:USD")
        params["filter"] = ",".join(filters)

        resp = self.client.get(
            f"{self.host}/buy/browse/v1/item_summary/search",
            params=params,
            headers={"Authorization": f"Bearer {self._access_token()}"},
        )
        resp.raise_for_status()
        items = resp.json().get("itemSummaries", []) or []
        listings = []
        for item in items:
            try:
                listings.append(self._to_raw_listing(item))
            except (KeyError, ValueError) as e:
                logger.warning("skipping malformed eBay item %s: %s", item.get("itemId"), e)
        return listings

    def _to_raw_listing(self, item: dict) -> RawListing:
        photos = []
        if image := item.get("image"):
            photos.append(image["imageUrl"])
        for extra in item.get("additionalImages", []) or []:
            photos.append(extra["imageUrl"])
        location = None
        if loc := item.get("itemLocation"):
            location = ", ".join(
                p for p in [loc.get("city"), loc.get("stateOrProvince"), loc.get("country")] if p
            )
        posted_at = None
        if created := item.get("itemCreationDate"):
            posted_at = datetime.fromisoformat(created.replace("Z", "+00:00"))
        return RawListing(
            source=self.source,
            external_id=item["itemId"],
            url=item["itemWebUrl"],
            title=item["title"],
            description=item.get("shortDescription"),
            price=Decimal(item["price"]["value"]),
            currency=item["price"].get("currency", "USD"),
            location=location,
            photos=photos,
            seller=(item.get("seller") or {}).get("username"),
            posted_at=posted_at,
        )
