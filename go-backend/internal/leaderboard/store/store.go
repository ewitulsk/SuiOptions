// Package store is the leaderboard's SQL layer. Every write path runs in
// one transaction — points inserts maintain the cached totals, and merges
// repoint all rows so reads never chase merged_into chains.
package store

import (
	"context"
	"errors"
	"fmt"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
)

// Identity is one external identity bound to an account.
type Identity struct {
	Type       string // wallet | twitter | discord
	Identifier string // already normalized by the caller
}

// PointsWrite is one internal points mutation.
type PointsWrite struct {
	Identity       Identity
	Delta          int64
	Source         string
	SourceLabel    string
	EventType      string
	IdempotencyKey string
	OccurredAt     time.Time
}

// Entry is one ranked row.
type Entry struct {
	Rank       int64    `json:"rank"`
	AccountID  int64    `json:"account_id"`
	Wallets    []string `json:"wallets"`
	Twitter    *string  `json:"twitter"`
	Points     int64    `json:"points"`
	EventCount int64    `json:"event_count"`
}

// SourceRow feeds the public filter dropdown.
type SourceRow struct {
	Source    string  `json:"source"`
	Label     *string `json:"label"`
	EventType *string `json:"event_type"`
}

// BreakdownRow is one per-source slice of an account's points.
type BreakdownRow struct {
	Source      string     `json:"source"`
	Label       *string    `json:"label"`
	EventType   *string    `json:"event_type"`
	Points      int64      `json:"points"`
	EventCount  int64      `json:"event_count"`
	LastEventMs *int64     `json:"last_event_ms"`
}

var ErrNotFound = errors.New("not found")

type Store struct {
	pool *pgxpool.Pool
}

func New(pool *pgxpool.Pool) *Store { return &Store{pool: pool} }

// --- writes ------------------------------------------------------------------

// resolveAccount returns the account owning ident, creating account + identity
// when absent. Runs inside tx.
func (s *Store) resolveAccount(ctx context.Context, tx pgx.Tx, ident Identity) (int64, error) {
	var id int64
	err := tx.QueryRow(ctx,
		`SELECT account_id FROM account_identities WHERE identity_type = $1 AND identifier = $2`,
		ident.Type, ident.Identifier).Scan(&id)
	if err == nil {
		return id, nil
	}
	if !errors.Is(err, pgx.ErrNoRows) {
		return 0, fmt.Errorf("lookup identity: %w", err)
	}
	// Create the account shell then bind the identity. The PK on
	// (identity_type, identifier) makes a concurrent double-create impossible;
	// the loser of that race re-reads below.
	if err := tx.QueryRow(ctx, `INSERT INTO accounts DEFAULT VALUES RETURNING id`).Scan(&id); err != nil {
		return 0, fmt.Errorf("create account: %w", err)
	}
	tag, err := tx.Exec(ctx,
		`INSERT INTO account_identities (identity_type, identifier, account_id) VALUES ($1, $2, $3)
		 ON CONFLICT (identity_type, identifier) DO NOTHING`,
		ident.Type, ident.Identifier, id)
	if err != nil {
		return 0, fmt.Errorf("bind identity: %w", err)
	}
	if tag.RowsAffected() == 0 {
		// Someone else created it first; adopt their account and drop ours.
		var winner int64
		if err := tx.QueryRow(ctx,
			`SELECT account_id FROM account_identities WHERE identity_type = $1 AND identifier = $2`,
			ident.Type, ident.Identifier).Scan(&winner); err != nil {
			return 0, fmt.Errorf("re-read identity after conflict: %w", err)
		}
		_, _ = tx.Exec(ctx, `DELETE FROM accounts WHERE id = $1 AND NOT EXISTS (SELECT 1 FROM account_identities WHERE account_id = $1)`, id)
		return winner, nil
	}
	return id, nil
}

