// Package server assembles the event-ingestor's two HTTP surfaces: the
// JWT-gated admin plane (:9023, nginx-routed) and the compose-internal
// health/metrics port (:9024, never routed).
package server

import (
	"net/http"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/api_admin"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/store"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/authclient"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/cors"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/obs"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/suigraphql"
)

type Config struct {
	Environment      string `toml:"environment"`
	DatabaseURL      string `toml:"database_url"`
	AdminBindAddr    string `toml:"admin_bind_addr"`
	InternalBindAddr string `toml:"internal_bind_addr"`
	SuiGraphqlURL    string `toml:"sui_graphql_url"`
	LeaderboardURL   string `toml:"leaderboard_url"`
	AuthServiceURL   string `toml:"auth_service_url"`

	PollIntervalMs      int `toml:"poll_interval_ms"`
	RetryBaseMs         int `toml:"retry_base_ms"`
	RetryCapMs          int `toml:"retry_cap_ms"`
	BackfillPagesPerSec int `toml:"backfill_pages_per_sec"`
}

// NewAdmin builds the :9023 mux. GET /health stays open (gatus); every other
// route sits behind RequireAuth. CORS wraps outside auth so browser
// preflights are never 401'd.
func NewAdmin(st *store.Store, gql *suigraphql.Client, auth *authclient.Client) http.Handler {
	api := http.NewServeMux()
	api_admin.Mount(api, st, gql)

	mux := http.NewServeMux()
	mux.HandleFunc("GET /health", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("content-type", "text/plain; charset=utf-8")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("ok"))
	})
	mux.Handle("/", authclient.RequireAuth(auth)(api))
	return cors.Wrap(mux)
}

// NewInternal builds the :9024 mux (health + metrics only).
func NewInternal() *http.ServeMux {
	mux := http.NewServeMux()
	obs.MountHealthAndMetrics(mux)
	return mux
}
