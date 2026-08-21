// Package store is the event-ingestor's SQL layer: tracked packages,
// points rules, module cursors, and the delivery audit log.
package store

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
)

var ErrNotFound = errors.New("not found")

// TrackedPackage is one admin-added Sui package with cached introspection.
type TrackedPackage struct {
	PackageAddress string          `json:"package_address"`
	Label          string          `json:"label"`
	ModulesJSON    json.RawMessage `json:"modules"`
	IntrospectedAt *time.Time      `json:"introspected_at"`
	CreatedBy      string          `json:"created_by"`
	CreatedAt      time.Time       `json:"created_at"`
}

// Rule is one configured event→points rule.
type Rule struct {
	ID             int64      `json:"id"`
	PackageAddress string     `json:"package_address"`
	ModuleName     string     `json:"module_name"`
	EventType      string     `json:"event_type"`
	Label          string     `json:"label"`
	Points         int64      `json:"points"`
	RecipientMode  string     `json:"recipient_mode"` // sender | field
	RecipientField *string    `json:"recipient_field"`
	StartMode      string     `json:"start_mode"` // tip | timestamp
	StartAt        *time.Time `json:"start_at"`
	BackfillState  string     `json:"backfill_state"`
	BackfillCursor *string    `json:"-"`
	Enabled        bool       `json:"enabled"`
	CreatedBy      string     `json:"created_by"`
	CreatedAt      time.Time  `json:"created_at"`
	UpdatedAt      time.Time  `json:"updated_at"`
}

type Store struct {
	pool *pgxpool.Pool
}

func New(pool *pgxpool.Pool) *Store { return &Store{pool: pool} }

// --- packages -----------------------------------------------------------------

// UpsertPackage inserts or refreshes a tracked package with fresh
// introspection (delete + re-add semantics without losing rules).
func (s *Store) UpsertPackage(ctx context.Context, packageAddress, label, createdBy string, modulesJSON json.RawMessage) error {
	_, err := s.pool.Exec(ctx, `
	INSERT INTO tracked_packages (package_address, label, modules_json, introspected_at, created_by)
	VALUES ($1, $2, $3, now(), $4)
	ON CONFLICT (package_address) DO UPDATE SET
	  label = EXCLUDED.label,
	  modules_json = EXCLUDED.modules_json,
	  introspected_at = now()`,
		packageAddress, label, modulesJSON, createdBy)
	return err
}

