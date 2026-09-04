package reference

import (
	"testing"

	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues"
)

// Real mainnet Rarible ListingPlaced, version 2417694028 (REST archival).
func TestRaribleListingPlaced(t *testing.T) {
	tx, err := venues.ParseRESTFile("../testdata/2417694028.json")
	if err != nil {
		t.Fatal(err)
	}
	m := New("rarible", venues.AddrRarible)
	acts, err := m.Map(tx)
	if err != nil {
		t.Fatal(err)
	}
	if len(acts) != 1 {
		t.Fatalf("got %d activities, want 1", len(acts))
	}
	a := acts[0]
	if a.Kind != venues.KindCreated {
		t.Errorf("kind = %q, want created", a.Kind)
	}
	if a.Price == nil || *a.Price != 300000000 {
		t.Errorf("price = %v, want 300000000", a.Price)
	}
	if a.Seller != "0x692889e587c5862a6002b1c8528802e59833db793fe0c82df27a5d957ff564d2" {
		t.Errorf("seller = %s", a.Seller)
	}
	if a.Collection != "Buddy Byte" || a.TokenName != "Buddy Byte #8864" {
		t.Errorf("meta = %q %q", a.Collection, a.TokenName)
	}
	if a.TokenDataID != "0x36aae482232ecf097e2a754b42d09058bec16439ab12d4678e72cd566168c819" {
		t.Errorf("token id = %s", a.TokenDataID)
	}
	if a.ListingID != "0x1b36c8401b5d4b7954b3eca1acfbf5228a69145e8f4a9c2d8c1b3f5abb5a9e5e" {
		t.Errorf("listing = %s", a.ListingID)
	}
	if a.QuoteToken != "" {
		t.Errorf("quote = %q, want empty (Coin venue predates quote field)", a.QuoteToken)
	}
}

// A mapper for another address must ignore foreign events.
func TestAddressIsolation(t *testing.T) {
	tx, err := venues.ParseRESTFile("../testdata/2417694028.json")
	if err != nil {
		t.Fatal(err)
	}
	m := New("wapal", venues.AddrWapal)
	acts, err := m.Map(tx)
	if err != nil {
		t.Fatal(err)
	}
	if len(acts) != 0 {
		t.Fatalf("got %d activities, want 0", len(acts))
	}
}
