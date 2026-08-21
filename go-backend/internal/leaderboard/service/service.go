// Package service is the leaderboard's business layer: identity
// normalization and thin orchestration over the store. Handlers never touch
// SQL directly.
package service

import (
	"context"
	"fmt"
	"strings"
	"time"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/leaderboard/store"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/suiaddr"
)

const (
	IdentityWallet  = "wallet"
	IdentityTwitter = "twitter"
	IdentityDiscord = "discord"
)

// ValidationError marks a 400-class problem (bad identity, bad params) as
// opposed to an internal failure.
type ValidationError struct{ msg string }

func (e *ValidationError) Error() string { return e.msg }

func invalidf(format string, args ...any) *ValidationError {
	return &ValidationError{msg: fmt.Sprintf(format, args...)}
}

// NormalizeIdentifier canonicalizes an identifier per its type so storage
// and lookup agree byte-for-byte. Wallets go through the allowlist.rs port;
// twitter handles are lowercased (@ stripped); discord ids pass through.
func NormalizeIdentifier(identityType, identifier string) (string, error) {
	switch identityType {
	case IdentityWallet:
		id := strings.TrimSpace(identifier)
		if !suiaddr.ValidHex(id) {
			return "", invalidf("invalid wallet address %q", identifier)
		}
		return suiaddr.Normalize(id), nil
	case IdentityTwitter:
		h := strings.TrimSpace(identifier)
		h = strings.TrimPrefix(h, "@")
		h = strings.ToLower(h)
		if h == "" || len(h) > 64 || strings.ContainsAny(h, "/ \t") {
			return "", invalidf("invalid twitter handle %q", identifier)
		}
		return h, nil
	case IdentityDiscord:
		d := strings.TrimSpace(identifier)
		if d == "" || len(d) > 128 {
			return "", invalidf("invalid discord id %q", identifier)
		}
		return d, nil
	default:
		return "", invalidf("unknown identity type %q", identityType)
	}
}

// ValidIdentityType reports whether t is a known identity type. New identity
// kinds extend the DB CHECK constraint and this switch together.
func ValidIdentityType(t string) bool {
	switch t {
	case IdentityWallet, IdentityTwitter, IdentityDiscord:
		return true
	}
	return false
}

// Service is the application core shared by both API surfaces.
type Service struct {
	store *store.Store
}

func New(st *store.Store) *Service { return &Service{store: st} }

// AddPoints normalizes the identity and applies one points write. Duplicate
// idempotency keys report applied=false (idempotent success). A negative
// delta is a removal.
func (s *Service) AddPoints(ctx context.Context, w store.PointsWrite) (bool, error) {
	identifier, err := NormalizeIdentifier(w.Identity.Type, w.Identity.Identifier)
	if err != nil {
		return false, err
	}
	w.Identity.Identifier = identifier
	if strings.TrimSpace(w.Source) == "" {
		return false, invalidf("source is required")
	}
	if w.OccurredAt.IsZero() {
		w.OccurredAt = time.Now().UTC()
	} else {
		w.OccurredAt = w.OccurredAt.UTC()
	}
	return s.store.AddPoints(ctx, w)
}

// Link normalizes both identities and runs the four-case link/merge:
// neither exists → one account with both; one exists → attach; different
// accounts → merge; same account → no-op.
func (s *Service) Link(ctx context.Context, a, b store.Identity) (store.LinkResult, error) {
	for _, ident := range []*store.Identity{&a, &b} {
		identifier, err := NormalizeIdentifier(ident.Type, ident.Identifier)
		if err != nil {
			return store.LinkResult{}, err
		}
		ident.Identifier = identifier
	}
	return s.store.Link(ctx, a, b)
}
