import logging

from sqlalchemy import select
from sqlalchemy.orm import Session

from app.adapters.base import RawListing
from app.db.models import Alert, Listing, SavedSearch, Valuation
from app.notify import discord
from app.valuation import llm

logger = logging.getLogger(__name__)


def ingest(db: Session, search: SavedSearch, raw_listings: list[RawListing]) -> list[Listing]:
    """Persist unseen listings; return only the newly-inserted ones."""
    new: list[Listing] = []
    for raw in raw_listings:
        exists = db.scalar(
            select(Listing.id).where(
                Listing.source == raw.source, Listing.external_id == raw.external_id
            )
        )
        if exists:
            continue
        listing = Listing(saved_search_id=search.id, **raw.model_dump())
        db.add(listing)
        new.append(listing)
    db.commit()
    return new


def process_listing(db: Session, listing: Listing, search: SavedSearch) -> Valuation | None:
    """Triage -> full valuation -> alert if the numbers clear the search threshold.
    Returns the stored Valuation, or None if triaged out / valuation failed."""
    triage_result = llm.triage(listing)
    listing.triage_passed = triage_result.promising
    listing.triage_reason = triage_result.reason
    db.commit()
    if not triage_result.promising:
        return None

    try:
        result = llm.full_valuation(listing)
    except llm.ValuationError as e:
        logger.error("valuation failed for listing %s: %s", listing.id, e)
        return None

    from app.config import get_settings

    valuation = Valuation(
        listing_id=listing.id,
        model=get_settings().full_model,
        **result.model_dump(),
    )
    db.add(valuation)
    db.commit()

    if float(result.max_buy_price) >= float(listing.price) * search.alert_threshold:
        _send_alert(db, listing, valuation)
    return valuation


def _send_alert(db: Session, listing: Listing, valuation: Valuation) -> None:
    alert = Alert(listing_id=listing.id, valuation_id=valuation.id, channel="discord")
    try:
        discord.send_deal_alert(listing, valuation)
        alert.status = "sent"
    except Exception as e:
        logger.error("alert failed for listing %s: %s", listing.id, e)
        alert.status = "failed"
        alert.error = str(e)[:512]
    db.add(alert)
    db.commit()
