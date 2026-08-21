// Package lbclient posts points to the leaderboard's internal write API.
// The ingestor's side of the at-least-once contract: every delivery carries
// a deterministic idempotency key ("{tx_digest}:{event_seq}:{rule_id}") so
// replays after a crash mid-page are deduped server-side.
package lbclient

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
	"time"
)

type Client struct {
	baseURL string
	http    *http.Client
}

func New(baseURL string) *Client {
	return &Client{
		baseURL: strings.TrimRight(baseURL, "/"),
		http:    &http.Client{Timeout: 10 * time.Second},
	}
}

// PointsRequest mirrors leaderboard POST /internal/points.
type PointsRequest struct {
	Identity       Identity  `json:"identity"`
	Delta          int64     `json:"delta"`
	Source         string    `json:"source"`
	SourceLabel    string    `json:"source_label,omitempty"`
	EventType      string    `json:"event_type,omitempty"`
	IdempotencyKey string    `json:"idempotency_key,omitempty"`
	OccurredAt     time.Time `json:"occurred_at"`
}

type Identity struct {
	Type       string `json:"type"`
	Identifier string `json:"identifier"`
}

// PostPoints applies one points mutation. Returns applied=false when the
// leaderboard reports a duplicate idempotency key (idempotent success).
func (c *Client) PostPoints(ctx context.Context, req PointsRequest) (bool, error) {
	body, err := json.Marshal(req)
	if err != nil {
		return false, err
	}
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost, c.baseURL+"/internal/points", bytes.NewReader(body))
	if err != nil {
		return false, err
	}
	httpReq.Header.Set("content-type", "application/json")
	resp, err := c.http.Do(httpReq)
	if err != nil {
		return false, fmt.Errorf("leaderboard unreachable: %w", err)
	}
	defer resp.Body.Close()
	switch {
	case resp.StatusCode >= 500:
		return false, fmt.Errorf("leaderboard /internal/points → %s", resp.Status)
	case resp.StatusCode != http.StatusOK:
		return false, &PermanentError{Msg: fmt.Sprintf("leaderboard /internal/points → %s", resp.Status)}
	}
	var out struct {
		Applied bool `json:"applied"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&out); err != nil {
		return false, fmt.Errorf("leaderboard response decode: %w", err)
	}
	return out.Applied, nil
}

// PermanentError marks a non-retryable failure (4xx): retrying cannot help,
// so the poller logs + skips instead of stalling its stream.
type PermanentError struct{ Msg string }

func (e *PermanentError) Error() string { return e.Msg }
