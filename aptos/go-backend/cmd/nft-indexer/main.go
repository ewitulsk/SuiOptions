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
	key := stream.KeyFromEnv()
	client := stream.NewWithKey(cfg.FullnodeURL, key)
	// One page in flight sustains ~210 tps against a ~190 tps chain —
	// no catch-up margin. Two overlapping pages double that; four
	// trips keyed 429s and collapses below single-flight.
	const fetchAhead = 2
	pageSize := uint64(client.PageSize())
	type pageResult struct {
		txs []venues.Transaction
		err error
	}
	inflight := map[uint64]chan pageResult{}
	launch := func(start uint64) {
		if _, ok := inflight[start]; ok {
			return
		}
		ch := make(chan pageResult, 1)
		inflight[start] = ch
		go func() {
			txs, err := client.Fetch(ctx, start)
			select {
			case ch <- pageResult{txs, err}:
			case <-ctx.Done():
			}
		}()
	}

	cursor, err := st.Cursor(ctx, "indexer")
	if err != nil {
		log.Fatalf("indexer: cursor: %v", err)
	}
	// Fast-forward a stale cursor (e.g. accumulated while anonymous):
	// anything before StartVersion is ancient history the REST window
	// would take days to replay; backfill stays an archival-endpoint job.
	if cfg.StartVersion != 0 && cursor < cfg.StartVersion {
		log.Printf("indexer: fast-forwarding cursor %d -> %d", cursor, cfg.StartVersion)
		cursor = cfg.StartVersion
	}
	log.Printf("indexer: starting at version %d", cursor)

	nextFetch := cursor
	for {
		select {
		case <-ctx.Done():
			return
		default:
		}
		// Keep the pipeline full; pages apply strictly in order below,
		// so out-of-order arrivals are safe (history is immutable).
		for len(inflight) < fetchAhead {
			launch(nextFetch)
			nextFetch += pageSize
		}
		res, ok := <-inflight[cursor]
		if !ok {
			// Abandoned during a tip reset; re-launch at the cursor.
			nextFetch = cursor
			continue
		}
		delete(inflight, cursor)
		if res.err != nil {
			log.Printf("indexer: fetch: %v", res.err)
			// Fetch errors are usually 429s: wait out the quota window
			// instead of hammering it, and collapse the pipeline so a
			// retry resumes exactly at the cursor.
			time.Sleep(10 * time.Second)
			for start := range inflight {
				delete(inflight, start)
			}
			nextFetch = cursor
			continue
		}
		txs := res.txs
		if len(txs) == 0 {
			for start := range inflight {
				delete(inflight, start)
			}
			nextFetch = cursor
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
			launch(cursor)
			continue
		}
		log.Printf("indexer: applied %d txs (%d activities), cursor=%d", len(txs), len(acts), last)
		cursor = last + 1
		if uint64(len(txs)) < pageSize {
			// Short page: reached the tip. Collapse the pipeline —
			// versions past last don't exist yet — and breathe.
			for start := range inflight {
				delete(inflight, start)
			}
			nextFetch = cursor
			client.Backoff(ctx)
		}
	}
}
