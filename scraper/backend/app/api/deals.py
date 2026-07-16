from datetime import UTC, datetime, timedelta
from decimal import Decimal

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel, ConfigDict, Field
from sqlalchemy import select
from sqlalchemy.orm import Session

from app.auth.deps import current_user
from app.db import get_db
from app.db.models import DEAL_STATUSES, Deal, Listing, User, utcnow

router = APIRouter(prefix="/api/deals", tags=["deals"])


class DealOut(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: int
    listing_id: int | None
    title: str
    status: str
    buy_price: Decimal | None
    buy_extra_costs: Decimal
    bought_at: datetime | None
    bought_by: int | None
    sale_price: Decimal | None
    sale_fees: Decimal
    sold_at: datetime | None
    sale_channel: str | None
    notes: str | None
    net_profit: Decimal | None
    created_at: datetime
    updated_at: datetime


class DealCreate(BaseModel):
    listing_id: int | None = None
    title: str | None = None  # required when no listing_id (off-platform finds)
    notes: str | None = None


class MarkBought(BaseModel):
    buy_price: Decimal = Field(ge=0)
    buy_extra_costs: Decimal = Field(default=Decimal("0"), ge=0)
    bought_at: datetime | None = None
    notes: str | None = None


class MarkSold(BaseModel):
    sale_price: Decimal = Field(ge=0)
    sale_fees: Decimal = Field(default=Decimal("0"), ge=0)
    sold_at: datetime | None = None
    sale_channel: str | None = None
    notes: str | None = None


class DealPatch(BaseModel):
    """After-the-fact edits (typo fixes). Only provided fields change."""

    status: str | None = None
    title: str | None = None
    buy_price: Decimal | None = None
    buy_extra_costs: Decimal | None = None
    bought_at: datetime | None = None
    sale_price: Decimal | None = None
    sale_fees: Decimal | None = None
    sold_at: datetime | None = None
    sale_channel: str | None = None
    notes: str | None = None


def _get_deal(db: Session, deal_id: int) -> Deal:
    deal = db.get(Deal, deal_id)
    if deal is None:
        raise HTTPException(status_code=404, detail="Deal not found")
    return deal


@router.get("", response_model=list[DealOut])
def list_deals(
    db: Session = Depends(get_db),
    user: User = Depends(current_user),
    status: str | None = None,
):
    stmt = select(Deal).order_by(Deal.updated_at.desc())
    if status:
        stmt = stmt.where(Deal.status == status)
    return db.scalars(stmt).all()


@router.post("", response_model=DealOut, status_code=201)
def create_deal(
    body: DealCreate, db: Session = Depends(get_db), user: User = Depends(current_user)
):
    title = body.title
    if body.listing_id is not None:
        listing = db.get(Listing, body.listing_id)
        if listing is None:
            raise HTTPException(status_code=404, detail="Listing not found")
        existing = db.scalar(select(Deal).where(Deal.listing_id == body.listing_id))
        if existing:
            raise HTTPException(
                status_code=409, detail=f"Deal {existing.id} already tracks this listing"
            )
        title = title or listing.title
    if not title:
        raise HTTPException(status_code=422, detail="title is required for manual deals")
    deal = Deal(listing_id=body.listing_id, title=title, notes=body.notes)
    db.add(deal)
    db.commit()
    db.refresh(deal)
    return deal


@router.post("/{deal_id}/bought", response_model=DealOut)
def mark_bought(
    deal_id: int,
    body: MarkBought,
    db: Session = Depends(get_db),
    user: User = Depends(current_user),
):
    deal = _get_deal(db, deal_id)
    deal.status = "bought"
    deal.buy_price = body.buy_price
    deal.buy_extra_costs = body.buy_extra_costs
    deal.bought_at = body.bought_at or utcnow()
    deal.bought_by = user.id
    if body.notes:
        deal.notes = body.notes
    db.commit()
    db.refresh(deal)
    return deal


@router.post("/{deal_id}/sold", response_model=DealOut)
def mark_sold(
    deal_id: int,
    body: MarkSold,
    db: Session = Depends(get_db),
    user: User = Depends(current_user),
):
    deal = _get_deal(db, deal_id)
    if deal.buy_price is None:
        raise HTTPException(status_code=422, detail="Record the buy first (mark bought)")
    deal.status = "sold"
    deal.sale_price = body.sale_price
    deal.sale_fees = body.sale_fees
    deal.sold_at = body.sold_at or utcnow()
    deal.sale_channel = body.sale_channel
    if body.notes:
        deal.notes = body.notes
    db.commit()
    db.refresh(deal)
    return deal


@router.patch("/{deal_id}", response_model=DealOut)
def patch_deal(
    deal_id: int,
    body: DealPatch,
    db: Session = Depends(get_db),
    user: User = Depends(current_user),
):
    deal = _get_deal(db, deal_id)
    updates = body.model_dump(exclude_unset=True)
    if "status" in updates and updates["status"] not in DEAL_STATUSES:
        raise HTTPException(status_code=422, detail=f"status must be one of {DEAL_STATUSES}")
    for key, value in updates.items():
        setattr(deal, key, value)
    db.commit()
    db.refresh(deal)
    return deal


class UserPnl(BaseModel):
    user_id: int
    username: str
    deals_bought: int
    realized_profit: Decimal


class DealStats(BaseModel):
    realized_profit_all_time: Decimal
    realized_profit_30d: Decimal
    capital_tied_up: Decimal  # cost basis of bought/listed-but-unsold deals
    deals_sold: int
    win_rate: float  # share of sold deals with positive net profit
    avg_days_to_sell: float | None
    per_user: list[UserPnl]


@router.get("/stats", response_model=DealStats)
def deal_stats(db: Session = Depends(get_db), user: User = Depends(current_user)):
    deals = db.scalars(select(Deal)).all()
    sold = [d for d in deals if d.status == "sold" and d.net_profit is not None]
    unsold = [d for d in deals if d.status in ("bought", "listed") and d.buy_price is not None]

    cutoff = utcnow() - timedelta(days=30)
    zero = Decimal("0")

    def _aware(dt: datetime) -> datetime:

        return dt if dt.tzinfo else dt.replace(tzinfo=UTC)

    realized_all = sum((d.net_profit for d in sold), zero)
    realized_30d = sum(
        (d.net_profit for d in sold if d.sold_at and _aware(d.sold_at) >= cutoff), zero
    )
    capital = sum((d.buy_price + d.buy_extra_costs for d in unsold), zero)
    wins = sum(1 for d in sold if d.net_profit > 0)
    days = [
        (_aware(d.sold_at) - _aware(d.bought_at)).total_seconds() / 86400
        for d in sold
        if d.sold_at and d.bought_at
    ]

    users = {u.id: u.username for u in db.scalars(select(User)).all()}
    per_user: dict[int, UserPnl] = {}
    for d in deals:
        if d.bought_by is None:
            continue
        entry = per_user.setdefault(
            d.bought_by,
            UserPnl(
                user_id=d.bought_by,
                username=users.get(d.bought_by, "?"),
                deals_bought=0,
                realized_profit=zero,
            ),
        )
        entry.deals_bought += 1
        if d.status == "sold" and d.net_profit is not None:
            entry.realized_profit += d.net_profit

    return DealStats(
        realized_profit_all_time=realized_all,
        realized_profit_30d=realized_30d,
        capital_tied_up=capital,
        deals_sold=len(sold),
        win_rate=(wins / len(sold)) if sold else 0.0,
        avg_days_to_sell=(sum(days) / len(days)) if days else None,
        per_user=sorted(per_user.values(), key=lambda p: p.realized_profit, reverse=True),
    )
