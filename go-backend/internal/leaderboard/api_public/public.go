// Package api_public is the read-only public API on :9021, nginx-routed at
// /{env}/leaderboard/…. No auth — every response is aggregate or
// address-derived public data.
package api_public

import (
	"encoding/json"
	"errors"
	"net/http"
	"strconv"
	"time"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/leaderboard/service"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/leaderboard/store"
)

type API struct {
	svc *service.Service
}

func New(svc *service.Service) *API { return &API{svc: svc} }

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("content-type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func errorJSON(w http.ResponseWriter, status int, msg string) {
	writeJSON(w, status, map[string]string{"error": msg})
}

// Mount registers the public routes on mux.
func Mount(mux *http.ServeMux, svc *service.Service) {
	a := &API{svc: svc}
	mux.HandleFunc("GET /leaderboard", a.getLeaderboard)
	mux.HandleFunc("GET /rank/{wallet}", a.getRank)
	mux.HandleFunc("GET /account/{wallet}/breakdown", a.getBreakdown)
	mux.HandleFunc("GET /sources", a.getSources)
}

var validWindows = map[string]bool{"all": true, "30d": true, "7d": true, "24h": true}

func windowParam(r *http.Request) (string, bool) {
	win := r.URL.Query().Get("window")
	if win == "" {
		return "all", true
	}
	return win, validWindows[win]
}

func clampInt(raw string, def, max int) int {
	if raw == "" {
		return def
	}
	n, err := strconv.Atoi(raw)
	if err != nil || n < 0 {
		return def
	}
	if n > max {
		return max
	}
	return n
}

type leaderboardResp struct {
	Window        string        `json:"window"`
	Source        string        `json:"source"`
	AsOfMs        int64         `json:"as_of_ms"`
	TotalAccounts int64         `json:"total_accounts"`
	Limit         int           `json:"limit"`
	Offset        int           `json:"offset"`
	Entries       []store.Entry `json:"entries"`
}

func (a *API) getLeaderboard(w http.ResponseWriter, r *http.Request) {
	window, ok := windowParam(r)
	if !ok {
		errorJSON(w, 400, "invalid window (all|30d|7d|24h)")
		return
	}
	source := r.URL.Query().Get("source")
	limit := clampInt(r.URL.Query().Get("limit"), 50, 100)
	offset := clampInt(r.URL.Query().Get("offset"), 0, 1<<30)

	entries, total, err := a.svc.Store().Leaderboard(r.Context(), window, source, limit, offset)
	if err != nil {
		errorJSON(w, 500, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, leaderboardResp{
		Window:        window,
		Source:        source,
		AsOfMs:        time.Now().UnixMilli(),
		TotalAccounts: total,
		Limit:         limit,
		Offset:        offset,
		Entries:       entries,
	})
}

type rankResp struct {
	Rank          *int64        `json:"rank"`
	Points        int64         `json:"points"`
	AccountID     int64         `json:"account_id"`
	Wallets       []string      `json:"wallets"`
	Twitter       *string       `json:"twitter"`
	Neighbors     []store.Entry `json:"neighbors"`
	TotalAccounts int64         `json:"total_accounts"`
}

func (a *API) getRank(w http.ResponseWriter, r *http.Request) {
	wallet := r.PathValue("wallet")
	window, ok := windowParam(r)
	if !ok {
		errorJSON(w, 400, "invalid window (all|30d|7d|24h)")
		return
	}
	source := r.URL.Query().Get("source")
	radius := clampInt(r.URL.Query().Get("radius"), 5, 25)

	identifier, err := service.NormalizeIdentifier(service.IdentityWallet, wallet)
	if err != nil {
		errorJSON(w, 400, err.Error())
		return
	}

	entries, total, err := a.svc.Store().RankOf(r.Context(), identifier, window, source, radius)
	if errors.Is(err, store.ErrNotFound) {
		errorJSON(w, 404, "no points found for this wallet in the selected range")
		return
	}
	if err != nil {
		errorJSON(w, 500, err.Error())
		return
	}

	resp := rankResp{Neighbors: entries, TotalAccounts: total, Wallets: []string{}}
	// The target is whichever neighbor entry carries the resolved wallet
	// identity (RankOf returns neighbors including the target itself).
	for _, e := range entries {
		for _, wl := range e.Wallets {
			if wl == identifier {
				rank := e.Rank
				resp.Rank = &rank
				resp.Points = e.Points
				resp.AccountID = e.AccountID
				resp.Wallets = e.Wallets
				resp.Twitter = e.Twitter
			}
		}
	}
	if resp.Rank == nil && len(entries) > 0 {
		// Identity attach raced; still return the first neighbor's data.
		e := entries[0]
		rank := e.Rank
		resp.Rank = &rank
		resp.Points = e.Points
		resp.AccountID = e.AccountID
		resp.Wallets = e.Wallets
		resp.Twitter = e.Twitter
	}
	writeJSON(w, http.StatusOK, resp)
}

type breakdownResp struct {
	AccountID int64                `json:"account_id"`
	Total     int64                `json:"total"`
	BySource  []store.BreakdownRow `json:"by_source"`
}

func (a *API) getBreakdown(w http.ResponseWriter, r *http.Request) {
	wallet := r.PathValue("wallet")
	window, ok := windowParam(r)
	if !ok {
		errorJSON(w, 400, "invalid window (all|30d|7d|24h)")
		return
	}
	identifier, err := service.NormalizeIdentifier(service.IdentityWallet, wallet)
	if err != nil {
		errorJSON(w, 400, err.Error())
		return
	}
	accountID, err := a.svc.Store().AccountByWallet(r.Context(), identifier)
	if errors.Is(err, store.ErrNotFound) {
		errorJSON(w, 404, "unknown wallet")
		return
	}
	if err != nil {
		errorJSON(w, 500, err.Error())
		return
	}
	total, err := a.svc.Store().AccountPoints(r.Context(), accountID, window, "")
	if err != nil {
		errorJSON(w, 500, err.Error())
		return
	}
	rows, err := a.svc.Store().Breakdown(r.Context(), accountID, window)
	if err != nil {
		errorJSON(w, 500, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, breakdownResp{AccountID: accountID, Total: total, BySource: rows})
}

type sourcesResp struct {
	Sources []store.SourceRow `json:"sources"`
}

func (a *API) getSources(w http.ResponseWriter, r *http.Request) {
	rows, err := a.svc.Store().Sources(r.Context())
	if err != nil {
		errorJSON(w, 500, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, sourcesResp{Sources: rows})
}
