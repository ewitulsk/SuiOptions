// Command nft-backfill is the one-time historical importer for the NFT
// aggregator: it pages a Geomi no-code processor (template in
// aptos/deployment/geomi-backfill.yaml) over GraphQL, rebuilds the venue
// event shapes the REST/gRPC clients produce, and replays them through the
// production venue mappers into Postgres. Applies are idempotent
// (store.ApplyBatch), so reruns and overlap with the live indexer converge.
//
// Required env: GEOMI_GRAPHQL_URL, GEOMI_API_KEY, DATABASE_URL,
// OUR_VENUE_ADDRESS. Optional: STOP_VERSION (default: live indexer cursor),
// ALLOW_EMPTY=1 (proceed when Geomi returns zero rows).
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/store"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues/reference"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues/tradeport"
)

const table = "venue_events_backfill"

type row map[string]any

func getenv(key string) string {
	v := strings.TrimSpace(os.Getenv(key))
	if v == "" {
		log.Fatalf("backfill: missing env %s", key)
	}
	return v
}

// str normalizes a GraphQL scalar (string, json.Number, float64) to string.
func str(v any) string {
	switch t := v.(type) {
	case nil:
		return ""
	case string:
		return t
	case json.Number:
		return t.String()
	case float64:
		return strconv.FormatUint(uint64(t), 10)
	case bool:
		return strconv.FormatBool(t)
	default:
		return ""
	}
}

// vec wraps a GraphQL array column into the {"vec": [...]} shape the
// mappers' FirstVec/AddrInVec expect. Null stays null (= field absent).
func vec(v any) any {
	a, ok := v.([]any)
	if !ok {
		return nil
	}
	out := make([]any, 0, len(a))
	for _, e := range a {
		if s := str(e); s != "" {
			out = append(out, s)
		}
	}
	return map[string]any{"vec": out}
}

func nonempty(m map[string]any) map[string]any {
	out := map[string]any{}
	for k, v := range m {
		if v == nil {
			continue
		}
		if s, ok := v.(string); ok && s == "" {
			continue
		}
		out[k] = v
	}
	return out
}

// buildData reconstructs the event data map for one Geomi row, mirroring
// the REST shapes in venues/rest.go so mappers run unchanged.
func buildData(r row, module, name string) map[string]any {
	g := func(col string) string { return str(r[col]) }
	if module == "events" {
		tm := nonempty(map[string]any{
			"creator_address":  g("tm_creator"),
			"collection_name":  g("tm_collection"),
			"token_name":       g("tm_token_name"),
			"property_version": vec(r["tm_property_version"]),
			"token":            vec(r["tm_token"]),
		})
		cm := nonempty(map[string]any{
			"creator_address": g("cm_creator"),
			"collection_name": g("cm_collection"),
		})
		data := nonempty(map[string]any{
			"listing":                g("listing"),
			"price":                  g("price"),
			"quote":                  g("quote"),
			"seller":                 g("seller"),
			"purchaser":              g("purchaser"),
			"token_offer":            g("token_offer"),
			"collection_offer":       g("collection_offer"),
			"commission":             g("commission"),
			"royalties":              g("royalties"),
			"remaining_token_amount": g("remaining_token_amount"),
		})
		if len(tm) > 0 {
			data["token_metadata"] = tm
		}
		if len(cm) > 0 {
			data["collection_metadata"] = cm
		}
		return data
	}
	switch module + "::" + name {
	case "listings::BuyEvent":
		return nonempty(map[string]any{
			"buyer": g("buyer"), "owner": g("owner"), "price": g("price"),
			"token_id": map[string]any{
				"token_data_id": map[string]any{
					"creator": g("v1_creator"), "collection": g("v1_collection"), "name": g("v1_name"),
				},
				"property_version": g("v1_property_version"),
			},
		})
	case "listings_v2::BuyEvent":
		data := nonempty(map[string]any{
			"buyer": g("buyer"), "seller": g("seller"), "price": g("price"),
		})
		if g("listing_inner") != "" {
			data["listing"] = map[string]any{"inner": g("listing_inner")}
		}
		if g("token_inner") != "" {
			data["token"] = map[string]any{"inner": g("token_inner")}
		}
		return data
	case "listings_v2::InsertListingEvent", "listings_v2::DeleteListingEvent":
		data := nonempty(map[string]any{"seller": g("seller"), "price": g("price")})
		if g("listing_inner") != "" {
			data["listing"] = map[string]any{"inner": g("listing_inner")}
		}
		return data
	}
	return nil
}

// parseTimestamp accepts chain micros ("7076...") or RFC3339.
func parseTimestamp(v any) (uint64, error) {
	if s := str(v); s != "" {
		if n, err := strconv.ParseUint(s, 10, 64); err == nil {
			return n, nil
		}
		if t, err := time.Parse(time.RFC3339, s); err == nil {
			return uint64(t.UnixMicro()), nil
		}
		return 0, fmt.Errorf("unparseable timestamp %q", s)
	}
	return 0, fmt.Errorf("missing timestamp")
}

func u64env(key string) uint64 {
	if v := strings.TrimSpace(os.Getenv(key)); v != "" {
		n, err := strconv.ParseUint(v, 10, 64)
		if err != nil {
			log.Fatalf("backfill: bad %s: %v", key, err)
		}
		return n
	}
	return 0
}

