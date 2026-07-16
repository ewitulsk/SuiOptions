from decimal import Decimal

from app import pipeline
from app.adapters.base import RawListing
from app.db.models import Alert, SavedSearch, Valuation
from app.valuation.schemas import TriageResult, ValuationResult


def _raw(external_id="v1|1|0", price="80.00") -> RawListing:
    return RawListing(
        source="ebay",
        external_id=external_id,
        url=f"https://ebay.example/{external_id}",
        title="Machinist tools",
        price=Decimal(price),
    )


def _search(db) -> SavedSearch:
    search = SavedSearch(source="ebay", name="tools", query="machinist tools")
    db.add(search)
    db.commit()
    return search


def test_ingest_dedupes(db):
    search = _search(db)
    first = pipeline.ingest(db, search, [_raw(), _raw("v1|2|0")])
    assert len(first) == 2
    second = pipeline.ingest(db, search, [_raw(), _raw("v1|3|0")])
    assert len(second) == 1
    assert second[0].external_id == "v1|3|0"


def test_process_listing_full_flow_alerts(db, monkeypatch):
    search = _search(db)
    [listing] = pipeline.ingest(db, search, [_raw()])

    monkeypatch.setattr(
        pipeline.llm, "triage", lambda listing: TriageResult(promising=True, reason="tools lot")
    )
    monkeypatch.setattr(
        pipeline.llm,
        "full_valuation",
        lambda listing: ValuationResult(
            est_resale_low=Decimal("400"),
            est_resale_high=Decimal("600"),
            expected_days_to_sell=14,
            max_buy_price=Decimal("250"),
            confidence=0.8,
            risk_flags=["condition unknown"],
        ),
    )
    sent = []
    monkeypatch.setattr(pipeline.discord, "send_deal_alert", lambda lst, val: sent.append(lst.id))

    valuation = pipeline.process_listing(db, listing, search)

    assert valuation is not None
    assert db.get(Valuation, valuation.id).max_buy_price == Decimal("250.00")
    assert listing.triage_passed is True
    assert sent == [listing.id]
    alert = db.query(Alert).one()
    assert alert.status == "sent"


def test_process_listing_triaged_out(db, monkeypatch):
    search = _search(db)
    [listing] = pipeline.ingest(db, search, [_raw()])
    monkeypatch.setattr(
        pipeline.llm, "triage", lambda listing: TriageResult(promising=False, reason="junk")
    )
    assert pipeline.process_listing(db, listing, search) is None
    assert listing.triage_passed is False
    assert db.query(Valuation).count() == 0


def test_no_alert_below_threshold(db, monkeypatch):
    search = _search(db)
    search.alert_threshold = 2.0  # require max_buy >= 2x asking
    db.commit()
    [listing] = pipeline.ingest(db, search, [_raw(price="200.00")])

    monkeypatch.setattr(
        pipeline.llm, "triage", lambda listing: TriageResult(promising=True, reason="ok")
    )
    monkeypatch.setattr(
        pipeline.llm,
        "full_valuation",
        lambda listing: ValuationResult(
            est_resale_low=Decimal("250"),
            est_resale_high=Decimal("300"),
            max_buy_price=Decimal("220"),  # 220 < 200 * 2.0
            confidence=0.6,
        ),
    )
    monkeypatch.setattr(
        pipeline.discord,
        "send_deal_alert",
        lambda lst, val: (_ for _ in ()).throw(AssertionError("should not alert")),
    )
    valuation = pipeline.process_listing(db, listing, search)
    assert valuation is not None
    assert db.query(Alert).count() == 0
