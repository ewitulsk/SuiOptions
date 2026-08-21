package extract

import (
	"encoding/json"
	"errors"
	"testing"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/store"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/suigraphql"
)

func strptr(s string) *string { return &s }

func fieldRule(path string) *store.Rule {
	return &store.Rule{ID: 1, RecipientMode: "field", RecipientField: strptr(path)}
}

func event(contents string) suigraphql.ChainEvent {
	return suigraphql.ChainEvent{
		TxDigest: "digest",
		Sender:   "0xabc",
		JSON:     json.RawMessage(contents),
	}
}

func TestRecipientSender(t *testing.T) {
	ident, err := Recipient(event(`{}`), &store.Rule{ID: 1, RecipientMode: "sender"})
	if err != nil {
		t.Fatalf("sender mode: %v", err)
	}
	if ident.Type != "wallet" {
		t.Fatalf("identity type = %q", ident.Type)
	}
	// Short-form sender must come back canonical (0x + 64 hex).
	if len(ident.Identifier) != 66 {
		t.Fatalf("sender not normalized: %q", ident.Identifier)
	}
}

func TestRecipientSenderInvalid(t *testing.T) {
	ev := event(`{}`)
	ev.Sender = "not-hex"
	if _, err := Recipient(ev, &store.Rule{ID: 1, RecipientMode: "sender"}); !errors.Is(err, ErrUnresolvable) {
		t.Fatalf("expected ErrUnresolvable, got %v", err)
	}
}

func TestRecipientFieldTopLevel(t *testing.T) {
	// Address leaves may render short-form without 0x (GraphQL JSON trap).
	ident, err := Recipient(event(`{"writer":"abc123"}`), fieldRule("writer"))
	if err != nil {
		t.Fatalf("field mode: %v", err)
	}
	want := "0x0000000000000000000000000000000000000000000000000000000000abc123"
	if ident.Identifier != want {
		t.Fatalf("identifier = %q, want %q", ident.Identifier, want)
	}
}

func TestRecipientFieldNested(t *testing.T) {
	// Nested structs arrive unwrapped: plain key navigation.
	ident, err := Recipient(event(`{"meta":{"creator":"0xff"}}`), fieldRule("meta.creator"))
	if err != nil {
		t.Fatalf("nested field: %v", err)
	}
	if ident.Identifier[:2] != "0x" || len(ident.Identifier) != 66 {
		t.Fatalf("identifier not canonical: %q", ident.Identifier)
	}
}

func TestRecipientFieldVariant(t *testing.T) {
	// Enum variants live under literal "@variant"-style keys.
	if _, err := Recipient(event(`{"phase":{"@active":{"who":"0x1"}}}`), fieldRule("phase.@active.who")); err != nil {
		t.Fatalf("variant segment: %v", err)
	}
}

func TestRecipientFieldMissing(t *testing.T) {
	if _, err := Recipient(event(`{"a":1}`), fieldRule("nope")); !errors.Is(err, ErrUnresolvable) {
		t.Fatalf("expected ErrUnresolvable for missing key, got %v", err)
	}
}

func TestRecipientFieldNotAddress(t *testing.T) {
	// Base64 vector<u8> leaves and numbers are rejected, never attributed.
	for _, contents := range []string{`{"f":"aGVsbG8gd29ybGQhIQ=="}`, `{"f":42}`, `{"f":{"x":1}}`} {
		if _, err := Recipient(event(contents), fieldRule("f")); !errors.Is(err, ErrUnresolvable) {
			t.Fatalf("contents %s: expected ErrUnresolvable, got %v", contents, err)
		}
	}
}

func TestRecipientBalanceValueHop(t *testing.T) {
	// Balance/Supply wrap their value: the admin writes the ".value" hop.
	// An address-typed leaf under it still resolves.
	if _, err := Recipient(event(`{"vault":{"value":"0x2"}}`), fieldRule("vault.value")); err != nil {
		t.Fatalf("balance hop: %v", err)
	}
}
