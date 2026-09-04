// Package store is the indexer's Postgres sink: idempotent activity
// inserts, live-listing state transitions, and the stream cursor.
package store

import (
	"context"
	"embed"
	"fmt"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"

	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/platform/db"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues"
)

//go:embed migrations/*.sql
var migrationsFS embed.FS

// Store wraps the pool with typed apply methods.
type Store struct {
	pool *pgxpool.Pool
}

// Open connects and migrates.
func Open(ctx context.Context, databaseURL string) (*Store, error) {
	pool, err := db.Connect(ctx, databaseURL, migrationsFS, "migrations")
	if err != nil {
		return nil, err
	}
	return &Store{pool: pool}, nil
}

// Close drains the pool.
func (s *Store) Close() { s.pool.Close() }

// Cursor returns the last applied version for a pipeline (0 when new).
func (s *Store) Cursor(ctx context.Context, name string) (uint64, error) {
	var v int64
	err := s.pool.QueryRow(ctx,
		`SELECT last_version FROM pipeline_progress WHERE name=$1`, name).Scan(&v)
	if err == pgx.ErrNoRows {
		return 0, nil
	}
	return uint64(v), err
}

// ApplyBatch inserts activities, folds listing state, and advances the
// cursor in one transaction. Retrying a batch is safe: activity inserts are
// idempotent and state folds converge.
func (s *Store) ApplyBatch(ctx context.Context, pipeline string, acts []venues.Activity, lastVersion uint64) error {
	tx, err := s.pool.Begin(ctx)
	if err != nil {
		return err
	}
	defer tx.Rollback(ctx)
	for _, a := range acts {
		if err := insertActivity(ctx, tx, a); err != nil {
			return err
		}
		if err := foldListing(ctx, tx, a); err != nil {
			return err
		}
	}
	_, err = tx.Exec(ctx, `
		INSERT INTO pipeline_progress (name, last_version, updated_at)
		VALUES ($1, $2, now())
		ON CONFLICT (name) DO UPDATE
		SET last_version = GREATEST(pipeline_progress.last_version, $2),
		    updated_at = now()`, pipeline, lastVersion)
	if err != nil {
		return err
	}
	return tx.Commit(ctx)
}

func insertActivity(ctx context.Context, tx pgx.Tx, a venues.Activity) error {
	_, err := tx.Exec(ctx, `
		INSERT INTO activities
		(version, event_index, timestamp_us, marketplace, kind, raw_event,
		 listing_id, token_data_id, creator, collection, token_name, property_ver,
		 price, quote_token, buyer, seller, commission, royalty, remaining)
		VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19)
		ON CONFLICT DO NOTHING`,
		a.Version, a.EventIndex, a.TimestampUs, a.Marketplace, a.Kind, a.RawEventType,
		a.ListingID, a.TokenDataID, a.Creator, a.Collection, a.TokenName,
		nullable(a.PropertyVer), nullable(a.Price), a.QuoteToken, a.Buyer, a.Seller,
		nullable(a.Commission), nullable(a.Royalty), nullable(a.Remaining))
	return err
}

// foldListing maintains live_listings: created/offer rows open (or refresh),
// fills and cancels close.
func foldListing(ctx context.Context, tx pgx.Tx, a venues.Activity) error {
	if a.ListingID == "" {
		return nil
	}
	switch a.Kind {
	case venues.KindCreated, venues.KindOffer:
		_, err := tx.Exec(ctx, `
			INSERT INTO live_listings
			(marketplace, listing_id, token_data_id, creator, collection, token_name,
			 property_ver, price, quote_token, seller, open_version, updated_at)
			VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,now())
			ON CONFLICT (marketplace, listing_id) DO UPDATE SET
				price = EXCLUDED.price, updated_at = now()`,
			a.Marketplace, a.ListingID, a.TokenDataID, a.Creator, a.Collection,
			a.TokenName, nullable(a.PropertyVer), orZero(a.Price), a.QuoteToken,
			a.Seller, a.Version)
		return err
	case venues.KindFilled, venues.KindCancelled:
		_, err := tx.Exec(ctx,
			`DELETE FROM live_listings WHERE marketplace=$1 AND listing_id=$2`,
			a.Marketplace, a.ListingID)
		return err
	}
	return fmt.Errorf("store: unknown kind %q", a.Kind)
}

func nullable(v *uint64) any {
	if v == nil {
		return nil
	}
	return int64(*v)
}

func orZero(v *uint64) int64 {
	if v == nil {
		return 0
	}
	return int64(*v)
}
