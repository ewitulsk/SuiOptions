// Command nft-api serves the public read API (listings, collections,
// status) plus transaction-payload builders, and a JWT-gated admin mux
// for venue/fee operations. Stateless: every instance serves everything.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"

	"github.com/jackc/pgx/v5/pgxpool"

	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/payload"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/platform/config"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/platform/cors"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/platform/obs"
)

type apiConfig struct {
	FullnodeURL   string `toml:"fullnode_url"`
	DatabaseURL   string `toml:"database_url"`
	BindAddr      string `toml:"bind_addr"`
	AdminBindAddr string `toml:"admin_bind_addr"`
	OurVenue      string `toml:"our_venue_address"`
	RouterPackage string `toml:"router_package"`
	RouterConfig  string `toml:"router_config"`
	JWTSecret     string `toml:"jwt_secret"`
	AdminToken    string `toml:"admin_token"`
}

type server struct {
	cfg  apiConfig
	pool *pgxpool.Pool
	http *http.Client
}

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	shutdown := obs.InitTracing(ctx, "nft-api")
	defer shutdown()

	path := os.Getenv("CONFIG_PATH")
	if path == "" {
		path = "config/nft.toml"
	}
	var base struct {
		FullnodeURL string `toml:"fullnode_url"`
		DatabaseURL string `toml:"database_url"`
	}
	if err := config.LoadTOML(path, &base); err != nil {
		log.Fatalf("api: config: %v", err)
	}
	cfg := apiConfig{
		FullnodeURL:   base.FullnodeURL,
		DatabaseURL:   base.DatabaseURL,
		BindAddr:      envOr("API_BIND", "127.0.0.1:8091"),
		AdminBindAddr: envOr("ADMIN_BIND", "127.0.0.1:8092"),
		OurVenue:      os.Getenv("OUR_VENUE_ADDRESS"),
		RouterPackage: os.Getenv("ROUTER_PACKAGE"),
		RouterConfig:  os.Getenv("ROUTER_CONFIG"),
		AdminToken:    os.Getenv("ADMIN_TOKEN"),
	}

	pool, err := pgxpool.New(ctx, cfg.DatabaseURL)
	if err != nil {
		log.Fatalf("api: db: %v", err)
	}
	defer pool.Close()

	s := &server{cfg: cfg, pool: pool, http: http.DefaultClient}

	pub := http.NewServeMux()
	obs.MountHealthAndMetrics(pub)
	pub.HandleFunc("GET /status", s.handleStatus)
	pub.HandleFunc("GET /listings", s.handleListings)
	pub.HandleFunc("GET /collections/{id}/items", s.handleCollectionItems)
	pub.HandleFunc("GET /items/{id}", s.handleItem)
	pub.HandleFunc("POST /tx/buy", s.handleTxBuy)
	pub.HandleFunc("POST /tx/sweep", s.handleTxSweep)

	adm := http.NewServeMux()
	obs.MountHealthAndMetrics(adm)
	adm.Handle("POST /admin/venues", s.requireAdmin(http.HandlerFunc(s.handleAdminVenues)))
	adm.Handle("POST /admin/fees", s.requireAdmin(http.HandlerFunc(s.handleAdminFees)))

	go func() {
		log.Printf("api: public on %s", cfg.BindAddr)
		if err := http.ListenAndServe(cfg.BindAddr, cors.Wrap(pub)); err != nil {
			log.Printf("api: public: %v", err)
		}
	}()
	log.Printf("api: admin on %s", cfg.AdminBindAddr)
	if err := http.ListenAndServe(cfg.AdminBindAddr, adm); err != nil {
		log.Fatalf("api: admin: %v", err)
	}
}

func envOr(k, d string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return d
}

func writeJSON(w http.ResponseWriter, code int, v any) {
	w.Header().Set("content-type", "application/json")
	w.WriteHeader(code)
	_ = json.NewEncoder(w).Encode(v)
}

// GET /status — pipeline cursor, venues, contract addresses.
func (s *server) handleStatus(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	var cursor int64
	_ = s.pool.QueryRow(ctx, `SELECT last_version FROM pipeline_progress WHERE name='indexer'`).Scan(&cursor)
	var live int64
	_ = s.pool.QueryRow(ctx, `SELECT count(*) FROM live_listings`).Scan(&live)
	writeJSON(w, 200, map[string]any{
		"indexer_cursor": cursor,
		"live_listings":  live,
		"our_venue":      s.cfg.OurVenue,
		"router_package": s.cfg.RouterPackage,
		"router_config":  s.cfg.RouterConfig,
		"venues":         []string{"ours", "wapal", "rarible", "topaz-v2", "tradeport", "tradeport-v2"},
	})
}

