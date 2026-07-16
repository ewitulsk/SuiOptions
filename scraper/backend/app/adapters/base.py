from abc import ABC, abstractmethod
from datetime import datetime
from decimal import Decimal

from pydantic import BaseModel

from app.db.models import SavedSearch


class RawListing(BaseModel):
    """Normalized listing shape every adapter maps into. Everything downstream
    (dedup, valuation, alerts, UI) is marketplace-agnostic."""

    source: str
    external_id: str
    url: str
    title: str
    description: str | None = None
    price: Decimal
    currency: str = "USD"
    location: str | None = None
    photos: list[str] = []
    seller: str | None = None
    posted_at: datetime | None = None


class MarketplaceAdapter(ABC):
    """One implementation per marketplace."""

    source: str

    @abstractmethod
    def search(self, saved_search: SavedSearch) -> list[RawListing]:
        """Run the saved search and return current listings (newest first)."""
        raise NotImplementedError
