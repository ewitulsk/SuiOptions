// Integration tests over a real Postgres, gated on TEST_DATABASE_URL
// (go-ci runs them against a postgres:16 service container; locally:
//
//	TEST_DATABASE_URL=postgresql://postgres:postgres@localhost:7654/postgres go test ./...
//
// The suite provisions its own scratch database so parallel packages never
// share tables.
package store

import (
	"context"
	"fmt"
	"net/url"
	"os"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/db"
)

const testDBName = "leaderboard_gotest"

func testStore(t *testing.T) *Store {
	t.Helper()
	base := os.Getenv("TEST_DATABASE_URL")
	if base == "" {
		t.Skip("TEST_DATABASE_URL not set; skipping Postgres integration tests")
	}
	ctx := context.Background()

	admin, err := pgx.Connect(ctx, base)
	if err != nil {
		t.Fatalf("connect admin: %v", err)
	}
	defer admin.Close(ctx)
	// Fresh database per run — cheap, and guarantees migration + test
	// isolation from the ingestor package's suite.
	if _, err := admin.Exec(ctx, fmt.Sprintf(`DROP DATABASE IF EXISTS %s`, testDBName)); err != nil {
		t.Fatalf("drop test db: %v", err)
	}
	if _, err := admin.Exec(ctx, fmt.Sprintf(`CREATE DATABASE %s`, testDBName)); err != nil {
		t.Fatalf("create test db: %v", err)
	}

	u, err := url.Parse(base)
	if err != nil {
		t.Fatalf("parse TEST_DATABASE_URL: %v", err)
	}
	u.Path = "/" + testDBName

	pool, err := db.Connect(ctx, u.String(), MigrationsFS, MigrationsDir)
	if err != nil {
		t.Fatalf("connect+migrate: %v", err)
	}
	t.Cleanup(pool.Close)
	return New(pool)
}

func wallet(hex string) Identity {
	return Identity{Type: "wallet", Identifier: fmt.Sprintf("0x%064s", hex)}
}

func addPoints(t *testing.T, s *Store, ident Identity, delta int64, source, key string, at time.Time) bool {
	t.Helper()
	applied, err := s.AddPoints(context.Background(), PointsWrite{
		Identity:       ident,
		Delta:          delta,
		Source:         source,
		IdempotencyKey: key,
		OccurredAt:     at,
	})
	if err != nil {
		t.Fatalf("AddPoints(%v %d %s): %v", ident, delta, source, err)
	}
	return applied
}

