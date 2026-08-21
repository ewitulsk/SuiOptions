// Package db connects a pgx pool and applies embedded goose migrations.
//
// The Go analogue of diesel's `embed_migrations!` + `run_pending_migrations`
// at service boot: the SQL lives in the binary (store/migrations/*.sql via
// embed.FS), so a deploy always migrates its own schema before serving.
package db

import (
	"context"
	"database/sql"
	"fmt"
	"io/fs"
	"time"

	"github.com/jackc/pgx/v5/pgxpool"
	_ "github.com/jackc/pgx/v5/stdlib" // registers the "pgx" database/sql driver
	"github.com/pressly/goose/v3"
)

// Connect builds a pgx pool from databaseURL and runs every pending migration
// from migrationsFS (goose-style layout: <dir>/00001_init.sql, ...) before
// returning. Failing to migrate is fatal — a half-migrated service must not
// serve.
func Connect(ctx context.Context, databaseURL string, migrationsFS fs.FS, migrationsDir string) (*pgxpool.Pool, error) {
	cfg, err := pgxpool.ParseConfig(databaseURL)
	if err != nil {
		return nil, fmt.Errorf("db: parse url: %w", err)
	}
	// Bounded acquire wait so boot fails loudly instead of hanging when the
	// database is unreachable.
	cfg.ConnConfig.ConnectTimeout = 10 * time.Second
	pool, err := pgxpool.NewWithConfig(ctx, cfg)
	if err != nil {
		return nil, fmt.Errorf("db: connect: %w", err)
	}

	if err := migrate(databaseURL, migrationsFS, migrationsDir); err != nil {
		pool.Close()
		return nil, err
	}
	return pool, nil
}

// migrate runs goose over a throwaway database/sql handle (goose speaks
// database/sql; the serving pool stays pgx-native).
func migrate(databaseURL string, migrationsFS fs.FS, dir string) error {
	sqlDB, err := sql.Open("pgx", databaseURL)
	if err != nil {
		return fmt.Errorf("db: open for migration: %w", err)
	}
	defer sqlDB.Close()

	// The provider wants an FS rooted at the migration files themselves.
	sub, err := fs.Sub(migrationsFS, dir)
	if err != nil {
		return fmt.Errorf("db: migrations sub-fs: %w", err)
	}
	g, err := goose.NewProvider(goose.DialectPostgres, sqlDB, sub)
	if err != nil {
		return fmt.Errorf("db: goose provider: %w", err)
	}
	// The provider serializes concurrent boots via its version table, so two
	// instances migrating at once apply each migration exactly once.
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	defer cancel()
	if _, err := g.Up(ctx); err != nil {
		return fmt.Errorf("db: migrate: %w", err)
	}
	return nil
}
