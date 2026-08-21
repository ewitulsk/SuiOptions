// Package backfill walks history backwards for timestamp-start rules.
//
// One worker over rules in backfill_state pending|running: a descending
// walk (last/before, page-reversal + startCursor continuation, as in
// sui-tx events.rs) delivering through the same idempotent pipeline as the
// forward poller. Stops done when a page's oldest event predates start_at,
// exhausted when chain history ends first (public RPC prunes — surfaced in
// /status). Throttled to backfill_pages_per_sec to stay under public
// GraphQL rate limits.
package backfill

import (
	"context"
	"fmt"
	"log"
	"time"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/extract"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/lbclient"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/store"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/suiaddr"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/suigraphql"
)

type Worker struct {
	gql       *suigraphql.Client
	st        *store.Store
	lb        *lbclient.Client
	pageDelay time.Duration
}

func New(gql *suigraphql.Client, st *store.Store, lb *lbclient.Client, pagesPerSec int) *Worker {
	if pagesPerSec <= 0 {
		pagesPerSec = 2
	}
	return &Worker{gql: gql, st: st, lb: lb, pageDelay: time.Second / time.Duration(pagesPerSec)}
}

// Run scans for claimable backfills once per interval; at most one rule is
// walked at a time (one worker — keeps the public GraphQL quota sane).
func (w *Worker) Run(ctx context.Context) {
	tick := time.NewTicker(10 * time.Second)
	defer tick.Stop()
	for {
		rules, err := w.st.ClaimableBackfills(ctx)
		if err != nil {
			log.Printf("backfill: scan: %v", err)
		}
		for _, rule := range rules {
			select {
			case <-ctx.Done():
				return
			default:
			}
			if err := w.backfillRule(ctx, rule); err != nil {
				log.Printf("backfill: rule %d: %v", rule.ID, err)
			}
		}
		select {
		case <-ctx.Done():
			return
		case <-tick.C:
		}
	}
}

func (w *Worker) backfillRule(ctx context.Context, rule *store.Rule) error {
	if rule.StartMode != "timestamp" || rule.StartAt == nil {
		// Nothing to walk to; close it out so it doesn't spin forever.
		return w.st.FinishBackfill(ctx, rule.ID, "done")
	}
	if err := w.st.SetBackfillRunning(ctx, rule.ID); err != nil {
		return fmt.Errorf("claim: %w", err)
	}
	var before *string
	resumeFrom := "tip"
	if rule.BackfillCursor != nil {
		before = rule.BackfillCursor
		resumeFrom = *rule.BackfillCursor
	}
	log.Printf("backfill: rule %d (%s) starting from %s", rule.ID, rule.EventType, resumeFrom)

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}

		page, err := w.gql.QueryEvents(ctx, map[string]any{"type": rule.EventType}, before, suigraphql.PageCap, true)
		if err != nil {
			// Leave state=running; the next scan resumes from the persisted cursor.
			return fmt.Errorf("query: %w", err)
		}

		if len(page.Data) > 0 {
			if err := w.deliverPage(ctx, rule, page.Data); err != nil {
				return err
			}
		}

		// Persist progress per page (startCursor = newest event of the page).
		if page.Cursor != nil {
			if err := w.st.SaveBackfillProgress(ctx, rule.ID, *page.Cursor); err != nil {
				return fmt.Errorf("save progress: %w", err)
			}
			before = page.Cursor
		}

		oldest := time.Time{}
		if len(page.Data) > 0 {
			// Data is reversed to ascending; first element is the oldest.
			oldest = time.UnixMilli(int64(page.Data[0].TimestampMs))
		}

		switch {
		case len(page.Data) == 0:
			return w.st.FinishBackfill(ctx, rule.ID, "exhausted")
		case !page.HasMore:
			// Reached the beginning of recorded history. Public RPCs prune;
			// that's expected and surfaced in /status.
			if oldest.Before(*rule.StartAt) || oldest.Equal(*rule.StartAt) {
				return w.st.FinishBackfill(ctx, rule.ID, "done")
			}
			return w.st.FinishBackfill(ctx, rule.ID, "exhausted")
		case !oldest.IsZero() && oldest.Before(*rule.StartAt):
			return w.st.FinishBackfill(ctx, rule.ID, "done")
		}

		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(w.pageDelay):
		}
	}
}

// deliverPage runs the same pipeline as the forward poller (idempotency keys
// make overlap at the seed point harmless). Unresolvable recipients skip.
func (w *Worker) deliverPage(ctx context.Context, rule *store.Rule, events []suigraphql.ChainEvent) error {
	evType := suiaddr.CanonicalType(rule.EventType)
	for _, ev := range events {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}
		if suiaddr.CanonicalType(ev.TypeRepr) != evType {
			continue // module-filtered pages are exact, but be safe
		}
		eventTime := time.UnixMilli(int64(ev.TimestampMs)).UTC()

		key := fmt.Sprintf("%s:%d:%d", ev.TxDigest, ev.EventSeq, rule.ID)
		claimed, err := w.st.DeliveryClaimed(ctx, key)
		if err != nil {
			return fmt.Errorf("delivery check: %w", err)
		}
		if claimed {
			continue
		}

		ident, err := extract.Recipient(ev, rule)
		if err != nil {
			log.Printf("backfill: skipping event %s:%d for rule %d: %v", ev.TxDigest, ev.EventSeq, rule.ID, err)
			continue
		}

		applied, err := w.lb.PostPoints(ctx, lbclient.PointsRequest{
			Identity:       ident,
			Delta:          rule.Points,
			Source:         fmt.Sprintf("rule:%d", rule.ID),
			SourceLabel:    rule.Label,
			EventType:      rule.EventType,
			IdempotencyKey: key,
			OccurredAt:     eventTime,
		})
		if err != nil {
			return fmt.Errorf("post points: %w", err) // retry whole page on next scan
		}
		_ = applied
		if err := w.st.RecordDelivery(ctx, rule.ID, key, ident.Identifier, rule.Points, eventTime); err != nil {
			return fmt.Errorf("record delivery: %w", err)
		}

		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(w.pageDelay / 4): // gentle pacing between posts too
		}
	}
	return nil
}
