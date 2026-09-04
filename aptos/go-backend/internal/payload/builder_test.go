package payload

import (
	"strings"
	"testing"
)

func TestBuyWapal(t *testing.T) {
	e, err := Buy(VenueWapal, "v2", "0xabc", "500")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(e.Function, "584b50b999c78ade62f8359c91b5165ff390338d45f8e55969a04e65d76258c9::coin_listing::purchase") {
		t.Errorf("function = %s", e.Function)
	}
	if len(e.TypeArguments) != 1 || e.TypeArguments[0] != "0x1::aptos_coin::AptosCoin" {
		t.Errorf("type args = %v", e.TypeArguments)
	}
}

func TestBuyTradeportV1(t *testing.T) {
	e, err := Buy(VenueTradeportV1, "v1", "0xcreator", "Bruh Bears", "Bruh Bear #3428", "1")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasSuffix(e.Function, "::listings::buy_token") {
		t.Errorf("function = %s", e.Function)
	}
	if len(e.TypeArguments) != 0 {
		t.Errorf("type args = %v, want none", e.TypeArguments)
	}
}

func TestBuyUnknownVenue(t *testing.T) {
	if _, err := Buy(99, "v2"); err == nil {
		t.Error("want error for unknown venue")
	}
}

func TestRouterSweepLengths(t *testing.T) {
	_, err := RouterSweep("0x1::router", "0xcfg", []int{1}, []string{"0xl"},
		[]string{"1"}, []string{}, []string{}, []string{}, []string{})
	if err == nil {
		t.Error("want length-mismatch error")
	}
	e, err := RouterSweep("0x1::router", "0xcfg", []int{1}, []string{"0xl"},
		[]string{"1"}, []string{""}, []string{""}, []string{""}, []string{""})
	if err != nil {
		t.Fatal(err)
	}
	if e.Function != "0x1::router::router::buy_many" {
		t.Errorf("function = %s", e.Function)
	}
	if _, err := RouterSweep("0x1::router", "", []int{}, []string{}, []string{},
		[]string{}, []string{}, []string{}, []string{}); err == nil {
		t.Error("want undeployed-config error")
	}
}