// AddPoints applies w atomically. When w.IdempotencyKey was already applied
// it returns applied=false (idempotent success) without touching anything.
func (s *Store) AddPoints(ctx context.Context, w PointsWrite) (bool, error) {
	tx, err := s.pool.Begin(ctx)
	if err != nil {
		return false, fmt.Errorf("begin: %w", err)
	}
	defer tx.Rollback(ctx)

	accountID, err := s.resolveAccount(ctx, tx, w.Identity)
	if err != nil {
		return false, err
	}

	var inserted bool
	if w.IdempotencyKey != "" {
		var id int64
		err := tx.QueryRow(ctx,
			`INSERT INTO points_entries (account_id, delta, source, event_type, idempotency_key, occurred_at)
			 VALUES ($1, $2, $3, NULLIF($4,''), $5, $6)
			 ON CONFLICT (idempotency_key) DO NOTHING
			 RETURNING id`,
			accountID, w.Delta, w.Source, w.EventType, w.IdempotencyKey, w.OccurredAt).Scan(&id)
		switch {
		case err == nil:
			inserted = true
		case errors.Is(err, pgx.ErrNoRows):
			// Duplicate delivery — idempotent success, nothing else changes.
			return false, tx.Commit(ctx)
		default:
			return false, fmt.Errorf("insert entry: %w", err)
		}
	} else {
		if _, err := tx.Exec(ctx,
			`INSERT INTO points_entries (account_id, delta, source, event_type, occurred_at)
			 VALUES ($1, $2, $3, NULLIF($4,''), $5)`,
			accountID, w.Delta, w.Source, w.EventType, w.OccurredAt); err != nil {
			return false, fmt.Errorf("insert entry: %w", err)
		}
		inserted = true
	}
	if !inserted {
		return false, tx.Commit(ctx)
	}

	// Maintain the cached total in the same tx.
	if _, err := tx.Exec(ctx,
		`INSERT INTO account_totals (account_id, total, updated_at) VALUES ($1, $2, now())
		 ON CONFLICT (account_id) DO UPDATE SET total = account_totals.total + EXCLUDED.total, updated_at = now()`,
		accountID, w.Delta); err != nil {
		return false, fmt.Errorf("update total: %w", err)
	}

	// Upsert the human label for the source filter dropdown when provided.
	if w.SourceLabel != "" || w.EventType != "" {
		if _, err := tx.Exec(ctx,
			`INSERT INTO sources (source, event_type, label, updated_at) VALUES ($1, NULLIF($2,''), NULLIF($3,''), now())
			 ON CONFLICT (source) DO UPDATE SET
			   event_type = COALESCE(EXCLUDED.event_type, sources.event_type),
			   label = COALESCE(EXCLUDED.label, sources.label),
			   updated_at = now()`,
			w.Source, w.EventType, w.SourceLabel); err != nil {
			return false, fmt.Errorf("upsert source label: %w", err)
		}
	}

	return true, tx.Commit(ctx)
}

// LinkResult reports the outcome of a link request.
type LinkResult struct {
	AccountID int64
	Merged    bool
}

// Link implements the four link cases: neither exists → one new account with
// both identities; one exists → attach the other; both exist on different
// accounts → merge; same account → no-op.
func (s *Store) Link(ctx context.Context, a, b Identity) (LinkResult, error) {
	tx, err := s.pool.Begin(ctx)
	if err != nil {
		return LinkResult{}, err
	}
	defer tx.Rollback(ctx)

	idA, errA := s.findIdentityTx(ctx, tx, a)
	idB, errB := s.findIdentityTx(ctx, tx, b)
	switch {
	case errA != nil || errB != nil:
		return LinkResult{}, fmt.Errorf("find identities: %w / %w", errA, errB)
	case idA != 0 && idB != 0 && idA == idB:
		return LinkResult{AccountID: idA, Merged: false}, tx.Commit(ctx)
	case idA != 0 && idB != 0:
		winner, loser := idA, idB
		if loser < winner {
			winner, loser = loser, winner
		}
		if err := s.mergeTx(ctx, tx, winner, loser); err != nil {
			return LinkResult{}, err
		}
		return LinkResult{AccountID: winner, Merged: true}, tx.Commit(ctx)
	case idA != 0:
		if _, err := tx.Exec(ctx,
			`INSERT INTO account_identities (identity_type, identifier, account_id) VALUES ($1,$2,$3)
			 ON CONFLICT (identity_type, identifier) DO NOTHING`, b.Type, b.Identifier, idA); err != nil {
			return LinkResult{}, fmt.Errorf("attach identity: %w", err)
		}
		return LinkResult{AccountID: idA}, tx.Commit(ctx)
	case idB != 0:
		if _, err := tx.Exec(ctx,
			`INSERT INTO account_identities (identity_type, identifier, account_id) VALUES ($1,$2,$3)
			 ON CONFLICT (identity_type, identifier) DO NOTHING`, a.Type, a.Identifier, idB); err != nil {
			return LinkResult{}, fmt.Errorf("attach identity: %w", err)
		}
		return LinkResult{AccountID: idB}, tx.Commit(ctx)
	default:
		// Neither exists: one fresh account carrying both identities.
		accountID, err := s.resolveAccount(ctx, tx, a)
		if err != nil {
			return LinkResult{}, err
		}
		if _, err := tx.Exec(ctx,
			`INSERT INTO account_identities (identity_type, identifier, account_id) VALUES ($1,$2,$3)
			 ON CONFLICT (identity_type, identifier) DO NOTHING`, b.Type, b.Identifier, accountID); err != nil {
			return LinkResult{}, fmt.Errorf("bind second identity: %w", err)
		}
		return LinkResult{AccountID: accountID}, tx.Commit(ctx)
	}
}

