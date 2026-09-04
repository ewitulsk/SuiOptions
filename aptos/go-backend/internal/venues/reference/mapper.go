// Package reference maps the reference-shape venue family: our venue,
// Wapal, Rarible and Topaz v2. All four compile the same marketplace
// modules, so one mapper serves every contract address — the Wapal mapper
// with a different address is literally our mapper.
//
// Event-name compatibility: our venue emits `...::events::ListingPlacedEvent`
// (reference names); Rarible emits `...::events::ListingPlaced` (no suffix).
// Matching is suffix-based so both work without per-venue code.
package reference

import (
	"fmt"
	"strings"

	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues"
)

// Mapper maps one reference-shape contract address.
type Mapper struct {
	marketplace string
	address     string
}

// New returns a Mapper for marketplace slug at contract address.
func New(marketplace, address string) *Mapper {
	return &Mapper{marketplace: marketplace, address: strings.ToLower(address)}
}

func (m *Mapper) Marketplace() string     { return m.marketplace }
func (m *Mapper) ContractAddress() string { return m.address }

// Map extracts activities from reference-shape events in tx.
func (m *Mapper) Map(tx venues.Transaction) ([]venues.Activity, error) {
	if !tx.Success {
		return nil, nil
	}
	var out []venues.Activity
	for _, e := range tx.Events {
		addr, module, name, ok := venues.SplitType(e.Type)
		if !ok || !strings.EqualFold(addr, m.address) || module != "events" {
			continue
		}
		a, ok := mapEvent(m.marketplace, tx, e, name)
		if ok {
			out = append(out, a)
		}
	}
	return out, nil
}

func mapEvent(market string, tx venues.Transaction, e venues.Event, name string) (venues.Activity, bool) {
	base := strings.TrimSuffix(name, "Event")
	a := venues.Activity{
		Version:      tx.Version,
		EventIndex:   e.SequenceNumber,
		TimestampUs:  tx.TimestampMicros,
		Marketplace:  market,
		RawEventType: e.Type,
	}
	str := func(key string) string {
		v, _ := e.Data[key].(string)
		return venues.NormalizeAddr(v)
	}
	u64p := func(key string) *uint64 {
		v, ok := venues.ParseU64(e.Data[key])
		if !ok {
			return nil
		}
		return &v
	}
	switch base {
	case "ListingPlaced":
		a.Kind = venues.KindCreated
		a.ListingID = str("listing")
		a.Price = u64p("price")
		a.QuoteToken = str("quote")
		a.Seller = str("seller")
		fillTokenMeta(&a, e.Data["token_metadata"])
	case "ListingCanceled":
		a.Kind = venues.KindCancelled
		a.ListingID = str("listing")
		a.Price = u64p("price")
		a.QuoteToken = str("quote")
		a.Seller = str("seller")
		fillTokenMeta(&a, e.Data["token_metadata"])
	case "ListingFilled":
		a.Kind = venues.KindFilled
		a.ListingID = str("listing")
		a.Price = u64p("price")
		a.QuoteToken = str("quote")
		a.Seller = str("seller")
		a.Buyer = str("purchaser")
		a.Commission = u64p("commission")
		a.Royalty = u64p("royalties")
		fillTokenMeta(&a, e.Data["token_metadata"])
	case "TokenOfferPlaced", "CollectionOfferPlaced":
		a.Kind = venues.KindOffer
		if id := str("token_offer"); id != "" {
			a.ListingID = id
		} else {
			a.ListingID = str("collection_offer")
		}
		a.Price = u64p("price")
		a.QuoteToken = str("quote")
		a.Buyer = str("purchaser")
		fillTokenMeta(&a, e.Data["token_metadata"])
		fillCollectionMeta(&a, e.Data["collection_metadata"])
	case "TokenOfferCanceled", "CollectionOfferCanceled":
		a.Kind = venues.KindCancelled
		if id := str("token_offer"); id != "" {
			a.ListingID = id
		} else {
			a.ListingID = str("collection_offer")
		}
		a.Price = u64p("price")
		a.QuoteToken = str("quote")
		a.Buyer = str("purchaser")
		a.Remaining = u64p("remaining_token_amount")
		fillTokenMeta(&a, e.Data["token_metadata"])
		fillCollectionMeta(&a, e.Data["collection_metadata"])
	case "TokenOfferFilled", "CollectionOfferFilled":
		a.Kind = venues.KindFilled
		if id := str("token_offer"); id != "" {
			a.ListingID = id
		} else {
			a.ListingID = str("collection_offer")
		}
		a.Price = u64p("price")
		a.QuoteToken = str("quote")
		a.Buyer = str("purchaser")
		a.Seller = str("seller")
		a.Commission = u64p("commission")
		a.Royalty = u64p("royalties")
		fillTokenMeta(&a, e.Data["token_metadata"])
	default:
		return a, false
	}
	return a, true
}

// fillTokenMeta decodes the reference TokenMetadata struct. A non-empty
// `token.vec` means TokenV2 (object address is the id); otherwise TokenV1
// (id is the TokenDataId hash — see TokenDataIDv1).
func fillTokenMeta(a *venues.Activity, raw any) {
	m, ok := raw.(map[string]any)
	if !ok {
		return
	}
	strAt := func(key string) string {
		v, _ := m[key].(string)
		return v
	}
	a.Creator = venues.NormalizeAddr(strAt("creator_address"))
	a.Collection = strAt("collection_name")
	a.TokenName = strAt("token_name")
	if v, ok := venues.ParseU64(venues.FirstVec(m["property_version"])); ok {
		pv := v
		a.PropertyVer = &pv
	}
	if tok := venues.AddrInVec(m["token"]); tok != "" {
		a.TokenDataID = tok
		return
	}
	if a.Creator != "" && a.Collection != "" && a.TokenName != "" {
		a.TokenDataID = TokenDataIDv1(a.Creator, a.Collection, a.TokenName)
	}
}

func fillCollectionMeta(a *venues.Activity, raw any) {
	m, ok := raw.(map[string]any)
	if !ok {
		return
	}
	if v, _ := m["creator_address"].(string); v != "" && a.Creator == "" {
		a.Creator = venues.NormalizeAddr(v)
	}
	if v, _ := m["collection_name"].(string); v != "" && a.Collection == "" {
		a.Collection = v
	}
}

// TokenDataIDv1 derives the v1 TokenDataId hash: sha3-256 over the BCS
// encoding of (creator_address, collection_name, token_name). Verified
// against the reference token module semantics; the Phase 1 gate
// re-validates one id per venue against the hosted indexer.
func TokenDataIDv1(creator, collection, name string) string {
	h := venues.Sha3New()
	h.Write(venues.MustHex32(creator))
	h.Write(venues.BcsStr(collection))
	h.Write(venues.BcsStr(name))
	return "0x" + fmt.Sprintf("%x", h.Sum(nil))
}

