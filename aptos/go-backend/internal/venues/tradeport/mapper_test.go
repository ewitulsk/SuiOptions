package tradeport

import (
	"testing"

	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues"
)

// Real mainnet Tradeport v1 Buy, version 2382221134 (REST archival).
func TestV1Buy(t *testing.T) {
	tx, err := venues.ParseRESTFile("../testdata/2382221134.json")
	if err != nil {
		t.Fatal(err)
	}
	m := New(venues.AddrTradeport)
	acts, err := m.Map(tx)
	if err != nil {
		t.Fatal(err)
	}
	if len(acts) != 2 {
		t.Fatalf("got %d activities, want 2", len(acts))
	}
	a := acts[0]
	if a.Kind != venues.KindFilled || a.Marketplace != "tradeport" {
		t.Errorf("kind/market = %q %q", a.Kind, a.Marketplace)
	}
	if a.Price == nil || *a.Price != 398000000 {
		t.Errorf("price = %v, want 398000000", a.Price)
	}
	if a.Buyer != "0x735507953f702ddad6dbf5a98de6fd3f57f50b89da9c68672414d8431f103726" {
		t.Errorf("buyer = %s", a.Buyer)
	}
	if a.Collection != "Bruh Bears" || a.TokenName != "Bruh Bear #3428" {
		t.Errorf("meta = %q %q", a.Collection, a.TokenName)
	}
	if a.PropertyVer == nil || *a.PropertyVer != 1 {
		t.Errorf("pv = %v", a.PropertyVer)
	}
	if len(a.TokenDataID) != 66 {
		t.Errorf("token id = %q, want 0x+64hex", a.TokenDataID)
	}
	if a.QuoteToken != "0xa" {
		t.Errorf("quote = %q, want 0xa", a.QuoteToken)
	}
}

// Real mainnet Tradeport v2 bulk buy, version 2386455218: 4 BuyEvents.
func TestV2BulkBuy(t *testing.T) {
	tx, err := venues.ParseRESTFile("../testdata/2386455218.json")
	if err != nil {
		t.Fatal(err)
	}
	m := New(venues.AddrTradeport)
	acts, err := m.Map(tx)
	if err != nil {
		t.Fatal(err)
	}
	if len(acts) != 4 {
		t.Fatalf("got %d activities, want 4", len(acts))
	}
	want := []uint64{164860000, 164900000, 165000000, 166240000}
	for i, a := range acts {
		if a.Marketplace != "tradeport-v2" || a.Kind != venues.KindFilled {
			t.Errorf("[%d] kind/market = %q %q", i, a.Kind, a.Marketplace)
		}
		if a.Price == nil || *a.Price != want[i] {
			t.Errorf("[%d] price = %v, want %d", i, a.Price, want[i])
		}
		if len(a.ListingID) != 66 || len(a.TokenDataID) != 66 {
			t.Errorf("[%d] ids = %q %q", i, a.ListingID, a.TokenDataID)
		}
	}
}
