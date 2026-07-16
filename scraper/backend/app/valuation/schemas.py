from decimal import Decimal

from pydantic import BaseModel, Field


class TriageResult(BaseModel):
    promising: bool
    reason: str = ""


class ValuationResult(BaseModel):
    est_resale_low: Decimal
    est_resale_high: Decimal
    expected_days_to_sell: int | None = None
    max_buy_price: Decimal
    confidence: float = Field(ge=0.0, le=1.0)
    risk_flags: list[str] = []
    resale_channel: str | None = None
    rationale: str | None = None
    outreach_draft: str | None = None
