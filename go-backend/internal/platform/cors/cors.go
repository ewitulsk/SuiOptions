// Package cors is the permissive-CORS middleware for browser-facing ports.
// Mirrors the Rust services' default `allowed_origins = ["*"]` posture
// (token-info router.rs): the public data is not origin-sensitive, and admin
// surfaces are gated by bearer JWT, not by origin.
package cors

import "net/http"

// Wrap adds permissive CORS headers and answers OPTIONS preflights directly.
// Must sit OUTSIDE any auth middleware so preflights are never 401'd.
func Wrap(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		h := w.Header()
		h.Set("Access-Control-Allow-Origin", "*")
		h.Set("Access-Control-Allow-Methods", "GET, POST, PATCH, DELETE, OPTIONS")
		h.Set("Access-Control-Allow-Headers", "Content-Type, Authorization")
		h.Set("Access-Control-Max-Age", "3600")
		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		next.ServeHTTP(w, r)
	})
}
