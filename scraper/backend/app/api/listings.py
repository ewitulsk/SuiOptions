from datetime import datetime
from decimal import Decimal

from fastapi import APIRouter, Depends, HTTPException, Query
from pydantic import BaseModel, ConfigDict
from sqlalchemy import select
from sqlalchemy.orm import Session, selectinload

from app.auth.deps import current_user
from app.db import get_db
from app.db.models import Listing, Valuation

router = APIRouter(prefix="/api/listings", tags=["listings"], dependencies=[Depends(current_user)])


class ValuationOut(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: int
    model: str
    est_resale_low: Decimal
    est_resale_high: Decimal
    expected_days_to_sell: int | None
    max_buy_price: Decimal
    confidence: float
    risk_flags: list
    resale_channel: str | None
    rationale: str | None
    outreach_draft: str | None
    created_at: datetime


class ListingOut(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: int
    source: str
    external_id: str
    url: str
    title: str
    description: str | None
    price: Decimal
    currency: str
    location: str | None
    photos: list
    seller: str | None
    posted_at: datetime | None
    scraped_at: datetime
    saved_search_id: int | None
    triage_passed: bool | None
    triage_reason: str | None
    valuations: list[ValuationOut]


@router.get("", response_model=list[ListingOut])
def list_listings(
    db: Session = Depends(get_db),
    valued_only: bool = Query(False, description="only listings with at least one valuation"),
    limit: int = Query(100, le=500),
    offset: int = 0,
):
    stmt = (
        select(Listing)
        .options(selectinload(Listing.valuations))
        .order_by(Listing.scraped_at.desc())
        .limit(limit)
        .offset(offset)
    )
    if valued_only:
        stmt = stmt.where(Listing.valuations.any())
    return db.scalars(stmt).all()


@router.get("/{listing_id}", response_model=ListingOut)
def get_listing(listing_id: int, db: Session = Depends(get_db)):
    listing = db.get(Listing, listing_id, options=[selectinload(Listing.valuations)])
    if listing is None:
        raise HTTPException(status_code=404, detail="Listing not found")
    return listing


@router.get("/{listing_id}/valuations", response_model=list[ValuationOut])
def listing_valuations(listing_id: int, db: Session = Depends(get_db)):
    return db.scalars(
        select(Valuation).where(Valuation.listing_id == listing_id).order_by(Valuation.id.desc())
    ).all()