func (s *Store) findIdentityTx(ctx context.Context, tx pgx.Tx, ident Identity) (int64, error) {
	var id int64
	err := tx.QueryRow(ctx,
		`SELECT account_id FROM account_identities WHERE identity_type=$1 AND identifier=$2`,
		ident.Type, ident.Identifier).Scan(&id)
	if errors.Is(err, pgx.ErrNoRows) {
		return 0, nil
	}
	return id, err
}

// mergeTx folds loser into winner inside the caller's transaction.
//
// Advisory locks are taken in ascending-id order so two concurrent merges
// sharing an endpoint can never deadlock. merged_into stays audit-only:
// every owned row is repointed here so no read ever has to chase chains.
func (s *Store) mergeTx(ctx context.Context, tx pgx.Tx, winner, loser int64) error {
	lo, hi := winner, loser
	if hi < lo {
		lo, hi = hi, lo
	}
	if _, err := tx.Exec(ctx, `SELECT pg_advisory_xact_lock($1)`, lo); err != nil {
		return fmt.Errorf("advisory lock: %w", err)
	}
	if _, err := tx.Exec(ctx, `SELECT pg_advisory_xact_lock($1)`, hi); err != nil {
		return fmt.Errorf("advisory lock: %w", err)
	}

	// Identities move wholesale — they were globally unique before the merge,
	// so the move cannot collide.
	if _, err := tx.Exec(ctx,
		`UPDATE account_identities SET account_id = $1 WHERE account_id = $2`, winner, loser); err != nil {
		return fmt.Errorf("repoint identities: %w", err)
	}
	if _, err := tx.Exec(ctx,
		`UPDATE points_entries SET account_id = $1 WHERE account_id = $2`, winner, loser); err != nil {
		return fmt.Errorf("repoint entries: %w", err)
	}
	// Fold the cached total: add loser's into winner's row (creating it if
	// winner had none), then drop loser's row.
	if _, err := tx.Exec(ctx,
		`INSERT INTO account_totals (account_id, total, updated_at)
		 SELECT $1, total, now() FROM account_totals WHERE account_id = $2
		 ON CONFLICT (account_id) DO UPDATE SET
		   total = account_totals.total + EXCLUDED.total, updated_at = now()`,
		winner, loser); err != nil {
		return fmt.Errorf("fold totals: %w", err)
	}
	if _, err := tx.Exec(ctx, `DELETE FROM account_totals WHERE account_id = $1`, loser); err != nil {
		return fmt.Errorf("drop loser total: %w", err)
	}
	if _, err := tx.Exec(ctx,
		`UPDATE accounts SET merged_into = $1 WHERE id = $2`, winner, loser); err != nil {
		return fmt.Errorf("mark merged: %w", err)
	}
	if _, err := tx.Exec(ctx,
		`INSERT INTO account_merges (winner_account_id, loser_account_id) VALUES ($1, $2)
		 ON CONFLICT DO NOTHING`, winner, loser); err != nil {
		return fmt.Errorf("record merge: %w", err)
	}
	return nil
}

// --- reads --------------------------------------------------------------------

// Window maps a public window param to a SQL interval suffix ("all" → "").
var windowIntervals = map[string]string{
	"30d": "30 days",
	"7d":  "7 days",
	"24h": "24 hours",
}

