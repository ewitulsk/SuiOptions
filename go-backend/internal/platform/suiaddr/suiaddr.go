// Package suiaddr normalizes Sui addresses and Move type tags for
// comparison — the Go port of auth-service's allowlist.rs plus the
// canonical-struct-tag helper the ingestor needs to match event types.
package suiaddr

import (
	"fmt"
	"strings"
)

// Normalize renders addr in the canonical form used for storage and
// comparison: lowercase, 0x-prefixed, zero-padded to 64 hex chars. Mirrors
// how Sui itself renders addresses. Accepts bare hex, short or long, any
// case. Does NOT validate hex-ness — pair with ValidHex where input is
// untrusted.
func Normalize(addr string) string {
	s := strings.TrimSpace(addr)
	s = strings.TrimPrefix(s, "0x")
	s = strings.TrimPrefix(s, "0X")
	return fmt.Sprintf("0x%064s", strings.ToLower(s))
}

// ValidHex reports whether s (with optional 0x) is 1..64 hex chars — the
// shape GraphQL event senders and address-typed Move fields render as
// (short forms allowed; leading zeros are frequently trimmed).
func ValidHex(addr string) bool {
	s := strings.TrimSpace(addr)
	s = strings.TrimPrefix(s, "0x")
	s = strings.TrimPrefix(s, "0X")
	if len(s) == 0 || len(s) > 64 {
		return false
	}
	for _, c := range s {
		if !((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F')) {
			return false
		}
	}
	return true
}

// CanonicalType reduces a Move struct-tag repr (`0x<pkg>::module::Name`,
// possibly with generic args and any address padding/case) to a single
// comparable form: `0x<64-padded-lowercase>::module::Name`, generics
// stripped.
//
// The trap this exists for: GraphQL `contents { type { repr } }` renders the
// package address fully padded while a rule configured from introspection
// may carry the short form (or vice versa) — raw string equality misses.
func CanonicalType(typeRepr string) string {
	s := strings.TrimSpace(typeRepr)
	// Strip generic arguments: `0x2::coin::Coin<0x2::sui::SUI>` → head only.
	if i := strings.IndexByte(s, '<'); i >= 0 {
		s = s[:i]
	}
	parts := strings.SplitN(s, "::", 3)
	if len(parts) != 3 {
		// Not a struct tag; best effort lowercase so comparisons stay sane.
		return strings.ToLower(s)
	}
	return Normalize(parts[0]) + "::" + parts[1] + "::" + parts[2]
}
