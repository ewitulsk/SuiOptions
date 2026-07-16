from decimal import Decimal

import httpx
import respx

from app.adapters.ebay import EbayAdapter
from app.db.models import SavedSearch

SEARCH_RESPONSE = {
    "itemSummaries": [
        {
            "itemId": "v1|110587|0",
            "title": "Mitutoyo digital micrometer 0-1in",
            "shortDescription": "Lightly used, works great",
            "price": {"value": "80.00", "currency": "USD"},
            "itemWebUrl": "https://www.ebay.com/itm/110587",
            "image": {"imageUrl": "https://i.ebayimg.com/1.jpg"},
            "additionalImages": [{"imageUrl": "https://i.ebayimg.com/2.jpg"}],
            "itemLocation": {"city": "Gladstone", "stateOrProvince": "MO", "country": "US"},
            "itemCreationDate": "2026-07-15T12:00:00.000Z",
            "seller": {"username": "toolguy55"},
        },
        {
            # malformed: missing price — should be skipped, not crash
            "itemId": "v1|999|0",
            "title": "Broken item",
            "itemWebUrl": "https://www.ebay.com/itm/999",
        },
    ]
}


@respx.mock
def test_ebay_search_maps_listings():
    EbayAdapter._token = None  # reset class-level token cache
    respx.post("https://api.ebay.com/identity/v1/oauth2/token").mock(
        return_value=httpx.Response(200, json={"access_token": "tok", "expires_in": 7200})
    )
    search_route = respx.get("https://api.ebay.com/buy/browse/v1/item_summary/search").mock(
        return_value=httpx.Response(200, json=SEARCH_RESPONSE)
    )

    search = SavedSearch(
        id=1,
        source="ebay",
        name="micrometers",
        query="mitutoyo micrometer",
        max_price=Decimal("150"),
    )
    listings = EbayAdapter().search(search)

    assert len(listings) == 1  # malformed item skipped
    listing = listings[0]
    assert listing.external_id == "v1|110587|0"
    assert listing.price == Decimal("80.00")
    assert listing.photos == ["https://i.ebayimg.com/1.jpg", "https://i.ebayimg.com/2.jpg"]
    assert listing.location == "Gladstone, MO, US"
    assert listing.seller == "toolguy55"

    params = search_route.calls[0].request.url.params
    assert params["q"] == "mitutoyo micrometer"
    assert params["sort"] == "newlyListed"
    assert "price:[..150]" in params["filter"]