func TestPointsLinkMergeAndRanking(t *testing.T) {
	s := testStore(t)
	ctx := context.Background()
	now := time.Now().UTC()

	// Points auto-create accounts + identities.
	if applied := addPoints(t, s, wallet("a"), 10, "rule:1", "k1", now); !applied {
		t.Fatal("first write should apply")
	}
	addPoints(t, s, wallet("b"), 20, "rule:1", "k2", now)
	addPoints(t, s, wallet("c"), 5, "rule:2", "k3", now.Add(-40*24*time.Hour)) // outside 30d

	// Idempotency replay reports applied=false and changes nothing.
	if applied := addPoints(t, s, wallet("a"), 10, "rule:1", "k1", now); applied {
		t.Fatal("duplicate idempotency key must not re-apply")
	}

	// Link twitter → wallet a (attach case).
	res, err := s.Link(ctx, wallet("a"), Identity{Type: "twitter", Identifier: "alice"})
	if err != nil {
		t.Fatalf("link attach: %v", err)
	}
	if res.Merged {
		t.Fatal("attach must not merge")
	}

	// Merge: link wallet a and wallet b (both exist on different accounts).
	res, err = s.Link(ctx, wallet("a"), wallet("b"))
	if err != nil {
		t.Fatalf("link merge: %v", err)
	}
	if !res.Merged {
		t.Fatal("linking two scored accounts must merge")
	}

	// All-time board: merged account (30 pts) first, c (5 pts) second.
	entries, total, err := s.Leaderboard(ctx, "all", "", 50, 0)
	if err != nil {
		t.Fatalf("leaderboard: %v", err)
	}
	if total != 2 || len(entries) != 2 {
		t.Fatalf("want 2 scored accounts, got total=%d len=%d", total, len(entries))
	}
	if entries[0].Points != 30 || entries[0].Rank != 1 {
		t.Fatalf("merged account should lead with 30 pts, got %+v", entries[0])
	}
	if len(entries[0].Wallets) != 2 || entries[0].Twitter == nil {
		t.Fatalf("merged account should carry both wallets + twitter, got %+v", entries[0])
	}

	// 30d window excludes c's old entry.
	entries, total, err = s.Leaderboard(ctx, "30d", "", 50, 0)
	if err != nil {
		t.Fatalf("leaderboard 30d: %v", err)
	}
	if total != 1 || entries[0].Points != 30 {
		t.Fatalf("30d window should only hold the merged account, got total=%d %+v", total, entries)
	}

	// Source filter.
	entries, _, err = s.Leaderboard(ctx, "all", "rule:2", 50, 0)
	if err != nil {
		t.Fatalf("leaderboard source: %v", err)
	}
	if len(entries) != 1 || entries[0].Points != 5 {
		t.Fatalf("rule:2 filter should return only c, got %+v", entries)
	}

	// Rank + neighbors for a merged-away wallet identity (b now lives on the
	// winner account).
	neighbors, total, err := s.RankOf(ctx, wallet("b").Identifier, "all", "", 5)
	if err != nil {
		t.Fatalf("rankof: %v", err)
	}
	if total != 2 || len(neighbors) != 2 {
		t.Fatalf("neighbors should span both accounts, got total=%d len=%d", total, len(neighbors))
	}
	if neighbors[0].Rank != 1 || neighbors[0].Points != 30 {
		t.Fatalf("target row wrong: %+v", neighbors[0])
	}

	// Unknown wallet → ErrNotFound.
	if _, _, err := s.RankOf(ctx, wallet("dead").Identifier, "all", "", 5); err != ErrNotFound {
		t.Fatalf("unknown wallet: want ErrNotFound, got %v", err)
	}

	// Breakdown sums by source.
	accountID, err := s.AccountByWallet(ctx, wallet("a").Identifier)
	if err != nil {
		t.Fatalf("account by wallet: %v", err)
	}
	rows, err := s.Breakdown(ctx, accountID, "all")
	if err != nil {
		t.Fatalf("breakdown: %v", err)
	}
	if len(rows) != 1 || rows[0].Source != "rule:1" || rows[0].Points != 30 || rows[0].EventCount != 2 {
		t.Fatalf("breakdown rows wrong: %+v", rows)
	}

	// Negative delta = removal, reflected in cached totals.
	addPoints(t, s, wallet("c"), -5, "admin:manual", "", now)
	pts, err := s.AccountPoints(ctx, mustAccount(t, s, wallet("c")), "all", "")
	if err != nil || pts != 0 {
		t.Fatalf("negative delta: points=%d err=%v", pts, err)
	}
}

func TestLinkFreshAndSameAccount(t *testing.T) {
	s := testStore(t)
	ctx := context.Background()

	// Neither identity exists → one account with both.
	res, err := s.Link(ctx, wallet("f1"), Identity{Type: "discord", Identifier: "d#1"})
	if err != nil {
		t.Fatalf("fresh link: %v", err)
	}
	if res.Merged {
		t.Fatal("fresh link must not merge")
	}

	// Same pair again → same account, no merge.
	res2, err := s.Link(ctx, wallet("f1"), Identity{Type: "discord", Identifier: "d#1"})
	if err != nil {
		t.Fatalf("relink: %v", err)
	}
	if res2.AccountID != res.AccountID || res2.Merged {
		t.Fatalf("relink should be a no-op on the same account: %+v vs %+v", res, res2)
	}
}

func mustAccount(t *testing.T, s *Store, ident Identity) int64 {
	t.Helper()
	id, err := s.AccountByWallet(context.Background(), ident.Identifier)
	if err != nil {
		t.Fatalf("resolve %v: %v", ident, err)
	}
	return id
}
