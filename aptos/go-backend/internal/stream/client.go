// Package stream polls the fullnode REST API for new transactions and
// converts them for venue mappers. REST-first by plan §4.7 (public fullnode
// prunes history, so backfill goes to the archival endpoint): the gRPC
// Transaction Stream upgrade only changes this package's internals — the
// venues.Transaction shape it produces is already stream-shaped.
package stream

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"time"

	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues"
)

// Client polls `<base>/transactions?start=<version>&limit=N`.
type Client struct {
	base   string
	http   *http.Client
	limit  int
	backoff time.Duration
}

// New returns a Client for a fullnode base URL (live or archival).
func New(base string) *Client {
	return &Client{
		base:    base,
		http:    &http.Client{Timeout: 30 * time.Second},
		limit:   100,
		backoff: 2 * time.Second,
	}
}

// LatestVersion returns the ledger tip.
func (c *Client) LatestVersion(ctx context.Context) (uint64, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, c.base+"/", nil)
	if err != nil {
		return 0, err
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return 0, err
	}
	defer resp.Body.Close()
	var info struct {
		LedgerVersion string `json:"ledger_version"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&info); err != nil {
		return 0, err
	}
	return strconv.ParseUint(info.LedgerVersion, 10, 64)
}

// Fetch returns up to limit transactions starting at version (inclusive).
func (c *Client) Fetch(ctx context.Context, start uint64) ([]venues.Transaction, error) {
	url := fmt.Sprintf("%s/transactions?start=%d&limit=%d", c.base, start, c.limit)
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Accept", "application/json")
	resp, err := c.http.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode == http.StatusGone {
		return nil, fmt.Errorf("stream: version %d pruned; backfill from archival endpoint", start)
	}
	if resp.StatusCode != http.StatusOK {
		body, _ := io.ReadAll(io.LimitReader(resp.Body, 1024))
		return nil, fmt.Errorf("stream: GET %s: %d %s", url, resp.StatusCode, body)
	}
	var bodies []map[string]any
	if err := json.NewDecoder(resp.Body).Decode(&bodies); err != nil {
		return nil, err
	}
	out := make([]venues.Transaction, 0, len(bodies))
	for _, b := range bodies {
		tx, err := venues.ParseRESTTransaction(b)
		if err != nil {
			continue
		}
		out = append(out, tx)
	}
	return out, nil
}

// Backoff sleeps between polls when the indexer is at the tip.
func (c *Client) Backoff(ctx context.Context) {
	t, cancel := context.WithTimeout(ctx, c.backoff)
	defer cancel()
	<-t.Done()
}
