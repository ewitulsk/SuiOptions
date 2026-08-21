// Package api_internal is the compose-network write API on :9022.
// Unauthenticated by design — the internal port is never nginx-routed (same
// trust model as token-info's 9006 / auth-service's 9008).
package api_internal

import (
	"encoding/json"
	"errors"
	"net/http"
	"time"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/leaderboard/service"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/leaderboard/store"
)

type API struct {
	svc *service.Service
}

func New(svc *service.Service) *API { return &API{svc: svc} }

type identityDTO struct {
	Type       string `json:"type"`
	Identifier string `json:"identifier"`
}

type pointsReq struct {
	Identity       identityDTO `json:"identity"`
	Delta          int64       `json:"delta"`
	Source         string      `json:"source"`
	SourceLabel    string      `json:"source_label"`
	EventType      string      `json:"event_type"`
	IdempotencyKey string      `json:"idempotency_key"`
	OccurredAt     *time.Time  `json:"occurred_at"`
}

type pointsResp struct {
	Applied bool `json:"applied"`
}

type linkReq struct {
	A identityDTO `json:"a"`
	B identityDTO `json:"b"`
}

type linkResp struct {
	AccountID int64 `json:"account_id"`
	Merged    bool  `json:"merged"`
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("content-type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func badRequest(w http.ResponseWriter, msg string) {
	writeJSON(w, http.StatusBadRequest, map[string]string{"error": msg})
}

// Mount registers the internal routes on mux.
func Mount(mux *http.ServeMux, svc *service.Service) {
	a := &API{svc: svc}
	mux.HandleFunc("POST /internal/points", a.postPoints)
	mux.HandleFunc("POST /internal/link", a.postLink)
}

func (a *API) postPoints(w http.ResponseWriter, r *http.Request) {
	var req pointsReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		badRequest(w, "invalid JSON body")
		return
	}
	if !service.ValidIdentityType(req.Identity.Type) {
		badRequest(w, "identity.type must be wallet|twitter|discord")
		return
	}
	if req.Source == "" {
		badRequest(w, "source is required")
		return
	}
	write := store.PointsWrite{
		Identity: store.Identity{
			Type:       req.Identity.Type,
			Identifier: req.Identity.Identifier,
		},
		Delta:          req.Delta,
		Source:         req.Source,
		SourceLabel:    req.SourceLabel,
		EventType:      req.EventType,
		IdempotencyKey: req.IdempotencyKey,
	}
	if req.OccurredAt != nil {
		write.OccurredAt = *req.OccurredAt
	}
	applied, err := a.svc.AddPoints(r.Context(), write)
	if err != nil {
		var ve *service.ValidationError
		if errors.As(err, &ve) {
			badRequest(w, ve.Error())
			return
		}
		writeJSON(w, http.StatusInternalServerError, map[string]string{"error": err.Error()})
		return
	}
	writeJSON(w, http.StatusOK, pointsResp{Applied: applied})
}

func (a *API) postLink(w http.ResponseWriter, r *http.Request) {
	var req linkReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		badRequest(w, "invalid JSON body")
		return
	}
	if !service.ValidIdentityType(req.A.Type) || !service.ValidIdentityType(req.B.Type) {
		badRequest(w, "identity.type must be wallet|twitter|discord")
		return
	}
	res, err := a.svc.Link(r.Context(),
		store.Identity{Type: req.A.Type, Identifier: req.A.Identifier},
		store.Identity{Type: req.B.Type, Identifier: req.B.Identifier})
	if err != nil {
		var ve *service.ValidationError
		if errors.As(err, &ve) {
			badRequest(w, ve.Error())
			return
		}
		writeJSON(w, http.StatusInternalServerError, map[string]string{"error": err.Error()})
		return
	}
	writeJSON(w, http.StatusOK, linkResp{AccountID: res.AccountID, Merged: res.Merged})
}