// scopedCTE is the shared aggregation every ranked read starts from: sum the
// ledger over the optional time window and source filter, rank by points.
const scopedCTE = `
WITH scoped AS (
	SELECT account_id,
	       SUM(delta)::bigint AS points,
	       COUNT(*)::bigint   AS event_count
	FROM points_entries
	WHERE ($1::timestamptz IS NULL OR occurred_at >= $1)
	  AND ($2::text IS NULL OR source = $2)
	GROUP BY account_id
), ranked AS (
	SELECT account_id, points, event_count,
	       RANK() OVER (ORDER BY points DESC) AS rank
	FROM scoped
)`

func windowSince(window string) (any, error) {
	interval, ok := windowIntervals[window]
	if !ok && window != "" && window != "all" {
		return nil, fmt.Errorf("invalid window %q", window)
	}
	if interval == "" {
		return nil, nil
	}
	since := time.Now().Add(-parseInterval(interval))
	return since.UTC(), nil
}

// parseInterval converts the fixed whitelist above; avoids depending on
// pgx interval scanning for constant inputs.
func parseInterval(s string) time.Duration {
	switch s {
	case "30 days":
		return 30 * 24 * time.Hour
	case "7 days":
		return 7 * 24 * time.Hour
	case "24 hours":
		return 24 * time.Hour
	}
	return 0
}

// Leaderboard returns one page of ranked entries plus the total number of
// scored accounts in scope.
func (s *Store) Leaderboard(ctx context.Context, window, source string, limit, offset int) ([]Entry, int64, error) {
	since, err := windowSince(window)
	if err != nil {
		return nil, 0, err
	}
	var src any
	if source != "" {
		src = source
	}
	rows, err := s.pool.Query(ctx, scopedCTE+`
	SELECT rank, account_id, points, event_count,
	       COUNT(*) OVER () AS total_accounts
	FROM ranked
	ORDER BY rank, account_id
	LIMIT $3 OFFSET $4`, since, src, limit, offset)
	if err != nil {
		return nil, 0, fmt.Errorf("leaderboard query: %w", err)
	}
	defer rows.Close()

	var entries []Entry
	var total int64
	for rows.Next() {
		var e Entry
		if err := rows.Scan(&e.Rank, &e.AccountID, &e.Points, &e.EventCount, &total); err != nil {
			return nil, 0, err
		}
		e.Wallets = []string{}
		entries = append(entries, e)
	}
	if err := rows.Err(); err != nil {
		return nil, 0, err
	}
	if len(entries) > 0 {
		if err := s.attachIdentities(ctx, entries); err != nil {
			return nil, 0, err
		}
	}
	return entries, total, nil
}

// attachIdentities fills wallets/twitter for a page of entries in one query.
func (s *Store) attachIdentities(ctx context.Context, entries []Entry) error {
	ids := make([]int64, len(entries))
	byID := make(map[int64]*Entry, len(entries))
	for i := range entries {
		ids[i] = entries[i].AccountID
		byID[entries[i].AccountID] = &entries[i]
	}
	rows, err := s.pool.Query(ctx,
		`SELECT account_id, identity_type, identifier FROM account_identities
		 WHERE account_id = ANY($1) ORDER BY created_at, identifier`, ids)
	if err != nil {
		return fmt.Errorf("identities query: %w", err)
	}
	defer rows.Close()
	for rows.Next() {
		var accountID int64
		var typ, identifier string
		if err := rows.Scan(&accountID, &typ, &identifier); err != nil {
			return err
		}
		e := byID[accountID]
		if e == nil {
			continue
		}
		switch typ {
		case "wallet":
			e.Wallets = append(e.Wallets, identifier)
		case "twitter":
			t := identifier
			e.Twitter = &t
		}
	}
	return rows.Err()
}

