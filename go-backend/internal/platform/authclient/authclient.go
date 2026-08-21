// Package authclient gates admin endpoints on auth-service's internal
// /verify route — the same trust model token-info uses. The JWT itself never
// leaves the request; only the bearer string is POSTed to auth-service, so
// the secret stays in exactly one service.
package authclient

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
	"time"
)

// VerifiedClaims are the claims auth-service confirms for a valid token.
type VerifiedClaims struct {
	// Address is the Sui address the token was issued to, 0x-prefixed.
	Address string
	// Expiry, unix seconds.
	Exp int64
}

type verifyResp struct {
	Valid   bool    `json:"valid"`
	Address *string `json:"address"`
	Exp     *int64  `json:"exp"`
}

// Client POSTs to auth-service's internal verify port.
type Client struct {
	baseURL string
	http    *http.Client
}

func New(baseURL string) *Client {
	return &Client{
		baseURL: strings.TrimRight(baseURL, "/"),
		http:    &http.Client{Timeout: 5 * time.Second},
	}
}

// Verify asks auth-service whether token is a currently-valid admin JWT.
// Returns (claims, nil) when valid, (nil, nil) when auth-service reports it
// invalid, and an error only on transport/upstream failure — callers must
// fail CLOSED on error.
func (c *Client) Verify(ctx context.Context, token string) (*VerifiedClaims, error) {
	body, err := json.Marshal(map[string]string{"token": token})
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.baseURL+"/verify", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("content-type", "application/json")
	resp, err := c.http.Do(req)
	if err != nil {
		return nil, fmt.Errorf("auth-service unreachable: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("auth-service /verify → %s", resp.Status)
	}
	var vr verifyResp
	if err := json.NewDecoder(resp.Body).Decode(&vr); err != nil {
		return nil, fmt.Errorf("auth-service /verify body: %w", err)
	}
	if !vr.Valid {
		return nil, nil
	}
	out := &VerifiedClaims{}
	if vr.Address != nil {
		out.Address = *vr.Address
	}
	if vr.Exp != nil {
		out.Exp = *vr.Exp
	}
	return out, nil
}

type ctxKey int

const addressKey ctxKey = iota

// AddressFromContext returns the verified admin address set by RequireAuth.
func AddressFromContext(ctx context.Context) string {
	a, _ := ctx.Value(addressKey).(string)
	return a
}

// RequireAuth middleware: 401 on a missing/invalid token, 502 fail-closed
// when auth-service itself is unreachable. The verified address lands in the
// request context (AddressFromContext).
func RequireAuth(c *Client) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			token := bearer(r.Header.Get("Authorization"))
			if token == "" {
				http.Error(w, `{"error":"missing bearer token"}`, http.StatusUnauthorized)
				return
			}
			claims, err := c.Verify(r.Context(), token)
			if err != nil {
				// Transport/upstream failure is NOT an auth rejection — but
				// it must never open the gate either. Fail closed with 502.
				http.Error(w, `{"error":"auth verification unavailable"}`, http.StatusBadGateway)
				return
			}
			if claims == nil {
				http.Error(w, `{"error":"invalid or expired token"}`, http.StatusUnauthorized)
				return
			}
			next.ServeHTTP(w, r.WithContext(context.WithValue(r.Context(), addressKey, claims.Address)))
		})
	}
}

func bearer(header string) string {
	raw := strings.TrimSpace(header)
	for _, prefix := range []string{"Bearer ", "bearer "} {
		if strings.HasPrefix(raw, prefix) {
			return strings.TrimSpace(strings.TrimPrefix(raw, prefix))
		}
	}
	return ""
}
