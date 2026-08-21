// Package api_admin is the JWT-gated admin plane on :9023, nginx-routed at
// /{env}/ingestor/…. Every route except /health sits behind RequireAuth
// (auth-service /verify); the verified address becomes created_by.
package api_admin

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/store"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/authclient"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/suiaddr"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/suigraphql"
)

type API struct {
	st  *store.Store
	gql *suigraphql.Client
}

func New(st *store.Store, gql *suigraphql.Client) *API { return &API{st: st, gql: gql} }

// Mount registers the admin routes on mux (auth is layered by the caller).
func Mount(mux *http.ServeMux, st *store.Store, gql *suigraphql.Client) {
	a := New(st, gql)
	mux.HandleFunc("POST /packages", a.addPackage)
	mux.HandleFunc("GET /packages", a.listPackages)
	mux.HandleFunc("DELETE /packages/{address}", a.deletePackage)
	mux.HandleFunc("POST /rules", a.createRule)
	mux.HandleFunc("GET /rules", a.listRules)
	mux.HandleFunc("PATCH /rules/{id}", a.patchRule)
	mux.HandleFunc("DELETE /rules/{id}", a.deleteRule)
	mux.HandleFunc("GET /status", a.status)
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("content-type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func errorJSON(w http.ResponseWriter, status int, msg string) {
	writeJSON(w, status, map[string]string{"error": msg})
}

// --- packages -----------------------------------------------------------------

type packageDTO struct {
	PackageAddress string          `json:"package_address"`
	Label          string          `json:"label"`
	Modules        json.RawMessage `json:"modules"`
	IntrospectedAt *time.Time      `json:"introspected_at"`
	CreatedBy      string          `json:"created_by"`
	CreatedAt      time.Time       `json:"created_at"`
}

func packageToDTO(p store.TrackedPackage) packageDTO {
	return packageDTO{
		PackageAddress: p.PackageAddress,
		Label:          p.Label,
		Modules:        p.ModulesJSON,
		IntrospectedAt: p.IntrospectedAt,
		CreatedBy:      p.CreatedBy,
		CreatedAt:      p.CreatedAt,
	}
}

// addPackage runs synchronous chain introspection and stores the result.
// Unknown addresses and packages without a single candidate-event struct are
// rejected 400 — there is nothing an event rule could ever match.
func (a *API) addPackage(w http.ResponseWriter, r *http.Request) {
	var req struct {
		PackageAddress string `json:"package_address"`
		Label          string `json:"label"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		errorJSON(w, http.StatusBadRequest, "invalid JSON body")
		return
	}
	if !suiaddr.ValidHex(req.PackageAddress) {
		errorJSON(w, http.StatusBadRequest, "package_address must be a Sui address")
		return
	}
	addr := suiaddr.Normalize(req.PackageAddress)

	ctx, cancel := context.WithTimeout(r.Context(), 60*time.Second)
	defer cancel()
	def, err := a.gql.IntrospectPackage(ctx, addr)
	if err != nil {
		errorJSON(w, http.StatusBadRequest, "introspection failed: "+err.Error())
		return
	}
	if len(def.CandidateEvents()) == 0 {
		errorJSON(w, http.StatusBadRequest, "package has no candidate event structs (copy+drop, no key)")
		return
	}
	modulesJSON, err := json.Marshal(def)
	if err != nil {
		errorJSON(w, http.StatusInternalServerError, err.Error())
		return
	}
	if err := a.st.UpsertPackage(r.Context(), addr, req.Label, authclient.AddressFromContext(r.Context()), modulesJSON); err != nil {
		errorJSON(w, http.StatusInternalServerError, err.Error())
		return
	}
	pkgs, err := a.st.ListPackages(r.Context())
	if err != nil {
		errorJSON(w, http.StatusInternalServerError, err.Error())
		return
	}
	for _, p := range pkgs {
		if p.PackageAddress == addr {
			writeJSON(w, http.StatusCreated, map[string]any{"package": packageToDTO(p)})
			return
		}
	}
	errorJSON(w, http.StatusInternalServerError, "package vanished after upsert")
}

// listPackages returns every tracked package with its introspection embedded
// (small payloads; keeps the admin UI to 3 queries).
func (a *API) listPackages(w http.ResponseWriter, r *http.Request) {
	pkgs, err := a.st.ListPackages(r.Context())
	if err != nil {
		errorJSON(w, http.StatusInternalServerError, err.Error())
		return
	}
	out := make([]packageDTO, 0, len(pkgs))
	for _, p := range pkgs {
		out = append(out, packageToDTO(p))
	}
	writeJSON(w, http.StatusOK, map[string]any{"packages": out})
}

func (a *API) deletePackage(w http.ResponseWriter, r *http.Request) {
	raw := r.PathValue("address")
	if !suiaddr.ValidHex(raw) {
		errorJSON(w, http.StatusBadRequest, "invalid package address")
		return
	}
	err := a.st.DeletePackage(r.Context(), suiaddr.Normalize(raw))
	if errors.Is(err, store.ErrNotFound) {
		errorJSON(w, http.StatusNotFound, "unknown package")
		return
	}
	if err != nil {
		errorJSON(w, http.StatusInternalServerError, err.Error())
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

// --- rules ---------------------------------------------------------------------

type ruleReq struct {
	PackageAddress string  `json:"package_address"`
	ModuleName     string  `json:"module_name"`
	EventType      string  `json:"event_type"`
	Label          string  `json:"label"`
	Points         int64   `json:"points"`
	RecipientMode  string  `json:"recipient_mode"`
	RecipientField *string `json:"recipient_field"`
	StartMode      string  `json:"start_mode"`
	StartAt        *string `json:"start_at"` // RFC3339
	Enabled        bool    `json:"enabled"`
}

// validateRuleShape enforces the invariants shared by create and patch:
// recipient_field present iff mode=field, start_at present iff
// mode=timestamp, and the event type resolving to a struct the package's
// cached introspection actually contains.
func (a *API) validateRuleShape(ctx context.Context, pkgAddr, moduleName, eventType, recipientMode string, recipientField *string, startMode string, startAt *time.Time) (string, error) {
	switch recipientMode {
	case "sender":
		if recipientField != nil && *recipientField != "" {
			return "", errors.New("recipient_field must be empty when recipient_mode=sender")
		}
	case "field":
		if recipientField == nil || strings.TrimSpace(*recipientField) == "" {
			return "", errors.New("recipient_field is required when recipient_mode=field")
		}
	default:
		return "", errors.New("recipient_mode must be sender|field")
	}
	switch startMode {
	case "tip":
		if startAt != nil {
			return "", errors.New("start_at must be empty when start_mode=tip")
		}
	case "timestamp":
		if startAt == nil {
			return "", errors.New("start_at is required when start_mode=timestamp")
		}
	default:
		return "", errors.New("start_mode must be tip|timestamp")
	}

	canonical := suiaddr.CanonicalType(eventType)
	parts := strings.SplitN(canonical, "::", 3)
	if len(parts) != 3 {
		return "", errors.New("event_type must be a full struct tag 0x<pkg>::module::Struct")
	}
	if parts[0] != suiaddr.Normalize(pkgAddr) {
		return "", errors.New("event_type package does not match package_address")
	}
	if parts[1] != moduleName {
		return "", errors.New("event_type module does not match module_name")
	}

	pkgs, err := a.st.ListPackages(ctx)
	if err != nil {
		return "", err
	}
	for _, p := range pkgs {
		if p.PackageAddress != suiaddr.Normalize(pkgAddr) {
			continue
		}
		var def suigraphql.PackageDef
		if err := json.Unmarshal(p.ModulesJSON, &def); err != nil {
			return "", errors.New("package introspection cache is corrupt; re-add the package")
		}
		if _, ok := def.FindStruct(moduleName, parts[2]); !ok {
			return "", errors.New("event_type not found in the package's introspection")
		}
		return canonical, nil
	}
	return "", errors.New("package is not tracked; add it first")
}

func parseStartAt(raw *string) (*time.Time, error) {
	if raw == nil || *raw == "" {
		return nil, nil
	}
	t, err := time.Parse(time.RFC3339, *raw)
	if err != nil {
		return nil, errors.New("start_at must be RFC3339")
	}
	u := t.UTC()
	return &u, nil
}

func (a *API) createRule(w http.ResponseWriter, r *http.Request) {
	var req ruleReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		errorJSON(w, http.StatusBadRequest, "invalid JSON body")
		return
	}
	if !suiaddr.ValidHex(req.PackageAddress) {
		errorJSON(w, http.StatusBadRequest, "package_address must be a Sui address")
		return
	}
	startAt, err := parseStartAt(req.StartAt)
	if err != nil {
		errorJSON(w, http.StatusBadRequest, err.Error())
		return
	}
	canonical, err := a.validateRuleShape(r.Context(), req.PackageAddress, req.ModuleName,
		req.EventType, req.RecipientMode, req.RecipientField, req.StartMode, startAt)
	if err != nil {
		errorJSON(w, http.StatusBadRequest, err.Error())
		return
	}

	rule, err := a.st.CreateRule(r.Context(), &store.Rule{
		PackageAddress: suiaddr.Normalize(req.PackageAddress),
		ModuleName:     req.ModuleName,
		EventType:      canonical,
		Label:          req.Label,
		Points:         req.Points,
		RecipientMode:  req.RecipientMode,
		RecipientField: req.RecipientField,
		StartMode:      req.StartMode,
		StartAt:        startAt,
		Enabled:        req.Enabled,
		CreatedBy:      authclient.AddressFromContext(r.Context()),
	})
	if err != nil {
		if strings.Contains(err.Error(), "duplicate key") {
			errorJSON(w, http.StatusConflict, "a rule for this event already exists")
			return
		}
		errorJSON(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusCreated, map[string]any{"rule": rule})
}

func (a *API) listRules(w http.ResponseWriter, r *http.Request) {
	pkg := r.URL.Query().Get("package")
	if pkg != "" {
		if !suiaddr.ValidHex(pkg) {
			errorJSON(w, http.StatusBadRequest, "invalid package filter")
			return
		}
		pkg = suiaddr.Normalize(pkg)
	}
	rules, err := a.st.ListRules(r.Context(), pkg)
	if err != nil {
		errorJSON(w, http.StatusInternalServerError, err.Error())
		return
	}
	if rules == nil {
		rules = []*store.Rule{}
	}
	writeJSON(w, http.StatusOK, map[string]any{"rules": rules})
}

type rulePatchReq struct {
	Label          *string `json:"label"`
	Points         *int64  `json:"points"`
	Enabled        *bool   `json:"enabled"`
	RecipientMode  *string `json:"recipient_mode"`
	RecipientField *string `json:"recipient_field"`
	StartMode      *string `json:"start_mode"`
	StartAt        *string `json:"start_at"`
}

func (a *API) patchRule(w http.ResponseWriter, r *http.Request) {
	id, err := strconv.ParseInt(r.PathValue("id"), 10, 64)
	if err != nil {
		errorJSON(w, http.StatusBadRequest, "invalid rule id")
		return
	}
	var req rulePatchReq
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		errorJSON(w, http.StatusBadRequest, "invalid JSON body")
		return
	}
	existing, err := a.st.GetRule(r.Context(), id)
	if errors.Is(err, store.ErrNotFound) {
		errorJSON(w, http.StatusNotFound, "unknown rule")
		return
	}
	if err != nil {
		errorJSON(w, http.StatusInternalServerError, err.Error())
		return
	}

	// Validate the EFFECTIVE post-patch shape, not just the delta.
	effMode := existing.RecipientMode
	if req.RecipientMode != nil {
		effMode = *req.RecipientMode
	}
	effField := existing.RecipientField
	if req.RecipientField != nil {
		effField = req.RecipientField
	}
	effStartMode := existing.StartMode
	if req.StartMode != nil {
		effStartMode = *req.StartMode
	}
	effStartAt := existing.StartAt
	startAt, err := parseStartAt(req.StartAt)
	if err != nil {
		errorJSON(w, http.StatusBadRequest, err.Error())
		return
	}
	if startAt != nil {
		effStartAt = startAt
	}
	if _, err := a.validateRuleShape(r.Context(), existing.PackageAddress, existing.ModuleName,
		existing.EventType, effMode, effField, effStartMode, effStartAt); err != nil {
		errorJSON(w, http.StatusBadRequest, err.Error())
		return
	}

	rule, err := a.st.PatchRule(r.Context(), id, store.RulePatch{
		Label:          req.Label,
		Points:         req.Points,
		Enabled:        req.Enabled,
		RecipientMode:  req.RecipientMode,
		RecipientField: req.RecipientField,
		StartMode:      req.StartMode,
		StartAt:        startAt,
	})
	if err != nil {
		errorJSON(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"rule": rule})
}

func (a *API) deleteRule(w http.ResponseWriter, r *http.Request) {
	id, err := strconv.ParseInt(r.PathValue("id"), 10, 64)
	if err != nil {
		errorJSON(w, http.StatusBadRequest, "invalid rule id")
		return
	}
	err = a.st.DeleteRule(r.Context(), id)
	if errors.Is(err, store.ErrNotFound) {
		errorJSON(w, http.StatusNotFound, "unknown rule")
		return
	}
	if err != nil {
		errorJSON(w, http.StatusInternalServerError, err.Error())
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

// --- status --------------------------------------------------------------------

func (a *API) status(w http.ResponseWriter, r *http.Request) {
	mods, rules, err := a.st.Status(r.Context())
	if err != nil {
		errorJSON(w, http.StatusInternalServerError, err.Error())
		return
	}
	if mods == nil {
		mods = []store.ModuleStatus{}
	}
	if rules == nil {
		rules = []store.RuleStatus{}
	}
	writeJSON(w, http.StatusOK, map[string]any{"modules": mods, "rules": rules})
}