// RankOf resolves wallet to its account and returns its entry plus the
// ±radius neighborhood (target included). ErrNotFound when the wallet is
// unknown or has no points in scope.
func (s *Store) RankOf(ctx context.Context, wallet string, window, source string, radius int) ([]Entry, int64, error) {
	since, err := windowSince(window)
	if err != nil {
		return nil, 0, err
	}
	var src any
	if source != "" {
		src = source
	}

	var accountID int64
	err = s.pool.QueryRow(ctx,
		`SELECT account_id FROM account_identities WHERE identity_type='wallet' AND identifier=$1`,
		wallet).Scan(&accountID)
	if errors.Is(err, pgx.ErrNoRows) {
		return nil, 0, ErrNotFound
	}
	if err != nil {
		return nil, 0, fmt.Errorf("resolve wallet: %w", err)
	}

	rows, err := s.pool.Query(ctx, scopedCTE+`
	SELECT rank, account_id, points, event_count, COUNT(*) OVER () AS total_accounts
	FROM ranked
	WHERE rank BETWEEN (SELECT rank FROM ranked WHERE account_id = $3) - $4
	               AND (SELECT rank FROM ranked WHERE account_id = $3) + $4
	ORDER BY rank, account_id`, since, src, accountID, radius)
	if err != nil {
		return nil, 0, fmt.Errorf("rank query: %w", err)
	}
	defer rows.Close()

	var entries []Entry
	var total int64
	for rows.Next() {
		var e Entry
		if err := rows.Scan(&e.Rank, &e.AccountID, &e.Points, &e.EventCount, &total); err != nil {
			return nil, 0, err
		}
		e.Wallets = []string{}
		entries = append(entries, e)
	}
	if err := rows.Err(); err != nil {
		return nil, 0, err
	}
	if len(entries) == 0 {
		return nil, 0, ErrNotFound
	}
	if err := s.attachIdentities(ctx, entries); err != nil {
		return nil, 0, err
	}
	return entries, total, nil
}

// AccountPoints returns the scoped total for one account (0 rows → not found).
func (s *Store) AccountPoints(ctx context.Context, accountID int64, window, source string) (int64, error) {
	since, err := windowSince(window)
	if err != nil {
		return 0, err
	}
	var src any
	if source != "" {
		src = source
	}
	var total int64
	err = s.pool.QueryRow(ctx, `
	SELECT COALESCE(SUM(delta), 0)::bigint FROM points_entries
	WHERE account_id = $1
	  AND ($2::timestamptz IS NULL OR occurred_at >= $2)
	  AND ($3::text IS NULL OR source = $3)`, accountID, since, src).Scan(&total)
	return total, err
}

// AccountByWallet resolves a wallet identity to its account.
func (s *Store) AccountByWallet(ctx context.Context, wallet string) (int64, error) {
	var id int64
	err := s.pool.QueryRow(ctx,
		`SELECT account_id FROM account_identities WHERE identity_type='wallet' AND identifier=$1`,
		wallet).Scan(&id)
	if errors.Is(err, pgx.ErrNoRows) {
		return 0, ErrNotFound
	}
	return id, err
}

// Breakdown slices one account's scoped points by source with labels.
func (s *Store) Breakdown(ctx context.Context, accountID int64, window string) ([]BreakdownRow, error) {
	since, err := windowSince(window)
	if err != nil {
		return nil, err
	}
	rows, err := s.pool.Query(ctx, `
	SELECT pe.source,
	       s.label,
	       s.event_type,
	       SUM(pe.delta)::bigint          AS points,
	       COUNT(*)::bigint               AS event_count,
	       MAX(pe.occurred_at)            AS last_event
	FROM points_entries pe
	LEFT JOIN sources s USING (source)
	WHERE pe.account_id = $1
	  AND ($2::timestamptz IS NULL OR pe.occurred_at >= $2)
	GROUP BY pe.source, s.label, s.event_type
	ORDER BY points DESC`, accountID, since)
	if err != nil {
		return nil, fmt.Errorf("breakdown query: %w", err)
	}
	defer rows.Close()

	var out []BreakdownRow
	for rows.Next() {
		var r BreakdownRow
		var last time.Time
		if err := rows.Scan(&r.Source, &r.Label, &r.EventType, &r.Points, &r.EventCount, &last); err != nil {
			return nil, err
		}
		ms := last.UnixMilli()
		r.LastEventMs = &ms
		out = append(out, r)
	}
	return out, rows.Err()
}

// Sources lists the known point sources for the filter dropdown.
func (s *Store) Sources(ctx context.Context) ([]SourceRow, error) {
	rows, err := s.pool.Query(ctx,
		`SELECT source, label, event_type FROM sources ORDER BY source`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []SourceRow
	for rows.Next() {
		var r SourceRow
		if err := rows.Scan(&r.Source, &r.Label, &r.EventType); err != nil {
			return nil, err
		}
		out = append(out, r)
	}
	return out, rows.Err()
}
