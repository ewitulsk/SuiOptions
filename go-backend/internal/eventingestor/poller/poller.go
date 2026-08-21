// Package poller runs one forward stream per module with enabled rules.
//
// Mirrors orderbook/src/sync.rs (per-module streams, cursor table,
// persist-after-page) plus price-charting's exchange_watcher.rs
// ("{package}|{cursor}" self-heal): a republished package orphans the old
// stream position, so a package-half mismatch re-seeds at the tip instead
// of wedging. The cursor only advances after a whole page is delivered — a
// crash mid-page replays that page, and leaderboard idempotency dedupes.
package poller

import (
	"context"
	"fmt"
	"log"
	"sync"
	"time"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promauto"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/extract"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/lbclient"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/eventingestor/store"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/suiaddr"
	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/suigraphql"
)

var (
	pollErrors = promauto.NewCounterVec(prometheus.CounterOpts{
		Name: "ingestor_poll_errors_total",
		Help: "Failed module-stream polls.",
	}, []string{"module"})
	deliveriesTotal = promauto.NewCounterVec(prometheus.CounterOpts{
		Name: "ingestor_deliveries_total",
		Help: "Points deliveries POSTed to the leaderboard.",
	}, []string{"rule", "outcome"}) // outcome: applied|duplicate|error
	unresolvableTotal = promauto.NewCounter(prometheus.CounterOpts{
		Name: "ingestor_recipient_unresolvable_total",
		Help: "Events skipped because no recipient could be extracted.",
	})
	skippedByStart = promauto.NewCounter(prometheus.CounterOpts{
		Name: "ingestor_events_skipped_before_start_total",
		Help: "Events older than a rule's configured start.",
	})
	streamLag = promauto.NewGaugeVec(prometheus.GaugeOpts{
		Name: "ingestor_stream_lag_seconds",
		Help: "Seconds behind chain time of the last delivered event per module stream.",
	}, []string{"module"})
)

type Config struct {
	PollIntervalMs int `toml:"poll_interval_ms"`
	RetryBaseMs    int `toml:"retry_base_ms"`
	RetryCapMs     int `toml:"retry_cap_ms"`
}

type Poller struct {
	cfg Config
	gql *suigraphql.Client
	st  *store.Store
	lb  *lbclient.Client

	mu      sync.Mutex
	running map[[2]string]*streamHandle
}

func New(cfg Config, gql *suigraphql.Client, st *store.Store, lb *lbclient.Client) *Poller {
	if cfg.PollIntervalMs <= 0 {
		cfg.PollIntervalMs = 2000
	}
	if cfg.RetryBaseMs <= 0 {
		cfg.RetryBaseMs = 500
	}
	if cfg.RetryCapMs <= 0 {
		cfg.RetryCapMs = 30000
	}
	return &Poller{cfg: cfg, gql: gql, st: st, lb: lb, running: map[[2]string]*streamHandle{}}
}

type streamHandle struct {
	cancel context.CancelFunc
	done   chan struct{}
}

// Run is the supervisor loop: reconcile the set of running module streams
// with ActiveModules() every tick. Streams are restarted with backoff on the
// next tick after exiting; disabled/deleted modules stop their streams.
// Rules re-read per page inside each stream, so admin changes land live.
func (p *Poller) Run(ctx context.Context) {
	tick := time.NewTicker(5 * time.Second)
	defer tick.Stop()
	for {
		if err := p.reconcile(ctx); err != nil {
			log.Printf("poller supervisor: %v", err)
		}
		select {
		case <-ctx.Done():
			p.stopAll()
			return
		case <-tick.C:
		}
	}
}

func (p *Poller) reconcile(ctx context.Context) error {
	modules, err := p.st.ActiveModules(ctx)
	if err != nil {
		return fmt.Errorf("active modules: %w", err)
	}
	want := map[[2]string]bool{}
	for _, m := range modules {
		want[m] = true
	}

	p.mu.Lock()
	defer p.mu.Unlock()
	for key, h := range p.running {
		if !want[key] {
			h.cancel()
			delete(p.running, key)
		}
	}
	for _, key := range modules {
		if _, ok := p.running[key]; ok {
			continue
		}
		sctx, cancel := context.WithCancel(ctx)
		h := &streamHandle{cancel: cancel, done: make(chan struct{})}
		p.running[key] = h
		go func(key [2]string) {
			defer close(h.done)
			p.runStream(sctx, key[0], key[1])
		}(key)
	}
	return nil
}

