// Command nft-indexer polls the Aptos fullnode for new transactions, maps
// venue events to normalized activities, and sinks them to Postgres.
// Cursor-gated and idempotent: kill -9 and restart any time.
package main

import (
	"context"
	"log"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/platform/config"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/platform/cors"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/platform/obs"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/store"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/stream"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues/reference"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues/tradeport"
)

type indexerConfig struct {
	FullnodeURL  string `toml:"fullnode_url"`
	DatabaseURL  string `toml:"database_url"`
	BindAddr     string `toml:"bind_addr"`
	OurVenue     string `toml:"our_venue_address"`
	StartVersion uint64 `toml:"start_version"`
}

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	shutdown := obs.InitTracing(ctx, "nft-indexer")
	defer shutdown()

	path := os.Getenv("CONFIG_PATH")
	if path == "" {
		path = "config/nft.toml"
	}
	var cfg indexerConfig
	if err := config.LoadTOML(path, &cfg); err != nil {
		log.Fatalf("indexer: config: %v", err)
	}
	if v := os.Getenv("BIND_ADDR"); v != "" {
		cfg.BindAddr = v
	}

	st, err := store.Open(ctx, cfg.DatabaseURL)
	if err != nil {
		log.Fatalf("indexer: store: %v", err)
	}
	defer st.Close()

	mux := http.NewServeMux()
	obs.MountHealthAndMetrics(mux)
	go func() {
		if err := http.ListenAndServe(cfg.BindAddr, cors.Wrap(mux)); err != nil {
			log.Printf("indexer: http: %v", err)
		}
	}()

	mappers := []venues.Mapper{
		reference.New("ours", cfg.OurVenue),
		reference.New("wapal", venues.AddrWapal),
		reference.New("rarible", venues.AddrRarible),
		reference.New("topaz-v2", venues.AddrTopazV2),
		tradeport.New(venues.AddrTradeport),
	}
	client := stream.New(cfg.FullnodeURL)

	cursor, err := st.Cursor(ctx, "indexer")
	if err != nil {
		log.Fatalf("indexer: cursor: %v", err)
	}
	if cursor == 0 && cfg.StartVersion != 0 {
		cursor = cfg.StartVersion
	}
	log.Printf("indexer: starting at version %d", cursor)

	for {
		select {
		case <-ctx.Done():
			return
		default:
		}
		txs, err := client.Fetch(ctx, cursor)
		if err != nil {
			log.Printf("indexer: fetch: %v", err)
			client.Backoff(ctx)
			continue
		}
		if len(txs) == 0 {
			client.Backoff(ctx)
			continue
		}
		var acts []venues.Activity
		for _, tx := range txs {
			for _, m := range mappers {
				mapped, err := m.Map(tx)
				if err != nil {
					log.Printf("indexer: mapper %s: %v", m.Marketplace(), err)
					continue
				}
				acts = append(acts, mapped...)
			}
		}
		last := txs[len(txs)-1].Version
		if err := st.ApplyBatch(ctx, "indexer", acts, last); err != nil {
			log.Printf("indexer: apply: %v", err)
			time.Sleep(2 * time.Second)
			continue
		}
		log.Printf("indexer: applied %d txs (%d activities), cursor=%d", len(txs), len(acts), last)
		cursor = last + 1
	}
}