const pageQuery = `query($after: String, $limit: Int) { ` + table +
	`(where: {version: {_gt: $after}}, order_by: [{version: asc}, {event_index: asc}], limit: $limit) {
    version event_index event_type timestamp_us
    listing price quote seller purchaser buyer owner token_offer collection_offer
    commission royalties remaining_token_amount
    tm_creator tm_collection tm_token_name tm_property_version tm_token
    cm_creator cm_collection
    v1_creator v1_collection v1_name v1_property_version
    listing_inner token_inner
  } }`

func fetchPage(url, key, after string) ([]row, error) {
	body, _ := json.Marshal(map[string]any{
		"query":     pageQuery,
		"variables": map[string]any{"after": after, "limit": 1000},
	})
	req, err := http.NewRequest(http.MethodPost, url, bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+key)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		slurp, _ := io.ReadAll(io.LimitReader(resp.Body, 1024))
		return nil, fmt.Errorf("backfill: graphql HTTP %d: %s", resp.StatusCode, slurp)
	}
	dec := json.NewDecoder(resp.Body)
	dec.UseNumber()
	var out struct {
		Data   map[string][]row `json:"data"`
		Errors []any            `json:"errors"`
	}
	if err := dec.Decode(&out); err != nil {
		return nil, fmt.Errorf("backfill: decode: %w", err)
	}
	if len(out.Errors) > 0 {
		return nil, fmt.Errorf("backfill: graphql: %v", out.Errors)
	}
	return out.Data[table], nil
}

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	url := getenv("GEOMI_GRAPHQL_URL")
	key := getenv("GEOMI_API_KEY")
	ourVenue := getenv("OUR_VENUE_ADDRESS")

	st, err := store.Open(ctx, getenv("DATABASE_URL"))
	if err != nil {
		log.Fatalf("backfill: store: %v", err)
	}
	defer st.Close()

	stopVersion := u64env("STOP_VERSION")
	if stopVersion == 0 {
		stopVersion, err = st.Cursor(ctx, "indexer")
		if err != nil {
			log.Fatalf("backfill: cursor: %v", err)
		}
		log.Printf("backfill: stop version = live cursor %d", stopVersion)
	}

	mappers := []venues.Mapper{
		reference.New("ours", ourVenue),
		reference.New("wapal", venues.AddrWapal),
		reference.New("rarible", venues.AddrRarible),
		reference.New("topaz-v2", venues.AddrTopazV2),
		tradeport.New(venues.AddrTradeport),
	}

	var (
		after      = "0"
		versions   uint64
		rows       uint64
		acts       = map[string]uint64{}
		skipped    uint64
		lastLogged uint64
	)
	for {
		select {
		case <-ctx.Done():
			return
		default:
		}
		page, err := fetchPage(url, key, after)
		if err != nil {
			log.Fatalf("backfill: fetch after %s: %v", after, err)
		}
		if len(page) == 0 {
			break
		}
		// Group rows into transactions in version order.
		var cur *venues.Transaction
		flush := func() error {
			if cur == nil {
				return nil
			}
			var mapped []venues.Activity
			for _, m := range mappers {
				got, err := m.Map(*cur)
				if err != nil {
					return fmt.Errorf("mapper %s: %w", m.Marketplace(), err)
				}
				mapped = append(mapped, got...)
			}
			if err := st.ApplyBatch(ctx, "indexer", mapped, cur.Version); err != nil {
				return err
			}
			for _, a := range mapped {
				acts[a.Marketplace+":"+a.Kind]++
			}
			versions++
			if versions-lastLogged >= 100 {
				log.Printf("backfill: %d versions, %d rows", versions, rows)
				lastLogged = versions
			}
			cur = nil
			return nil
		}
		for _, r := range page {
			vstr := str(r["version"])
			v, err := strconv.ParseUint(vstr, 10, 64)
			if err != nil {
				log.Fatalf("backfill: bad version %q", vstr)
			}
			after = vstr
			rows++
			if stopVersion != 0 && v >= stopVersion {
				continue
			}
			typ := str(r["event_type"])
			_, module, name, ok := venues.SplitType(typ)
			if !ok {
				skipped++
				continue
			}
			data := buildData(r, module, name)
			if data == nil {
				skipped++
				continue
			}
			ts, err := parseTimestamp(r["timestamp_us"])
			if err != nil {
				log.Fatalf("backfill: version %d: %v", v, err)
			}
			seq, err := strconv.ParseUint(str(r["event_index"]), 10, 64)
			if err != nil {
				log.Fatalf("backfill: version %d bad event_index: %v", v, err)
			}
			if cur == nil || cur.Version != v {
				if err := flush(); err != nil {
					log.Fatalf("backfill: %v", err)
				}
				cur = &venues.Transaction{Version: v, TimestampMicros: ts, Success: true}
			}
			cur.Events = append(cur.Events, venues.Event{Type: typ, SequenceNumber: seq, Data: data})
		}
		if err := flush(); err != nil {
			log.Fatalf("backfill: %v", err)
		}
		if len(page) < 1000 {
			break
		}
	}
	if rows == 0 && os.Getenv("ALLOW_EMPTY") == "" {
		log.Fatal("backfill: zero rows: processor empty or misconfigured (ALLOW_EMPTY=1 to proceed)")
	}
	log.Printf("backfill: done versions=%d rows=%d skipped=%d acts=%v", versions, rows, skipped, acts)
}