func (p *Poller) stopAll() {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, h := range p.running {
		h.cancel()
	}
	p.running = map[[2]string]*streamHandle{}
}

// runStream is one module's forward walk. It never returns while healthy;
// errors back off and continue (a dead GraphQL endpoint must not kill the
// supervisor).
func (p *Poller) runStream(ctx context.Context, pkg, module string) {
	name := shortModule(pkg, module)
	log.Printf("poller: stream %s starting", name)
	backoff := p.baseBackoff()

	cursor, err := p.initCursor(ctx, pkg, module)
	if err != nil {
		log.Printf("poller: %s cursor init failed: %v", name, err)
		pollErrors.WithLabelValues(name).Inc()
	}

	for {
		select {
		case <-ctx.Done():
			return
		default:
		}
		hasMore, err := p.pollPage(ctx, pkg, module, &cursor)
		if ctx.Err() != nil {
			return
		}
		if err != nil {
			pollErrors.WithLabelValues(name).Inc()
			log.Printf("poller: %s page failed: %v; backing off %s", name, err, backoff)
			sleepCtx(ctx, backoff)
			backoff *= 2
			if backoff > p.maxBackoff() {
				backoff = p.maxBackoff()
			}
			continue
		}
		backoff = p.baseBackoff()
		if !hasMore {
			sleepCtx(ctx, time.Duration(p.cfg.PollIntervalMs)*time.Millisecond)
		}
	}
}

func (p *Poller) baseBackoff() time.Duration {
	return time.Duration(p.cfg.RetryBaseMs) * time.Millisecond
}
func (p *Poller) maxBackoff() time.Duration {
	return time.Duration(p.cfg.RetryCapMs) * time.Millisecond
}

// initCursor resumes the persisted position when its package half still
// matches; anything else seeds at the stream tip (descending last:1 → take
// startCursor). No events ever → walk from genesis (nil cursor).
func (p *Poller) initCursor(ctx context.Context, pkg, module string) (*string, error) {
	raw, err := p.st.LoadCursor(ctx, pkg, module)
	if err != nil {
		return nil, err
	}
	if raw != nil {
		if savedPkg, cur, ok := splitTagged(*raw); ok && savedPkg == pkg {
			return &cur, nil
		}
		log.Printf("poller: %s cursor belongs to another package; reseeding at tip", shortModule(pkg, module))
	}
	page, err := p.gql.QueryEvents(ctx, map[string]any{"module": pkg + "::" + module}, nil, 1, true)
	if err != nil {
		return nil, fmt.Errorf("tip query: %w", err)
	}
	return page.Cursor, nil
}

func splitTagged(raw string) (pkg, cursor string, ok bool) {
	for i := 0; i < len(raw); i++ {
		if raw[i] == '|' {
			return raw[:i], raw[i+1:], true
		}
	}
	return "", "", false
}

// pollPage fetches, delivers, and (only after full delivery) persists one
// ascending page. Returns whether more pages are immediately available.
func (p *Poller) pollPage(ctx context.Context, pkg, module string, cursor **string) (bool, error) {
	var curArg *string
	if *cursor != nil {
		curArg = *cursor
	}
	page, err := p.gql.QueryEvents(ctx, map[string]any{"module": pkg + "::" + module}, curArg, suigraphql.PageCap, false)
	if err != nil {
		return false, err
	}

	// Rules re-read every page so admin edits land without a restart.
	rules, err := p.st.EnabledRulesForModule(ctx, pkg, module)
	if err != nil {
		return false, fmt.Errorf("load rules: %w", err)
	}

	for i := range page.Data {
		if err := p.deliverEvent(ctx, pkg, module, rules, page.Data[i]); err != nil {
			return false, err // page not persisted — replayed on retry
		}
	}

	// Persist only after the whole page is delivered. An empty page with a
	// fresh cursor still advances so we don't spin in place.
	if page.Cursor != nil {
		if err := p.st.SaveCursor(ctx, pkg, module, pkg+"|"+*page.Cursor); err != nil {
			return false, fmt.Errorf("save cursor: %w", err)
		}
		*cursor = page.Cursor
	}
	return page.HasMore, nil
}

