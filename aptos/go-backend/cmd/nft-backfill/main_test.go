package main

import (
	"testing"

	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues/reference"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues/tradeport"
)

func TestBuildDataReference(t *testing.T) {
	r := row{
		"listing": "0xAAA", "price": "1000", "quote": "0xA", "seller": "0xBBB",
		"tm_creator": "0xCCC", "tm_collection": "Col", "tm_token_name": "Tok",
		"tm_property_version": []any{"3"}, "tm_token": []any{},
	}
	data := buildData(r, "events", "ListingPlacedEvent")
	tx := venues.Transaction{Version: 7, TimestampMicros: 8, Success: true,
		Events: []venues.Event{{Type: "0x1::events::ListingPlacedEvent", SequenceNumber: 0, Data: data}}}
	m := reference.New("ours", "0x1")
	acts, err := m.Map(tx)
	if err != nil || len(acts) != 1 {
		t.Fatalf("acts=%v err=%v", acts, err)
	}
	a := acts[0]
	if a.Kind != venues.KindCreated || a.ListingID != "0xaaa" || a.Seller != "0xbbb" {
		t.Fatalf("activity=%+v", a)
	}
	if a.PropertyVer == nil || *a.PropertyVer != 3 {
		t.Fatalf("property ver=%+v", a)
	}
	if a.Creator != "0xccc" || a.Collection != "Col" || a.TokenName != "Tok" {
		t.Fatalf("meta=%+v", a)
	}
	if a.TokenDataID == "" {
		t.Fatal("v1 token data id must derive")
	}
}

func TestBuildDataTradeportV2(t *testing.T) {
	r := row{
		"buyer": "0xB1", "seller": "0xS1", "price": "500",
		"listing_inner": "0xL1", "token_inner": "0xT1",
	}
	data := buildData(r, "listings_v2", "BuyEvent")
	tx := venues.Transaction{Version: 9, TimestampMicros: 9, Success: true,
		Events: []venues.Event{{Type: "0xe11c12ec495f3989c35e1c6a0af414451223305b579291fc8f3d9d0575a23c26::listings_v2::BuyEvent", SequenceNumber: 1, Data: data}}}
	m := tradeport.New("0xe11c12ec495f3989c35e1c6a0af414451223305b579291fc8f3d9d0575a23c26")
	acts, err := m.Map(tx)
	if err != nil || len(acts) != 1 {
		t.Fatalf("acts=%v err=%v", acts, err)
	}
	a := acts[0]
	if a.Kind != venues.KindFilled || a.ListingID != "0xl1" || a.TokenDataID != "0xt1" {
		t.Fatalf("activity=%+v", a)
	}
}

func TestBuildDataUnknown(t *testing.T) {
	if buildData(row{}, "events", "Nope") == nil {
		t.Fatal("reference family always builds (mapper decides)")
	}
	if buildData(row{}, "listings", "Nope") != nil {
		t.Fatal("unknown tradeport event must be nil")
	}
}

func TestParseTimestamp(t *testing.T) {
	if ts, err := parseTimestamp("7076000001000000"); err != nil || ts != 7076000001000000 {
		t.Fatalf("micros=%d err=%v", ts, err)
	}
	if _, err := parseTimestamp("garbage"); err == nil {
		t.Fatal("garbage must fail fast")
	}
	if _, err := parseTimestamp(nil); err == nil {
		t.Fatal("missing must fail fast")
	}
}
