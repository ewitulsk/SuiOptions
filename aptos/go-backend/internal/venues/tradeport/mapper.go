// Package tradeport maps Tradeport v1 (`listings`) and v2 (`listings_v2`)
// events. Unlike reference-shape venues, Tradeport emits Buy events with
// no royalty/commission split and v1 TokenIds inline, so it gets its own
// mapper. Quote is always APT (Coin, 8 decimals) on both versions.
package tradeport

import (
	"strings"

	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues/reference"
)

const (
	slugV1 = "tradeport"
	slugV2 = "tradeport-v2"
)

// Mapper maps one Tradeport contract package (v1 and v2 live together).
type Mapper struct {
	address string
}

// New returns a Mapper for the Tradeport package address.
func New(address string) *Mapper {
	return &Mapper{address: strings.ToLower(address)}
}

func (m *Mapper) Marketplace() string     { return slugV2 }
func (m *Mapper) ContractAddress() string { return m.address }

// Map extracts activities from Tradeport v1/v2 events in tx.
func (m *Mapper) Map(tx venues.Transaction) ([]venues.Activity, error) {
	if !tx.Success {
		return nil, nil
	}
	var out []venues.Activity
	for _, e := range tx.Events {
		addr, module, name, ok := venues.SplitType(e.Type)
		if !ok || !strings.EqualFold(addr, m.address) {
			continue
		}
		var a venues.Activity
		var mapped bool
		switch module + "::" + name {
		case "listings::BuyEvent":
			a, mapped = m.mapV1Buy(tx, e)
		case "listings_v2::BuyEvent":
			a, mapped = mapV2Buy(tx, e)
		case "listings_v2::InsertListingEvent":
			a, mapped = mapV2Listing(tx, e, venues.KindCreated)
		case "listings_v2::DeleteListingEvent":
			a, mapped = mapV2Listing(tx, e, venues.KindCancelled)
		}
		if mapped {
			out = append(out, a)
		}
	}
	return out, nil
}

func base(tx venues.Transaction, e venues.Event, market, kind string) venues.Activity {
	return venues.Activity{
		Version:      tx.Version,
		EventIndex:   e.SequenceNumber,
		TimestampUs:  tx.TimestampMicros,
		Marketplace:  market,
		Kind:         kind,
		RawEventType: e.Type,
		QuoteToken:   "0xa",
	}
}

func strOf(data map[string]any, key string) string {
	v, _ := data[key].(string)
	return venues.NormalizeAddr(v)
}

// mapV1Buy maps listings::BuyEvent {buyer, owner, price, timestamp, token_id}.
func (m *Mapper) mapV1Buy(tx venues.Transaction, e venues.Event) (venues.Activity, bool) {
	a := base(tx, e, slugV1, venues.KindFilled)
	a.Buyer = strOf(e.Data, "buyer")
	a.Seller = strOf(e.Data, "owner")
	if p, ok := venues.ParseU64(e.Data["price"]); ok {
		a.Price = &p
	} else {
		return a, false
	}
	tid, ok := e.Data["token_id"].(map[string]any)
	if !ok {
		return a, false
	}
	inner, ok := tid["token_data_id"].(map[string]any)
	if !ok {
		return a, false
	}
	creator, _ := inner["creator"].(string)
	collection, _ := inner["collection"].(string)
	name, _ := inner["name"].(string)
	if creator == "" || collection == "" || name == "" {
		return a, false
	}
	a.Creator = venues.NormalizeAddr(creator)
	a.Collection = collection
	a.TokenName = name
	if v, ok := venues.ParseU64(tid["property_version"]); ok {
		a.PropertyVer = &v
	}
	a.TokenDataID = reference.TokenDataIDv1(a.Creator, collection, name)
	a.ListingID = a.TokenDataID
	return a, true
}

// mapV2Buy maps listings_v2::BuyEvent {buyer, seller, price, listing{inner}, token{inner}}.
func mapV2Buy(tx venues.Transaction, e venues.Event) (venues.Activity, bool) {
	a := base(tx, e, slugV2, venues.KindFilled)
	a.Buyer = strOf(e.Data, "buyer")
	a.Seller = strOf(e.Data, "seller")
	if p, ok := venues.ParseU64(e.Data["price"]); ok {
		a.Price = &p
	} else {
		return a, false
	}
	if l, ok := e.Data["listing"].(map[string]any); ok {
		a.ListingID = venues.NormalizeAddr(strOf(l, "inner"))
	} else {
		return a, false
	}
	if t, ok := e.Data["token"].(map[string]any); ok {
		a.TokenDataID = venues.NormalizeAddr(strOf(t, "inner"))
	}
	return a, true
}

// mapV2Listing maps Insert/DeleteListingEvent. Shape: {listing{inner},
// seller, price, ...}; unknown extra fields are ignored.
func mapV2Listing(tx venues.Transaction, e venues.Event, kind string) (venues.Activity, bool) {
	a := base(tx, e, slugV2, kind)
	if l, ok := e.Data["listing"].(map[string]any); ok {
		a.ListingID = venues.NormalizeAddr(strOf(l, "inner"))
	} else {
		return a, false
	}
	a.Seller = strOf(e.Data, "seller")
	if p, ok := venues.ParseU64(e.Data["price"]); ok {
		a.Price = &p
	}
	return a, true
}
