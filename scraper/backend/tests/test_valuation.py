import json
from decimal import Decimal
from types import SimpleNamespace

import pytest

from app.db.models import Listing
from app.valuation import llm


def _fake_completion(payload: dict):
    def completion(**kwargs):
        message = SimpleNamespace(content=json.dumps(payload))
        return SimpleNamespace(choices=[SimpleNamespace(message=message)])

    return completion


def _listing() -> Listing:
    return Listing(
        id=1,
        source="ebay",
        external_id="x",
        url="https://ebay.example/x",
        title="Machinist tools",
        price=Decimal("80.00"),
        photos=["https://img.example/1.jpg"],
        saved_search=None,
    )


def test_extract_json_direct():
    assert llm._extract_json('{"a": 1}') == {"a": 1}


def test_extract_json_fenced():
    content = 'Here you go:\n```json\n{"a": 1}\n```\nDone.'
    assert llm._extract_json(content) == {"a": 1}


def test_extract_json_garbage_raises():
    with pytest.raises(llm.ValuationError):
        llm._extract_json("no json here")


def test_triage_parses(monkeypatch):
    monkeypatch.setattr(
        llm.litellm, "completion", _fake_completion({"promising": True, "reason": "good brand"})
    )
    result = llm.triage(_listing())
    assert result.promising is True


def test_triage_error_passes_through(monkeypatch):
    def boom(**kwargs):
        raise llm.ValuationError("bad json")

    monkeypatch.setattr(llm, "_complete_json", lambda model, messages: boom())
    result = llm.triage(_listing())
    assert result.promising is True  # fail-open: don't drop deals on triage errors


def test_full_valuation_parses_and_sends_photos(monkeypatch):
    captured = {}

    def completion(**kwargs):
        captured.update(kwargs)
        payload = {
            "est_resale_low": 400,
            "est_resale_high": 600,
            "expected_days_to_sell": 14,
            "max_buy_price": 250,
            "confidence": 0.8,
            "risk_flags": ["untested"],
            "resale_channel": "eBay sold comps",
            "rationale": "Mitutoyo tools hold value.",
            "outreach_draft": "Hi, is this still available?",
        }
        message = SimpleNamespace(content=json.dumps(payload))
        return SimpleNamespace(choices=[SimpleNamespace(message=message)])

    monkeypatch.setattr(llm.litellm, "completion", completion)
    result = llm.full_valuation(_listing())

    assert result.max_buy_price == Decimal("250")
    user_content = captured["messages"][1]["content"]
    image_parts = [p for p in user_content if p["type"] == "image_url"]
    assert image_parts == [{"type": "image_url", "image_url": {"url": "https://img.example/1.jpg"}}]