// deliverEvent matches one event against the module's enabled rules and
// posts points for every hit. Malformed/unattributable events skip with a
// metric — never fatal to the stream.
func (p *Poller) deliverEvent(ctx context.Context, pkg, module string, rules []*store.Rule, ev suigraphql.ChainEvent) error {
	eventTime := time.UnixMilli(int64(ev.TimestampMs)).UTC()
	evType := suiaddr.CanonicalType(ev.TypeRepr)
	matched := false

	for _, rule := range rules {
		if evType != suiaddr.CanonicalType(rule.EventType) {
			continue
		}
		matched = true

		// Start gating: timestamp rules ignore history before start_at; tip
		// rules ignore events older than their creation (module streams are
		// shared across rules).
		switch rule.StartMode {
		case "timestamp":
			if rule.StartAt != nil && eventTime.Before(*rule.StartAt) {
				skippedByStart.Inc()
				continue
			}
		case "tip":
			if eventTime.Before(rule.CreatedAt) {
				skippedByStart.Inc()
				continue
			}
		}

		key := fmt.Sprintf("%s:%d:%d", ev.TxDigest, ev.EventSeq, rule.ID)
		claimed, err := p.st.DeliveryClaimed(ctx, key)
		if err != nil {
			return fmt.Errorf("delivery check: %w", err)
		}
		if claimed {
			continue
		}

		ident, err := extract.Recipient(ev, rule)
		if err != nil {
			unresolvableTotal.Inc()
			log.Printf("poller: skipping event %s:%d for rule %d: %v", ev.TxDigest, ev.EventSeq, rule.ID, err)
			continue
		}

		applied, err := p.postWithRetry(ctx, rule, ident, key, eventTime)
		if err != nil {
			return err // transient — hold the cursor, caller backs off
		}
		outcome := "applied"
		if !applied {
			outcome = "duplicate"
		}
		deliveriesTotal.WithLabelValues(fmt.Sprint(rule.ID), outcome).Inc()

		if err := p.st.RecordDelivery(ctx, rule.ID, key, ident.Identifier, rule.Points, eventTime); err != nil {
			return fmt.Errorf("record delivery: %w", err)
		}
	}

	if matched {
		lagMs := time.Since(eventTime).Milliseconds()
		if lagMs < 0 {
			lagMs = 0
		}
		streamLag.WithLabelValues(shortModule(pkg, module)).Set(float64(lagMs) / 1000)
	}
	return nil
}

// postWithRetry retries transient delivery failures with capped exponential
// backoff. Permanent (4xx-class) failures log + skip: retrying cannot help,
// and one bad delivery must never stall the module's cursor.
func (p *Poller) postWithRetry(ctx context.Context, rule *store.Rule, ident lbclient.Identity, key string, eventTime time.Time) (bool, error) {
	req := lbclient.PointsRequest{
		Identity:       ident,
		Delta:          rule.Points,
		Source:         fmt.Sprintf("rule:%d", rule.ID),
		SourceLabel:    rule.Label,
		EventType:      rule.EventType,
		IdempotencyKey: key,
		OccurredAt:     eventTime,
	}
	backoff := p.baseBackoff()
	maxBackoff := p.maxBackoff()
	for {
		applied, err := p.lb.PostPoints(ctx, req)
		if err == nil {
			return applied, nil
		}
		var perm *lbclient.PermanentError
		if asPermanent(err, &perm) {
			deliveriesTotal.WithLabelValues(fmt.Sprint(rule.ID), "error").Inc()
			log.Printf("poller: permanent delivery failure for rule %d (%s): %v", rule.ID, key, err)
			return false, nil // skip, don't stall
		}
		log.Printf("poller: delivery %s failed (%v); retrying in %s", key, err, backoff)
		sleepCtx(ctx, backoff)
		backoff *= 2
		if backoff > maxBackoff {
			backoff = maxBackoff
		}
		if ctx.Err() != nil {
			return false, ctx.Err()
		}
	}
}

func asPermanent(err error, target **lbclient.PermanentError) bool {
	if e, ok := err.(*lbclient.PermanentError); ok {
		*target = e
		return true
	}
	return false
}

func shortModule(pkg, module string) string {
	if len(pkg) > 10 {
		pkg = pkg[:10]
	}
	return pkg + "::" + module
}

func sleepCtx(ctx context.Context, d time.Duration) {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
	case <-t.C:
	}
}
