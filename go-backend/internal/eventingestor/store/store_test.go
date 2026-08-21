// Integration tests over a real Postgres, gated on TEST_DATABASE_URL (see
// the leaderboard store suite for the local invocation). Provisions its own
// scratch database, so both suites can run in parallel.
package store

import (
	"context"
	"encoding/json"
	"fmt"
	"net/url"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/db"
)

const testDBName = "event_ingestor_gotest"

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

const testPkg = "0x00000000000000000000000000000000000000000000000000000000000000aa"

func trackPackage(t *testing.T, s *Store) {
	t.Helper()
	modules, _ := json.Marshal(map[string]any{"package": testPkg, "modules": []any{}})
	if err := s.UpsertPackage(context.Background(), testPkg, "test", "0xadmin", modules); err != nil {
		t.Fatalf("upsert package: %v", err)
	}
}

func strptr(s string) *string { return &s }

func TestRulesCursorsDeliveriesLifecycle(t *testing.T) {
	s := testStore(t)
	ctx := context.Background()
	trackPackage(t, s)

	// Create a tip rule and a timestamp rule; the latter arms a backfill.
	tip, err := s.CreateRule(ctx, &Rule{
		PackageAddress: testPkg, ModuleName: "settlement",
		EventType: testPkg + "::settlement::FillEvent",
		Points:    10, RecipientMode: "sender", StartMode: "tip", Enabled: true,
	})
	if err != nil {
		t.Fatalf("create tip rule: %v", err)
	}
	if tip.BackfillState != "none" {
		t.Fatalf("tip rule backfill_state = %q, want none", tip.BackfillState)
	}
	startAt := time.Now().UTC().Add(-24 * time.Hour).Truncate(time.Second)
	ts, err := s.CreateRule(ctx, &Rule{
		PackageAddress: testPkg, ModuleName: "settlement",
		EventType: testPkg + "::settlement::CancelEvent",
		Points:    5, RecipientMode: "field", RecipientField: strptr("maker"),
		StartMode: "timestamp", StartAt: &startAt, Enabled: true,
	})
	if err != nil {
		t.Fatalf("create timestamp rule: %v", err)
	}
	if ts.BackfillState != "pending" {
		t.Fatalf("timestamp rule backfill_state = %q, want pending", ts.BackfillState)
	}

	// Duplicate event rule per module is rejected by the unique constraint.
	if _, err := s.CreateRule(ctx, &Rule{
		PackageAddress: testPkg, ModuleName: "settlement",
		EventType: testPkg + "::settlement::FillEvent",
		Points:    1, RecipientMode: "sender", StartMode: "tip",
	}); err == nil || !strings.Contains(err.Error(), "duplicate key") {
		t.Fatalf("duplicate rule should hit unique constraint, got %v", err)
	}

	// Both rules watch one module stream.
	mods, err := s.ActiveModules(ctx)
	if err != nil || len(mods) != 1 {
		t.Fatalf("active modules = %v (%v)", mods, err)
	}
	rules, err := s.EnabledRulesForModule(ctx, testPkg, "settlement")
	if err != nil || len(rules) != 2 {
		t.Fatalf("enabled rules = %d (%v)", len(rules), err)
	}

	// Patch: disable the tip rule; stream roster respects it.
	off := false
	patched, err := s.PatchRule(ctx, tip.ID, RulePatch{Enabled: &off})
	if err != nil || patched.Enabled {
		t.Fatalf("patch disable: %+v (%v)", patched, err)
	}
	rules, _ = s.EnabledRulesForModule(ctx, testPkg, "settlement")
	if len(rules) != 1 {
		t.Fatalf("disabled rule still active: %d", len(rules))
	}

	// Cursor save/load round-trip with the pkg| tag.
	if cur, err := s.LoadCursor(ctx, testPkg, "settlement"); err != nil || cur != nil {
		t.Fatalf("fresh cursor should be nil, got %v (%v)", cur, err)
	}
	if err := s.SaveCursor(ctx, testPkg, "settlement", testPkg+"|opaque123"); err != nil {
		t.Fatalf("save cursor: %v", err)
	}
	cur, err := s.LoadCursor(ctx, testPkg, "settlement")
	if err != nil || cur == nil || *cur != testPkg+"|opaque123" {
		t.Fatalf("cursor round-trip: %v (%v)", cur, err)
	}

	// Deliveries: record once, duplicate is a no-op, claim check sees it.
	key := "digest:0:" + fmt.Sprint(ts.ID)
	if err := s.RecordDelivery(ctx, ts.ID, key, "0xrecipient", 5, time.Now().UTC()); err != nil {
		t.Fatalf("record delivery: %v", err)
	}
	if err := s.RecordDelivery(ctx, ts.ID, key, "0xrecipient", 5, time.Now().UTC()); err != nil {
		t.Fatalf("duplicate delivery should be no-op: %v", err)
	}
	claimed, err := s.DeliveryClaimed(ctx, key)
	if err != nil || !claimed {
		t.Fatalf("delivery claim: %v (%v)", claimed, err)
	}

	// Backfill lifecycle: claim → progress → done.
	claimable, err := s.ClaimableBackfills(ctx)
	if err != nil || len(claimable) != 1 || claimable[0].ID != ts.ID {
		t.Fatalf("claimable = %+v (%v)", claimable, err)
	}
	if err := s.SetBackfillRunning(ctx, ts.ID); err != nil {
		t.Fatalf("set running: %v", err)
	}
	if err := s.SaveBackfillProgress(ctx, ts.ID, "back-cursor"); err != nil {
		t.Fatalf("save progress: %v", err)
	}
	if err := s.FinishBackfill(ctx, ts.ID, "done"); err != nil {
		t.Fatalf("finish: %v", err)
	}
	if claimable, _ = s.ClaimableBackfills(ctx); len(claimable) != 0 {
		t.Fatalf("done rule still claimable: %+v", claimable)
	}

	// Status surfaces the module stream and both rules.
	modStatus, ruleStatus, err := s.Status(ctx)
	if err != nil || len(modStatus) != 1 || len(ruleStatus) != 2 {
		t.Fatalf("status: mods=%d rules=%d (%v)", len(modStatus), len(ruleStatus), err)
	}

	// Package delete cascades rules and clears cursors.
	if err := s.DeletePackage(ctx, testPkg); err != nil {
		t.Fatalf("delete package: %v", err)
	}
	if rules, _ := s.ListRules(ctx, ""); len(rules) != 0 {
		t.Fatalf("rules survived package delete: %+v", rules)
	}
	if cur, _ := s.LoadCursor(ctx, testPkg, "settlement"); cur != nil {
		t.Fatalf("cursor survived package delete: %v", *cur)
	}
}
