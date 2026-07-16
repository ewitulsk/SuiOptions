from datetime import UTC, datetime
from decimal import Decimal

from sqlalchemy import (
    JSON,
    Boolean,
    Computed,
    DateTime,
    Float,
    ForeignKey,
    Integer,
    Numeric,
    String,
    Text,
    UniqueConstraint,
)
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, relationship


def utcnow() -> datetime:
    return datetime.now(UTC)


class Base(DeclarativeBase):
    pass


class User(Base):
    __tablename__ = "users"

    id: Mapped[int] = mapped_column(Integer, primary_key=True)
    username: Mapped[str] = mapped_column(String(64), unique=True, nullable=False)
    password_hash: Mapped[str] = mapped_column(String(128), nullable=False)
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)


class SavedSearch(Base):
    __tablename__ = "saved_searches"

    id: Mapped[int] = mapped_column(Integer, primary_key=True)
    source: Mapped[str] = mapped_column(String(32), nullable=False)  # adapter name, e.g. "ebay"
    name: Mapped[str] = mapped_column(String(128), nullable=False)
    query: Mapped[str] = mapped_column(String(256), nullable=False)
    category: Mapped[str | None] = mapped_column(String(64))
    min_price: Mapped[Decimal | None] = mapped_column(Numeric(10, 2))
    max_price: Mapped[Decimal | None] = mapped_column(Numeric(10, 2))
    poll_interval_seconds: Mapped[int] = mapped_column(Integer, default=300)
    # alert when valuation.max_buy_price >= asking_price * alert_threshold
    alert_threshold: Mapped[float] = mapped_column(Float, default=1.0)
    active: Mapped[bool] = mapped_column(Boolean, default=True)
    last_polled_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True))
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)

    listings: Mapped[list["Listing"]] = relationship(back_populates="saved_search")


class Listing(Base):
    __tablename__ = "listings"
    __table_args__ = (UniqueConstraint("source", "external_id", name="uq_listing_source_ext"),)

    id: Mapped[int] = mapped_column(Integer, primary_key=True)
    source: Mapped[str] = mapped_column(String(32), nullable=False)
    external_id: Mapped[str] = mapped_column(String(128), nullable=False)
    url: Mapped[str] = mapped_column(String(1024), nullable=False)
    title: Mapped[str] = mapped_column(String(512), nullable=False)
    description: Mapped[str | None] = mapped_column(Text)
    price: Mapped[Decimal] = mapped_column(Numeric(10, 2), nullable=False)
    currency: Mapped[str] = mapped_column(String(8), default="USD")
    location: Mapped[str | None] = mapped_column(String(256))
    photos: Mapped[list] = mapped_column(JSON, default=list)
    seller: Mapped[str | None] = mapped_column(String(128))
    posted_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True))
    scraped_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)
    saved_search_id: Mapped[int | None] = mapped_column(ForeignKey("saved_searches.id"))

    # cheap-model pre-filter result
    triage_passed: Mapped[bool | None] = mapped_column(Boolean)
    triage_reason: Mapped[str | None] = mapped_column(String(512))

    saved_search: Mapped[SavedSearch | None] = relationship(back_populates="listings")
    valuations: Mapped[list["Valuation"]] = relationship(back_populates="listing")


class Valuation(Base):
    __tablename__ = "valuations"

    id: Mapped[int] = mapped_column(Integer, primary_key=True)
    listing_id: Mapped[int] = mapped_column(ForeignKey("listings.id"), nullable=False)
    model: Mapped[str] = mapped_column(String(128), nullable=False)
    est_resale_low: Mapped[Decimal] = mapped_column(Numeric(10, 2), nullable=False)
    est_resale_high: Mapped[Decimal] = mapped_column(Numeric(10, 2), nullable=False)
    expected_days_to_sell: Mapped[int | None] = mapped_column(Integer)
    max_buy_price: Mapped[Decimal] = mapped_column(Numeric(10, 2), nullable=False)
    confidence: Mapped[float] = mapped_column(Float, nullable=False)
    risk_flags: Mapped[list] = mapped_column(JSON, default=list)
    resale_channel: Mapped[str | None] = mapped_column(String(256))
    rationale: Mapped[str | None] = mapped_column(Text)
    outreach_draft: Mapped[str | None] = mapped_column(Text)
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)

    listing: Mapped[Listing] = relationship(back_populates="valuations")


class Alert(Base):
    __tablename__ = "alerts"

    id: Mapped[int] = mapped_column(Integer, primary_key=True)
    listing_id: Mapped[int] = mapped_column(ForeignKey("listings.id"), nullable=False)
    valuation_id: Mapped[int] = mapped_column(ForeignKey("valuations.id"), nullable=False)
    channel: Mapped[str] = mapped_column(String(32), default="discord")
    status: Mapped[str] = mapped_column(String(16), default="sent")  # sent | failed
    error: Mapped[str | None] = mapped_column(String(512))
    sent_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)


DEAL_STATUSES = ("watching", "contacted", "bought", "listed", "sold", "dead")


class Deal(Base):
    """The money ledger: what we ACTUALLY paid and ACTUALLY received.
    net_profit is a generated column so dashboard math can't drift from the data."""

    __tablename__ = "deals"

    id: Mapped[int] = mapped_column(Integer, primary_key=True)
    listing_id: Mapped[int | None] = mapped_column(ForeignKey("listings.id"))
    # title copied from the listing, or entered by hand for off-platform finds
    title: Mapped[str] = mapped_column(String(512), nullable=False)
    status: Mapped[str] = mapped_column(String(16), default="watching")

    buy_price: Mapped[Decimal | None] = mapped_column(Numeric(10, 2))
    buy_extra_costs: Mapped[Decimal] = mapped_column(Numeric(10, 2), default=Decimal("0"))
    bought_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True))
    bought_by: Mapped[int | None] = mapped_column(ForeignKey("users.id"))

    sale_price: Mapped[Decimal | None] = mapped_column(Numeric(10, 2))
    sale_fees: Mapped[Decimal] = mapped_column(Numeric(10, 2), default=Decimal("0"))
    sold_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True))
    sale_channel: Mapped[str | None] = mapped_column(String(64))

    notes: Mapped[str | None] = mapped_column(Text)
    # NULL until both sides of the trade are recorded
    net_profit: Mapped[Decimal | None] = mapped_column(
        Numeric(10, 2),
        Computed("sale_price - sale_fees - buy_price - buy_extra_costs"),
    )
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)
    updated_at: Mapped[datetime] = mapped_column(
        DateTime(timezone=True), default=utcnow, onupdate=utcnow
    )

    listing: Mapped[Listing | None] = relationship()
    buyer: Mapped[User | None] = relationship()