// GET /listings?marketplace=&collection=&seller=&limit=
func (s *server) handleListings(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	conds := []string{"1=1"}
	args := []any{}
	push := func(cond string, v any) {
		args = append(args, v)
		conds = append(conds, cond+"=$"+strconv.Itoa(len(args)))
	}
	if v := q.Get("marketplace"); v != "" {
		push("marketplace=", v)
	}
	if v := q.Get("collection"); v != "" {
		push("collection=", v)
	}
	if v := q.Get("seller"); v != "" {
		push("seller=", strings.ToLower(v))
	}
	limit := 50
	if v, err := strconv.Atoi(q.Get("limit")); err == nil && v > 0 && v <= 200 {
		limit = v
	}
	rows, err := s.pool.Query(r.Context(), `
		SELECT marketplace, listing_id, token_data_id, creator, collection,
		       token_name, property_ver, price, quote_token, seller, open_version
		FROM live_listings WHERE `+strings.Join(conds, " AND ")+`
		ORDER BY open_version DESC LIMIT `+strconv.Itoa(limit), args...)
	if err != nil {
		writeJSON(w, 500, map[string]string{"error": err.Error()})
		return
	}
	defer rows.Close()
	type listing struct {
		Marketplace string `json:"marketplace"`
		ListingID   string `json:"listing_id"`
		TokenDataID string `json:"token_data_id"`
		Creator     string `json:"creator"`
		Collection  string `json:"collection"`
		TokenName   string `json:"token_name"`
		PropertyVer *int64 `json:"property_version"`
		Price       int64  `json:"price"`
		QuoteToken  string `json:"quote_token"`
		Seller      string `json:"seller"`
		OpenVersion int64  `json:"open_version"`
	}
	out := []listing{}
	for rows.Next() {
		var l listing
		if err := rows.Scan(&l.Marketplace, &l.ListingID, &l.TokenDataID, &l.Creator,
			&l.Collection, &l.TokenName, &l.PropertyVer, &l.Price, &l.QuoteToken,
			&l.Seller, &l.OpenVersion); err != nil {
			break
		}
		out = append(out, l)
	}
	writeJSON(w, 200, out)
}

// GET /collections/{id}/items — items = live listings filtered by collection,
// plus provenance from activities.
func (s *server) handleCollectionItems(w http.ResponseWriter, r *http.Request) {
	r.URL.RawQuery += "&collection=" + r.PathValue("id")
	s.handleListings(w, r)
}

// GET /items/{id} — item detail: live listing (if any) + recent activities.
func (s *server) handleItem(w http.ResponseWriter, r *http.Request) {
	id := strings.ToLower(r.PathValue("id"))
	var listing any
	row := s.pool.QueryRow(r.Context(), `
		SELECT marketplace, listing_id, token_data_id, creator, collection,
		       token_name, property_ver, price, quote_token, seller, open_version
		FROM live_listings WHERE token_data_id=$1 OR listing_id=$1 LIMIT 1`, id)
	var mp, lid, tdid, creator, coll, tname, quote, seller string
	var pv *int64
	var price, openVer int64
	if err := row.Scan(&mp, &lid, &tdid, &creator, &coll, &tname, &pv, &price, &quote, &seller, &openVer); err != nil {
		listing = nil
	} else {
		listing = map[string]any{"marketplace": mp, "listing_id": lid, "token_data_id": tdid,
			"creator": creator, "collection": coll, "token_name": tname, "property_version": pv,
			"price": price, "quote_token": quote, "seller": seller, "open_version": openVer}
	}
	arows, _ := s.pool.Query(r.Context(), `
		SELECT marketplace, kind, price, buyer, seller, version, timestamp_us
		FROM activities WHERE token_data_id=$1 OR listing_id=$1
		ORDER BY version DESC, event_index DESC LIMIT 50`, id)
	type act struct {
		Marketplace string `json:"marketplace"`
		Kind        string `json:"kind"`
		Price       *int64 `json:"price"`
		Buyer       string `json:"buyer"`
		Seller      string `json:"seller"`
		Version     int64  `json:"version"`
		TimestampUs int64  `json:"timestamp_us"`
	}
	acts := []act{}
	if arows != nil {
		defer arows.Close()
		for arows.Next() {
			var x act
			if err := arows.Scan(&x.Marketplace, &x.Kind, &x.Price, &x.Buyer, &x.Seller, &x.Version, &x.TimestampUs); err != nil {
				break
			}
			acts = append(acts, x)
		}
	}
	writeJSON(w, 200, map[string]any{"listing": listing, "activities": acts})
}

