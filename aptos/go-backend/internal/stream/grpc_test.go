package stream

import (
	"strings"
	"testing"

	indexerv1 "github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/indexerpb/streamv1"
	txnv1 "github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/indexerpb/txnv1"
	tspb "github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/indexerpb/tspb"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues"
)

func grpcTestTx() *txnv1.Transaction {
	return &txnv1.Transaction{
		Timestamp: &tspb.Timestamp{Seconds: 1700000000, Nanos: 1234000},
		Version:   7076000001,
		Info:      &txnv1.TransactionInfo{Success: true},
		TxnData: &txnv1.Transaction_User{
			User: &txnv1.UserTransaction{
				Request: &txnv1.UserTransactionRequest{Sender: "0xABC"},
				Events: []*txnv1.Event{{
					SequenceNumber: 3,
					TypeStr:        "0x5b388f4c8f4957aae4bf29196bb67c22498c149bb114f7b350f5727fba91df44::events::ListingPlacedEvent",
					Data:           `{"listing":"0x1","price":"1000"}`,
				}},
			},
		},
	}
}

func TestMapProtoTransaction(t *testing.T) {
	tx, ok := mapProtoTransaction(grpcTestTx())
	if !ok {
		t.Fatal("user tx must map")
	}
	if tx.Version != 7076000001 {
		t.Fatalf("version = %d", tx.Version)
	}
	if tx.TimestampMicros != 1700000000*1e6+1234 {
		t.Fatalf("timestamp_us = %d", tx.TimestampMicros)
	}
	if tx.Sender != venues.NormalizeAddr("0xABC") {
		t.Fatalf("sender = %q", tx.Sender)
	}
	if !tx.Success {
		t.Fatal("success must propagate")
	}
	if len(tx.Events) != 1 {
		t.Fatalf("events = %d", len(tx.Events))
	}
	e := tx.Events[0]
	if e.SequenceNumber != 3 || e.Data["listing"] != "0x1" || e.Data["price"] != "1000" {
		t.Fatalf("event = %+v", e)
	}
	if _, _, _, ok := venues.SplitType(e.Type); !ok {
		t.Fatalf("type not mapper-shaped: %q", e.Type)
	}
}

func TestMapProtoSkipsNonUser(t *testing.T) {
	tx, ok := mapProtoTransaction(&txnv1.Transaction{Version: 9})
	if ok || tx.Version != 9 {
		t.Fatalf("block tx must pass version through unmappable: %+v %v", tx, ok)
	}
	if _, ok := mapProtoTransaction(grpcTestTx()); !ok {
		t.Fatal("sanity")
	}
	bad := grpcTestTx()
	bad.GetUser().Events[0].Data = "not json"
	tx, ok = mapProtoTransaction(bad)
	if !ok || len(tx.Events) != 0 {
		t.Fatalf("bad event data must be dropped: %+v", tx)
	}
}

func TestEventFilterCoversMappers(t *testing.T) {
	c := &GRPCClient{addrs: []string{
		"0xours", venues.AddrWapal, venues.AddrRarible,
		venues.AddrTopazV2, venues.AddrTradeport,
	}}
	f := c.eventFilter()
	or := f.GetLogicalOr()
	if or == nil || len(or.Filters) != 5 {
		t.Fatalf("filter must OR 5 venue filters: %v", f)
	}
	seen := map[string]string{}
	for _, b := range or.Filters {
		ef := b.GetApiFilter().GetEventFilter()
		seen[strings.ToLower(ef.GetStructType().GetAddress())] = ef.GetStructType().GetModule()
	}
	for _, a := range []string{"0xours", venues.AddrWapal, venues.AddrRarible, venues.AddrTopazV2} {
		if seen[a] != "events" {
			t.Fatalf("%s module = %q, want events", a, seen[a])
		}
	}
	if mod, ok := seen[venues.AddrTradeport]; !ok || mod != "" {
		t.Fatalf("tradeport filter = %q,%v, want open module", mod, ok)
	}
	_ = indexerv1.BooleanTransactionFilter{}
}
