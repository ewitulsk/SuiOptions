// Package extract resolves a delivery recipient from an event node.
//
// GraphQL `contents { json }` renders Move values differently from the old
// JSON-RPC shape (docs/sui-json-rpc-migration.md) — the traps that matter
// here: nested structs are unwrapped (plain keys, no {type,fields} wrapper),
// enum variants live under literal "@variant" keys, Balance/Supply wrap
// their value in {"value": …} (the admin writes that hop into the dotted
// path), UID/ID render as bare strings, and address leaves may be short-form
// without 0x. vector<u8> renders as base64 — those are rejected outright.
package extract

import (
	"encoding/json"
	"fmt"
	"strings"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/lbclient"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/store"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/suiaddr"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/suigraphql"
)

// ErrUnresolvable marks a recipient we could not pull out of the event.
// Callers count it, log it, and skip — one bad event must never stall its
// module's cursor.
var ErrUnresolvable = fmt.Errorf("recipient unresolvable")

// Recipient resolves the destination identity for rule against ev.
// mode=sender → the transaction sender; mode=field → dotted path over
// contents.json. The returned Identity is always wallet-typed (a Sui
// address); linking to twitter/discord identities is future work.
func Recipient(ev suigraphql.ChainEvent, rule *store.Rule) (lbclient.Identity, error) {
	switch rule.RecipientMode {
	case "sender":
		if !suiaddr.ValidHex(ev.Sender) || ev.Sender == "" {
			return lbclient.Identity{}, fmt.Errorf("%w: bad sender %q", ErrUnresolvable, ev.Sender)
		}
		return lbclient.Identity{Type: "wallet", Identifier: suiaddr.Normalize(ev.Sender)}, nil

	case "field":
		field := ""
		if rule.RecipientField != nil {
			field = *rule.RecipientField
		}
		if field == "" {
			return lbclient.Identity{}, fmt.Errorf("%w: rule %d has empty recipient_field", ErrUnresolvable, rule.ID)
		}
		var doc any
		if err := json.Unmarshal(ev.JSON, &doc); err != nil {
			return lbclient.Identity{}, fmt.Errorf("%w: contents.json: %v", ErrUnresolvable, err)
		}
		raw, err := walkPath(doc, field)
		if err != nil {
			return lbclient.Identity{}, err
		}
		s, ok := raw.(string)
		if !ok {
			return lbclient.Identity{}, fmt.Errorf("%w: field %q is not an address string", ErrUnresolvable, field)
		}
		if !suiaddr.ValidHex(s) {
			// Covers numbers-as-address and base64 vector<u8> leaves alike:
			// anything that isn't 1..64 hex chars is not an address we can
			// attribute.
			return lbclient.Identity{}, fmt.Errorf("%w: field %q value is not a Sui address", ErrUnresolvable, field)
		}
		return lbclient.Identity{Type: "wallet", Identifier: suiaddr.Normalize(s)}, nil

	default:
		return lbclient.Identity{}, fmt.Errorf("rule %d: unknown recipient_mode %q", rule.ID, rule.RecipientMode)
	}
}

// walkPath descends a dotted path ("writer" / "meta.creator" /
// "phase.@variant") through nested maps. Nested structs arrive unwrapped in
// GraphQL JSON, so plain key navigation is all that's needed; "@…" variant
// segments work because they're literal keys too.
func walkPath(doc any, dotted string) (any, error) {
	cur := doc
	for _, seg := range strings.Split(dotted, ".") {
		obj, ok := cur.(map[string]any)
		if !ok {
			return nil, fmt.Errorf("%w: segment %q of %q: parent is not an object", ErrUnresolvable, seg, dotted)
		}
		v, ok := obj[seg]
		if !ok {
			return nil, fmt.Errorf("%w: no key %q in %q", ErrUnresolvable, seg, dotted)
		}
		cur = v
	}
	return cur, nil
}
