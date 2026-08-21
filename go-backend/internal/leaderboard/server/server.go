// Package server assembles the leaderboard's two HTTP surfaces:
// public read (:9021) and the compose-internal write API (:9022).
package server

import (
	"net/http"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/leaderboard/api_internal"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/leaderboard/api_public"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/leaderboard/service"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/obs"
)

type Config struct {
	Environment      string `toml:"environment"`
	DatabaseURL      string `toml:"database_url"`
	PublicBindAddr   string `toml:"public_bind_addr"`
	InternalBindAddr string `toml:"internal_bind_addr"`
}

// NewPublic builds the :9021 mux (read-only + health/metrics).
func NewPublic(svc *service.Service) *http.ServeMux {
	mux := http.NewServeMux()
	api_public.Mount(mux, svc)
	obs.MountHealthAndMetrics(mux)
	return mux
}

// NewInternal builds the :9022 mux (internal writes + health/metrics).
func NewInternal(svc *service.Service) *http.ServeMux {
	mux := http.NewServeMux()
	api_internal.Mount(mux, svc)
	obs.MountHealthAndMetrics(mux)
	return mux
}