func (s *Store) ListPackages(ctx context.Context) ([]TrackedPackage, error) {
	rows, err := s.pool.Query(ctx, `
	SELECT package_address, label, modules_json, introspected_at, created_by, created_at
	FROM tracked_packages ORDER BY created_at`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []TrackedPackage
	for rows.Next() {
		var p TrackedPackage
		if err := rows.Scan(&p.PackageAddress, &p.Label, &p.ModulesJSON, &p.IntrospectedAt, &p.CreatedBy, &p.CreatedAt); err != nil {
			return nil, err
		}
		out = append(out, p)
	}
	return out, rows.Err()
}

// DeletePackage cascades to its rules (FK) and clears its module cursors
// (no FK — cursors key on (package, module) only).
func (s *Store) DeletePackage(ctx context.Context, packageAddress string) error {
	tx, err := s.pool.Begin(ctx)
	if err != nil {
		return err
	}
	defer tx.Rollback(ctx)
	tag, err := tx.Exec(ctx,
		`DELETE FROM tracked_packages WHERE package_address = $1`, packageAddress)
	if err != nil {
		return err
	}
	if tag.RowsAffected() == 0 {
		return ErrNotFound
	}
	if _, err := tx.Exec(ctx,
		`DELETE FROM module_cursors WHERE package_address = $1`, packageAddress); err != nil {
		return err
	}
	return tx.Commit(ctx)
}

// --- rules ---------------------------------------------------------------------

const ruleColumns = `id, package_address, module_name, event_type, label, points,
	recipient_mode, recipient_field, start_mode, start_at, backfill_state,
	backfill_cursor, enabled, created_by, created_at, updated_at`

func scanRule(row pgx.Row) (*Rule, error) {
	var r Rule
	err := row.Scan(&r.ID, &r.PackageAddress, &r.ModuleName, &r.EventType, &r.Label,
		&r.Points, &r.RecipientMode, &r.RecipientField, &r.StartMode, &r.StartAt,
		&r.BackfillState, &r.BackfillCursor, &r.Enabled, &r.CreatedBy,
		&r.CreatedAt, &r.UpdatedAt)
	if errors.Is(err, pgx.ErrNoRows) {
		return nil, ErrNotFound
	}
	if err != nil {
		return nil, err
	}
	return &r, nil
}

// CreateRule validates-agnostic insert; callers own validation.
// start_mode=timestamp sets backfill_state='pending' so the backfill worker
// picks it up; 'tip' starts at the stream tip with no history walk.
func (s *Store) CreateRule(ctx context.Context, r *Rule) (*Rule, error) {
	state := "none"
	if r.StartMode == "timestamp" {
		state = "pending"
	}
	row := s.pool.QueryRow(ctx, `
	INSERT INTO event_rules (package_address, module_name, event_type, label, points,
	                         recipient_mode, recipient_field, start_mode, start_at,
	                         backfill_state, enabled, created_by)
	VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
	RETURNING `+ruleColumns,
		r.PackageAddress, r.ModuleName, r.EventType, r.Label, r.Points,
		r.RecipientMode, r.RecipientField, r.StartMode, r.StartAt,
		state, r.Enabled, r.CreatedBy)
	return scanRule(row)
}

func (s *Store) GetRule(ctx context.Context, id int64) (*Rule, error) {
	return scanRule(s.pool.QueryRow(ctx,
		`SELECT `+ruleColumns+` FROM event_rules WHERE id = $1`, id))
}

func (s *Store) ListRules(ctx context.Context, packageAddress string) ([]*Rule, error) {
	where := ""
	args := []any{}
	if packageAddress != "" {
		where = ` WHERE package_address = $1`
		args = append(args, packageAddress)
	}
	rows, err := s.pool.Query(ctx,
		`SELECT `+ruleColumns+` FROM event_rules`+where+` ORDER BY id`, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []*Rule
	for rows.Next() {
		var r Rule
		if err := rows.Scan(&r.ID, &r.PackageAddress, &r.ModuleName, &r.EventType, &r.Label,
			&r.Points, &r.RecipientMode, &r.RecipientField, &r.StartMode, &r.StartAt,
			&r.BackfillState, &r.BackfillCursor, &r.Enabled, &r.CreatedBy,
			&r.CreatedAt, &r.UpdatedAt); err != nil {
			return nil, err
		}
		out = append(out, &r)
	}
	return out, rows.Err()
}

// RulePatch is a partial rule update; nil fields are left untouched.
type RulePatch struct {
	Label          *string
	Points         *int64
	Enabled        *bool
	RecipientMode  *string
	RecipientField *string
	StartMode      *string
	StartAt        *time.Time
}

// PatchRule applies a partial update. nil fields are left alone; an empty
// patch is a no-op touch. Recipient/start changes re-validate upstream.
func (s *Store) PatchRule(ctx context.Context, id int64, patch RulePatch) (*Rule, error) {
	set, args := []string{"updated_at = now()"}, []any{}
	add := func(col string, v any) {
		args = append(args, v)
		set = append(set, fmt.Sprintf("%s = $%d", col, len(args)))
	}
	if patch.Label != nil {
		add("label", *patch.Label)
	}
	if patch.Points != nil {
		add("points", *patch.Points)
	}
	if patch.Enabled != nil {
		add("enabled", *patch.Enabled)
	}
	if patch.RecipientMode != nil {
		add("recipient_mode", *patch.RecipientMode)
	}
	if patch.RecipientField != nil {
		add("recipient_field", *patch.RecipientField)
	}
	if patch.StartMode != nil {
		add("start_mode", *patch.StartMode)
	}
	if patch.StartAt != nil {
		add("start_at", *patch.StartAt)
		// Re-arming a timestamp start schedules a fresh backfill.
		add("backfill_state", "pending")
	}
	if len(set) == 1 {
		return s.GetRule(ctx, id)
	}
	args = append(args, id)
	row := s.pool.QueryRow(ctx,
		`UPDATE event_rules SET `+strings.Join(set, ", ")+` WHERE id = $`+fmt.Sprint(len(args))+`
		 RETURNING `+ruleColumns, args...)
	return scanRule(row)
}

// DeleteRule removes one rule (deliveries cascade).
func (s *Store) DeleteRule(ctx context.Context, id int64) error {
	tag, err := s.pool.Exec(ctx, `DELETE FROM event_rules WHERE id = $1`, id)
	if err != nil {
		return err
	}
	if tag.RowsAffected() == 0 {
		return ErrNotFound
	}
	return nil
}

// EnabledRulesForModule returns the enabled rules watching pkg::module —
// what each poller tick needs. Rules are re-read every tick so admin edits
// land live without a restart.
func (s *Store) EnabledRulesForModule(ctx context.Context, packageAddress, moduleName string) ([]*Rule, error) {
	rows, err := s.pool.Query(ctx, `
	SELECT `+ruleColumns+` FROM event_rules
	WHERE package_address = $1 AND module_name = $2 AND enabled = true`, packageAddress, moduleName)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []*Rule
	for rows.Next() {
		var r Rule
		if err := rows.Scan(&r.ID, &r.PackageAddress, &r.ModuleName, &r.EventType, &r.Label,
			&r.Points, &r.RecipientMode, &r.RecipientField, &r.StartMode, &r.StartAt,
			&r.BackfillState, &r.BackfillCursor, &r.Enabled, &r.CreatedBy,
			&r.CreatedAt, &r.UpdatedAt); err != nil {
			return nil, err
		}
		out = append(out, &r)
	}
	return out, rows.Err()
}

// ActiveModules lists every distinct (package, module) watched by ≥1 enabled
// rule — the supervisor's stream roster.
func (s *Store) ActiveModules(ctx context.Context) ([][2]string, error) {
	rows, err := s.pool.Query(ctx, `
	SELECT DISTINCT package_address, module_name FROM event_rules WHERE enabled = true ORDER BY 1, 2`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out [][2]string
	for rows.Next() {
		var pkg, mod string
		if err := rows.Scan(&pkg, &mod); err != nil {
			return nil, err
		}
		out = append(out, [2]string{pkg, mod})
	}
	return out, rows.Err()
}

// --- cursors --------------------------------------------------------------------

// LoadCursor returns the raw "{pkg}|{cursor}" value for a module stream.
func (s *Store) LoadCursor(ctx context.Context, packageAddress, moduleName string) (*string, error) {
	var cur *string
	err := s.pool.QueryRow(ctx,
		`SELECT cursor FROM module_cursors WHERE package_address=$1 AND module_name=$2`,
		packageAddress, moduleName).Scan(&cur)
	if errors.Is(err, pgx.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return cur, nil
}

// SaveCursor persists "{pkg}|{cursor}" after a fully-delivered page.
func (s *Store) SaveCursor(ctx context.Context, packageAddress, moduleName, value string) error {
	_, err := s.pool.Exec(ctx, `
	INSERT INTO module_cursors (package_address, module_name, cursor, updated_at)
	VALUES ($1, $2, $3, now())
	ON CONFLICT (package_address, module_name) DO UPDATE SET cursor = EXCLUDED.cursor, updated_at = now()`,
		packageAddress, moduleName, value)
	return err
}

// --- deliveries -------------------------------------------------------------------

// DeliveryClaimed reports whether an idempotency key was already delivered.
func (s *Store) DeliveryClaimed(ctx context.Context, key string) (bool, error) {
	var exists bool
	err := s.pool.QueryRow(ctx,
		`SELECT EXISTS (SELECT 1 FROM deliveries WHERE idempotency_key = $1)`, key).Scan(&exists)
	return exists, err
}

// RecordDelivery inserts the audit row; a duplicate key is a no-op.
func (s *Store) RecordDelivery(ctx context.Context, ruleID int64, key, recipient string, points int64, eventTime time.Time) error {
	_, err := s.pool.Exec(ctx, `
	INSERT INTO deliveries (rule_id, idempotency_key, recipient, points, event_time)
	VALUES ($1, $2, $3, $4, $5)
	ON CONFLICT (idempotency_key) DO NOTHING`,
		ruleID, key, recipient, points, eventTime)
	return err
}

// --- backfill -----------------------------------------------------------------------

// ClaimableBackfills returns rules needing backfill work ('pending' never
// started, or 'running' resumed after restart).
func (s *Store) ClaimableBackfills(ctx context.Context) ([]*Rule, error) {
	rows, err := s.pool.Query(ctx, `
	SELECT `+ruleColumns+` FROM event_rules
	WHERE backfill_state IN ('pending','running') AND enabled = true
	ORDER BY id`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []*Rule
	for rows.Next() {
		var r Rule
		if err := rows.Scan(&r.ID, &r.PackageAddress, &r.ModuleName, &r.EventType, &r.Label,
			&r.Points, &r.RecipientMode, &r.RecipientField, &r.StartMode, &r.StartAt,
			&r.BackfillState, &r.BackfillCursor, &r.Enabled, &r.CreatedBy,
			&r.CreatedAt, &r.UpdatedAt); err != nil {
			return nil, err
		}
		out = append(out, &r)
	}
	return out, rows.Err()
}

// SetBackfillRunning marks a claimed backfill as running.
func (s *Store) SetBackfillRunning(ctx context.Context, ruleID int64) error {
	_, err := s.pool.Exec(ctx,
		`UPDATE event_rules SET backfill_state='running', updated_at=now() WHERE id=$1`, ruleID)
	return err
}

// SaveBackfillProgress persists the per-page descending cursor.
func (s *Store) SaveBackfillProgress(ctx context.Context, ruleID int64, cursor string) error {
	_, err := s.pool.Exec(ctx,
		`UPDATE event_rules SET backfill_cursor=$2, updated_at=now() WHERE id=$1`,
		ruleID, cursor)
	return err
}

// FinishBackfill closes a backfill as done (reached start_at) or exhausted
// (history ended first — public RPC prunes).
func (s *Store) FinishBackfill(ctx context.Context, ruleID int64, state string) error {
	_, err := s.pool.Exec(ctx,
		`UPDATE event_rules SET backfill_state=$2, updated_at=now() WHERE id=$1`, ruleID, state)
	return err
}

// --- status ---------------------------------------------------------------------------

// ModuleStatus is one module stream's freshness for /status.
type ModuleStatus struct {
	PackageAddress  string     `json:"package_address"`
	Module          string     `json:"module"`
	Cursor          *string    `json:"cursor"`
	CursorUpdatedAt *time.Time `json:"cursor_updated_at"`
	LastEventMs     *int64     `json:"last_event_ms"`
	LagMs           *int64     `json:"lag_ms"`
}

// RuleStatus is one rule's delivery health for /status.
type RuleStatus struct {
	RuleID         int64      `json:"rule_id"`
	BackfillState  string     `json:"backfill_state"`
	Delivered      int64      `json:"delivered"`
	LastDeliveryAt *time.Time `json:"last_delivery_at"`
}

// Status aggregates everything the admin UI's ingestion panel shows.
func (s *Store) Status(ctx context.Context) ([]ModuleStatus, []RuleStatus, error) {
	modRows, err := s.pool.Query(ctx, `
	SELECT mc.package_address, mc.module_name, mc.cursor, mc.updated_at
	FROM module_cursors mc ORDER BY mc.package_address, mc.module_name`)
	if err != nil {
		return nil, nil, err
	}
	defer modRows.Close()
	var mods []ModuleStatus
	for modRows.Next() {
		var m ModuleStatus
		if err := modRows.Scan(&m.PackageAddress, &m.Module, &m.Cursor, &m.CursorUpdatedAt); err != nil {
			return nil, nil, err
		}
		mods = append(mods, m)
	}
	if err := modRows.Err(); err != nil {
		return nil, nil, err
	}

	// Freshness per module from delivered event times (max across its rules).
	lastRows, err := s.pool.Query(ctx, `
	SELECT r.package_address, r.module_name, MAX(d.event_time)
	FROM deliveries d JOIN event_rules r ON r.id = d.rule_id
	GROUP BY r.package_address, r.module_name`)
	if err != nil {
		return nil, nil, err
	}
	defer lastRows.Close()
	lastByMod := map[[2]string]time.Time{}
	for lastRows.Next() {
		var pkg, mod string
		var t time.Time
		if err := lastRows.Scan(&pkg, &mod, &t); err != nil {
			return nil, nil, err
		}
		lastByMod[[2]string{pkg, mod}] = t
	}
	if err := lastRows.Err(); err != nil {
		return nil, nil, err
	}
	now := time.Now().UTC()
	for i := range mods {
		key := [2]string{mods[i].PackageAddress, mods[i].Module}
		if t, ok := lastByMod[key]; ok {
			ms := t.UnixMilli()
			lag := now.Sub(t).Milliseconds()
			mods[i].LastEventMs = &ms
			mods[i].LagMs = &lag
		}
	}

	ruleRows, err := s.pool.Query(ctx, `
	SELECT r.id, r.backfill_state, COUNT(d.id), MAX(d.delivered_at)
	FROM event_rules r LEFT JOIN deliveries d ON d.rule_id = r.id
	GROUP BY r.id, r.backfill_state ORDER BY r.id`)
	if err != nil {
		return nil, nil, err
	}
	defer ruleRows.Close()
	var rules []RuleStatus
	for ruleRows.Next() {
		var rs RuleStatus
		var delivered *int64
		if err := ruleRows.Scan(&rs.RuleID, &rs.BackfillState, &delivered, &rs.LastDeliveryAt); err != nil {
			return nil, nil, err
		}
		if delivered != nil {
			rs.Delivered = *delivered
		}
		rules = append(rules, rs)
	}
	return mods, rules, ruleRows.Err()
}
