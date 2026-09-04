package main

import (
	"context"
	"log"
	"time"

	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/store"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/stream"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues"
)

// runGRPC follows the Transaction Stream Service forever, resuming from
// the stored cursor after every break. The server sends only transactions
// matching our venue filter, so each response applies directly.
func runGRPC(ctx context.Context, cfg indexerConfig, mappers []venues.Mapper, st *store.Store,
	loadCursor func() uint64, applyTxs func([]venues.Transaction) (uint64, error)) {
	addrs := make([]string, 0, len(mappers))
	for _, m := range mappers {
		addrs = append(addrs, m.ContractAddress())
	}
	key := stream.KeyFromEnv()
	for {
		select {
		case <-ctx.Done():
			return
		default:
		}
		cursor := loadCursor()
		endpoint := cfg.GRPCEndpoint
		if endpoint == "" {
			endpoint = stream.DefaultGRPCEndpoint
		}
		client, err := stream.NewGRPC(endpoint, key, addrs)
		if err != nil {
			log.Printf("indexer: grpc dial: %v", err)
			time.Sleep(10 * time.Second)
			continue
		}
		log.Printf("indexer: grpc streaming from version %d", cursor)
		err = client.Stream(ctx, cursor, func(txs []venues.Transaction) (uint64, error) {
			return applyTxs(txs)
		})
		client.Close()
		if ctx.Err() != nil {
			return
		}
		// Streams break (idle timeouts, deploys, 429s): resume at cursor.
		log.Printf("indexer: grpc stream ended (%v); resuming", err)
		time.Sleep(5 * time.Second)
	}
}