// POST /tx/buy {"venue":1,"standard":"v2","args":[...]} — tier-0 payload,
// simulated before return.
func (s *server) handleTxBuy(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Venue    int    `json:"venue"`
		Standard string `json:"standard"`
		Args     []any  `json:"args"`
		Sender   string `json:"sender"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSON(w, 400, map[string]string{"error": err.Error()})
		return
	}
	entry, err := payload.Buy(req.Venue, req.Standard, req.Args...)
	if err != nil {
		writeJSON(w, 400, map[string]string{"error": err.Error()})
		return
	}
	if err := s.simulate(r.Context(), req.Sender, entry); err != nil {
		writeJSON(w, 422, map[string]string{"error": "simulation failed: " + err.Error()})
		return
	}
	writeJSON(w, 200, entry)
}

// POST /tx/sweep — tier-1 router payload, simulated before return.
func (s *server) handleTxSweep(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Venues    []int    `json:"venues"`
		Listings  []string `json:"listings"`
		Prices    []string `json:"prices"`
		V1C       []string `json:"v1_creators"`
		V1Coll    []string `json:"v1_collections"`
		V1Names   []string `json:"v1_names"`
		V1PVs     []string `json:"v1_property_versions"`
		Sender    string   `json:"sender"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSON(w, 400, map[string]string{"error": err.Error()})
		return
	}
	entry, err := payload.RouterSweep(s.cfg.RouterPackage, s.cfg.RouterConfig,
		req.Venues, req.Listings, req.Prices, req.V1C, req.V1Coll, req.V1Names, req.V1PVs)
	if err != nil {
		writeJSON(w, 400, map[string]string{"error": err.Error()})
		return
	}
	if err := s.simulate(r.Context(), req.Sender, entry); err != nil {
		writeJSON(w, 422, map[string]string{"error": "simulation failed: " + err.Error()})
		return
	}
	writeJSON(w, 200, entry)
}

// simulate runs the payload against the fullnode simulation endpoint.
func (s *server) simulate(ctx context.Context, sender string, e payload.Entry) error {
	body, _ := json.Marshal(map[string]any{
		"sender": sender, "sequence_number": "0", "max_gas_amount": "10000",
		"gas_unit_price": "100", "expiration_timestamp_secs": "9999999999",
		"payload": map[string]any{
			"type": "entry_function_payload", "function": e.Function,
			"type_arguments": e.TypeArguments, "arguments": e.Arguments,
		},
		"signature": map[string]string{
			"type": "ed25519_signature", "public_key": strings.Repeat("0", 64),
			"signature": "0x" + strings.Repeat("0", 128),
		},
	})
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		s.cfg.FullnodeURL+"/transactions/simulate", bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")
	resp, err := s.http.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	var out []struct {
		Success bool `json:"success"`
		VMStatus string `json:"vm_status"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&out); err != nil {
		return err
	}
	if len(out) == 0 || !out[0].Success {
		status := ""
		if len(out) > 0 {
			status = out[0].VMStatus
		}
		return fmt.Errorf("vm abort: %s", status)
	}
	return nil
}

// requireAdmin gates the admin mux on the shared bearer token (LAN-only
// bind + token, same posture as the options admin surfaces).
func (s *server) requireAdmin(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if s.cfg.AdminToken == "" {
			writeJSON(w, 503, map[string]string{"error": "admin disabled"})
			return
		}
		if r.Header.Get("Authorization") != "Bearer "+s.cfg.AdminToken {
			writeJSON(w, 401, map[string]string{"error": "unauthorized"})
			return
		}
		next.ServeHTTP(w, r)
	})
}

// Admin stubs return the exact entry payloads for venue/fee mutations so
// ops signs them with the multisig: never auto-submits, never holds keys.
func (s *server) handleAdminVenues(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Config string `json:"config"`
		Venue  int    `json:"venue"`
		Enable bool   `json:"enable"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSON(w, 400, map[string]string{"error": err.Error()})
		return
	}
	writeJSON(w, 200, payload.Entry{
		Function:      s.cfg.RouterPackage + "::router::set_venue_enabled",
		TypeArguments: []string{},
		Arguments:     []any{req.Config, fmt.Sprintf("%d", req.Venue), req.Enable},
	})
}

func (s *server) handleAdminFees(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Config string `json:"config"`
		FeeBps string `json:"fee_bps"`
		MinFee string `json:"min_fee"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSON(w, 400, map[string]string{"error": err.Error()})
		return
	}
	writeJSON(w, 200, payload.Entry{
		Function:      s.cfg.RouterPackage + "::router::set_fee",
		TypeArguments: []string{},
		Arguments:     []any{req.Config, req.FeeBps, req.MinFee},
	})
}
