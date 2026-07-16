from app.adapters.base import MarketplaceAdapter, RawListing
from app.adapters.ebay import EbayAdapter

# Adapter registry. Adding a marketplace = one adapter module + one entry here.
ADAPTERS: dict[str, type[MarketplaceAdapter]] = {
    "ebay": EbayAdapter,
}


def get_adapter(source: str) -> MarketplaceAdapter:
    try:
        return ADAPTERS[source]()
    except KeyError:
        raise ValueError(f"Unknown marketplace source: {source!r}") from None


__all__ = ["ADAPTERS", "MarketplaceAdapter", "RawListing", "get_adapter"]
