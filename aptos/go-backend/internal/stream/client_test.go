package stream

import (
	"net/http"
	"testing"
)

func TestApplyKey(t *testing.T) {
	anon := New("https://fullnode.mainnet.aptoslabs.com/v1")
	req, _ := http.NewRequest(http.MethodGet, "https://fullnode.mainnet.aptoslabs.com/v1/transactions?start=0&limit=100", nil)
	anon.ApplyKey(req)
	if h := req.Header.Get("Authorization"); h != "" {
		t.Fatalf("anonymous client set auth: %q", h)
	}
	if v := req.URL.Query().Get("api_key"); v != "" {
		t.Fatalf("anonymous client set api_key: %q", v)
	}

	keyed := NewWithKey("https://fullnode.mainnet.aptoslabs.com/v1", "k")
	req2, _ := http.NewRequest(http.MethodGet, "https://fullnode.mainnet.aptoslabs.com/v1/transactions?start=0&limit=100", nil)
	keyed.ApplyKey(req2)
	if h := req2.Header.Get("Authorization"); h != "Bearer k" {
		t.Fatalf("bearer = %q", h)
	}
	if v := req2.URL.Query().Get("api_key"); v != "k" {
		t.Fatalf("api_key = %q", v)
	}
	// start/limit survive the query rewrite.
	if v := req2.URL.Query().Get("start"); v != "0" {
		t.Fatalf("start lost: %q", v)
	}
}
