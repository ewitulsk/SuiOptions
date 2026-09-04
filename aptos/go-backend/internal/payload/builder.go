// Package payload builds wallet-ready transaction payloads: tier 0 direct
// venue calls and tier 1 router sweeps. Payloads are returned as JSON the
// frontend submits via the wallet adapter; every payload is simulated
// against the fullnode before it leaves the API.
package payload

import (
	"fmt"
	"strings"
)

// Entry is a wallet-submittable entry-function payload.
type Entry struct {
	Function      string   `json:"function"`
	TypeArguments []string `json:"type_arguments"`
	Arguments     []any    `json:"arguments"`
}

// Venue ids mirror router::router (1..7).
const (
	VenueWapal       = 1
	VenueRarible     = 2
	VenueTopazV2     = 3
	VenueBluemoveV2  = 4
	VenueTradeportV2 = 5
	VenueTradeportV1 = 6
	VenueOKX         = 7
)

// venueEntrypoint maps a venue + standard to its direct-buy function.
func venueEntrypoint(venue int, standard string) (function string, typeArgs []string, err error) {
	aptosCoin := []string{"0x1::aptos_coin::AptosCoin"}
	switch venue {
	case VenueWapal:
		return "0x584b50b999c78ade62f8359c91b5165ff390338d45f8e55969a04e65d76258c9::coin_listing::purchase", aptosCoin, nil
	case VenueRarible:
		return "0x465a0051e8535859d4794f0af24dbf35c5349bedadab26404b20b825035ee790::coin_listing::purchase", aptosCoin, nil
	case VenueTopazV2:
		return "0x6de37368e31dff4580b211295198159ee6f98b42ffa93c5683bb955ca1be67e0::coin_listing::purchase", aptosCoin, nil
	case VenueBluemoveV2:
		return "0x0d520d8669b0a3de23119898dcdff3e0a27910db247663646ad18cf16e44c6f5::coin_listing::purchase", aptosCoin, nil
	case VenueTradeportV2:
		return "0xe11c12ec495f3989c35e1c6a0af414451223305b579291fc8f3d9d0575a23c26::listings_v2::buy_token", nil, nil
	case VenueTradeportV1:
		return "0xe11c12ec495f3989c35e1c6a0af414451223305b579291fc8f3d9d0575a23c26::listings::buy_token", nil, nil
	case VenueOKX:
		return "0x1e6009ce9d288f3d5031c06ca0b19a334214ead798a0cb38808485bd6d997a43::okx_fixed_price::buy_direct_listing", aptosCoin, nil
	}
	return "", nil, fmt.Errorf("payload: unknown venue %d", venue)
}

// Buy builds the tier-0 direct-buy payload for one listing. Args after the
// listing depend on the venue (see router::buy_many for the matrix).
func Buy(venue int, standard string, args ...any) (Entry, error) {
	fn, tyArgs, err := venueEntrypoint(venue, standard)
	if err != nil {
		return Entry{}, err
	}
	if tyArgs == nil {
		tyArgs = []string{}
	}
	return Entry{Function: fn, TypeArguments: tyArgs, Arguments: args}, nil
}

// RouterSweep builds the tier-1 router::buy_many payload. Lengths must
// match; v1 vectors are empty unless a Tradeport v1 item is included.
// config is the on-chain RouterConfig object address (empty until the
// router package is deployed; the API reports it via /status).
func RouterSweep(routerPackage, config string, venues []int, listings []string, prices []string,
	v1creators, v1collections, v1names, v1pvs []string) (Entry, error) {
	n := len(venues)
	if len(listings) != n || len(prices) != n || len(v1creators) != n ||
		len(v1collections) != n || len(v1names) != n || len(v1pvs) != n {
		return Entry{}, fmt.Errorf("payload: vector length mismatch")
	}
	if !strings.HasPrefix(config, "0x") {
		return Entry{}, fmt.Errorf("payload: router config not deployed yet")
	}
	vs := make([]any, n)
	for i, v := range venues {
		vs[i] = v
	}
	toAny := func(in []string) []any {
		out := make([]any, len(in))
		for i, s := range in {
			out[i] = s
		}
		return out
	}
	return Entry{
		Function:      routerPackage + "::router::buy_many",
		TypeArguments: []string{},
		Arguments: []any{
			config, vs, toAny(listings), toAny(prices),
			toAny(v1creators), toAny(v1collections), toAny(v1names), toAny(v1pvs),
		},
	}, nil
}
