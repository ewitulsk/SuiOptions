from decimal import Decimal

from app.db.models import Listing


def _make_listing(db) -> Listing:
    listing = Listing(
        source="ebay",
        external_id="v1|123|0",
        url="https://ebay.example/item/123",
        title="Machinist tools lot",
        price=Decimal("80.00"),
        photos=[],
    )
    db.add(listing)
    db.commit()
    return listing


def test_full_deal_lifecycle_computes_net_profit(auth_client, db):
    listing = _make_listing(db)

    resp = auth_client.post("/api/deals", json={"listing_id": listing.id})
    assert resp.status_code == 201, resp.text
    deal = resp.json()
    assert deal["title"] == "Machinist tools lot"
    assert deal["status"] == "watching"
    assert deal["net_profit"] is None

    resp = auth_client.post(
        f"/api/deals/{deal['id']}/bought",
        json={"buy_price": "80.00", "buy_extra_costs": "10.00"},
    )
    assert resp.status_code == 200, resp.text
    bought = resp.json()
    assert bought["status"] == "bought"
    assert bought["bought_by"] is not None
    assert bought["net_profit"] is None  # not sold yet

    resp = auth_client.post(
        f"/api/deals/{deal['id']}/sold",
        json={"sale_price": "500.00", "sale_fees": "25.00", "sale_channel": "eBay"},
    )
    assert resp.status_code == 200, resp.text
    sold = resp.json()
    assert sold["status"] == "sold"
    # 500 - 25 - 80 - 10 = 385, computed by the database
    assert Decimal(sold["net_profit"]) == Decimal("385.00")


def test_sold_requires_bought_first(auth_client, db):
    listing = _make_listing(db)
    deal = auth_client.post("/api/deals", json={"listing_id": listing.id}).json()
    resp = auth_client.post(f"/api/deals/{deal['id']}/sold", json={"sale_price": "500.00"})
    assert resp.status_code == 422


def test_patch_fixes_typo(auth_client, db):
    listing = _make_listing(db)
    deal = auth_client.post("/api/deals", json={"listing_id": listing.id}).json()
    auth_client.post(f"/api/deals/{deal['id']}/bought", json={"buy_price": "800.00"})
    resp = auth_client.patch(f"/api/deals/{deal['id']}", json={"buy_price": "80.00"})
    assert resp.status_code == 200
    assert Decimal(resp.json()["buy_price"]) == Decimal("80.00")


def test_manual_deal_requires_title(auth_client):
    assert auth_client.post("/api/deals", json={}).status_code == 422
    resp = auth_client.post("/api/deals", json={"title": "Garage sale find"})
    assert resp.status_code == 201


def test_one_deal_per_listing(auth_client, db):
    listing = _make_listing(db)
    assert auth_client.post("/api/deals", json={"listing_id": listing.id}).status_code == 201
    assert auth_client.post("/api/deals", json={"listing_id": listing.id}).status_code == 409


def test_stats(auth_client, db):
    listing = _make_listing(db)
    deal = auth_client.post("/api/deals", json={"listing_id": listing.id}).json()
    auth_client.post(
        f"/api/deals/{deal['id']}/bought", json={"buy_price": "80.00", "buy_extra_costs": "10.00"}
    )
    auth_client.post(
        f"/api/deals/{deal['id']}/sold", json={"sale_price": "500.00", "sale_fees": "25.00"}
    )
    # a second deal still tied up
    open_deal = auth_client.post("/api/deals", json={"title": "Another find"}).json()
    auth_client.post(f"/api/deals/{open_deal['id']}/bought", json={"buy_price": "40.00"})

    stats = auth_client.get("/api/deals/stats").json()
    assert Decimal(stats["realized_profit_all_time"]) == Decimal("385.00")
    assert Decimal(stats["realized_profit_30d"]) == Decimal("385.00")
    assert Decimal(stats["capital_tied_up"]) == Decimal("40.00")
    assert stats["deals_sold"] == 1
    assert stats["win_rate"] == 1.0
    assert stats["per_user"][0]["username"] == "admin"
    assert stats["per_user"][0]["deals_bought"] == 2
