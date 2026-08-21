// Package suigraphql is a minimal Sui GraphQL RPC client.
//
// JSON-RPC is dead on Sui fullnodes (July 2026); everything here rides the
// GraphQL endpoint. The events query is a direct port of
// rust-backend/crates/sui-tx/src/events.rs (page cap 50, opaque cursors,
// descending = last/before with the server's ascending page reversed). The
// package-introspection queries power the ingestor's admin flow.
package suigraphql

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"

	"github.com/ewitulsk/SuiOptions/go-backend/internal/platform/suiaddr"
)

// PageCap is the GraphQL events-query page size limit. Observed live:
// "Page size is too large: 100 > 50" — every poll failed (see orderbook
// sync.rs).
const PageCap = 50

// ChainEvent is one event node, flattened from the GraphQL shape.
type ChainEvent struct {
	// TxDigest of the enclosing transaction.
	TxDigest string
	// EventSeq is the event's index within its transaction
	// (GraphQL sequenceNumber).
	EventSeq uint64
	// TypeRepr is contents.type.repr, e.g. `0x…::settlement::FillEvent`.
	TypeRepr string
	// JSON is contents.json verbatim (raw bytes).
	JSON json.RawMessage
	// Sender is the transaction sender's address as rendered by the node
	// (may be short-form; normalize before storing/comparing).
	Sender string
	// TimestampMs is the event time in unix milliseconds.
	TimestampMs uint64
	// Module is transactionModule.name (the emitting module's name only,
	// without package).
	Module string
}

// EventPage mirrors sui-tx's EventPage: data + opaque cursor + continuation.
type EventPage struct {
	Data []ChainEvent
	// Cursor is the opaque cursor to resume from (endCursor going forward,
	// startCursor going backward). nil when the page carried none.
	Cursor *string
	// HasMore is hasNextPage (forward) / hasPreviousPage (backward).
	HasMore bool
}

type Client struct {
	url string
	hc  *http.Client
}

func New(url string) *Client {
	return &Client{
		url: strings.TrimRight(url, "/"),
		hc:  &http.Client{Timeout: 30 * time.Second},
	}
}

// --- wire types -------------------------------------------------------------

type graphqlRequest struct {
	Query     string         `json:"query"`
	Variables map[string]any `json:"variables,omitempty"`
}

type graphqlError struct {
	Message string `json:"message"`
}

type graphqlResponse struct {
	Data   json.RawMessage `json:"data"`
	Errors []graphqlError  `json:"errors"`
}

// query POSTs one GraphQL operation and decodes data into out.
func (c *Client) query(ctx context.Context, q string, vars map[string]any, out any) error {
	body, err := json.Marshal(graphqlRequest{Query: q, Variables: vars})
	if err != nil {
		return err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.url, bytes.NewReader(body))
	if err != nil {
		return err
	}
	req.Header.Set("content-type", "application/json")
	resp, err := c.hc.Do(req)
	if err != nil {
		return fmt.Errorf("sui graphql transport: %w", err)
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(io.LimitReader(resp.Body, 32<<20))
	if err != nil {
		return fmt.Errorf("sui graphql read: %w", err)
	}
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("sui graphql → %s: %.200s", resp.Status, raw)
	}
	var gr graphqlResponse
	if err := json.Unmarshal(raw, &gr); err != nil {
		return fmt.Errorf("sui graphql decode: %w", err)
	}
	if len(gr.Errors) > 0 {
		msgs := make([]string, 0, len(gr.Errors))
		for _, e := range gr.Errors {
			msgs = append(msgs, e.Message)
		}
		return fmt.Errorf("sui graphql errors: %s", strings.Join(msgs, "; "))
	}
	if out != nil {
		if err := json.Unmarshal(gr.Data, out); err != nil {
			return fmt.Errorf("sui graphql data decode: %w", err)
		}
	}
	return nil
}

// --- events ------------------------------------------------------------------

const eventsQueryAsc = `query($f: EventFilter!, $n: Int!, $c: String) {
  events(filter: $f, first: $n, after: $c) {
    pageInfo { hasNextPage endCursor }
    nodes { sequenceNumber timestamp transactionModule { name }
            sender { address } transaction { digest }
            contents { type { repr } json } }
  }
}`

const eventsQueryDesc = `query($f: EventFilter!, $n: Int!, $c: String) {
  events(filter: $f, last: $n, before: $c) {
    pageInfo { hasPreviousPage startCursor }
    nodes { sequenceNumber timestamp transactionModule { name }
            sender { address } transaction { digest }
            contents { type { repr } json } }
  }
}`

type eventsData struct {
	Events struct {
		PageInfo struct {
			HasNextPage     bool    `json:"hasNextPage"`
			EndCursor       *string `json:"endCursor"`
			HasPreviousPage bool    `json:"hasPreviousPage"`
			StartCursor     *string `json:"startCursor"`
		} `json:"pageInfo"`
		Nodes []eventNode `json:"nodes"`
	} `json:"events"`
}

type eventNode struct {
	SequenceNumber    any    `json:"sequenceNumber"`
	Timestamp         string `json:"timestamp"`
	TransactionModule struct {
		Name string `json:"name"`
	} `json:"transactionModule"`
	Sender struct {
		Address string `json:"address"`
	} `json:"sender"`
	Transaction struct {
		Digest string `json:"digest"`
	} `json:"transaction"`
	Contents struct {
		Type struct {
			Repr string `json:"repr"`
		} `json:"type"`
		JSON json.RawMessage `json:"json"`
	} `json:"contents"`
}

// QueryEvents fetches one page of events filtered by module ("pkg::mod") or
// full type tag. descending=true walks backwards from the newest (last/
// before); the server always returns a page in ascending order, so that path
// reverses it — matching sui-tx events.rs semantics. A node that does not
// parse is skipped rather than failing the page: a single malformed event
// must not stall a watcher loop.
func (c *Client) QueryEvents(ctx context.Context, filter map[string]any, cursor *string, limit int, descending bool) (*EventPage, error) {
	if limit > PageCap {
		limit = PageCap
	}
	q := eventsQueryAsc
	if descending {
		q = eventsQueryDesc
	}
	vars := map[string]any{"f": filter, "n": limit, "c": cursor}
	var data eventsData
	if err := c.query(ctx, q, vars, &data); err != nil {
		return nil, err
	}

	events := make([]ChainEvent, 0, len(data.Events.Nodes))
	for _, n := range data.Events.Nodes {
		ev, err := parseEvent(n)
		if err != nil {
			continue // malformed node — skip, never fatal
		}
		events = append(events, ev)
	}

	page := &EventPage{Data: events}
	if descending {
		// Reverse to oldest-first so callers process in chain order.
		for i, j := 0, len(events)-1; i < j; i, j = i+1, j-1 {
			events[i], events[j] = events[j], events[i]
		}
		page.HasMore = data.Events.PageInfo.HasPreviousPage
		page.Cursor = data.Events.PageInfo.StartCursor
	} else {
		page.HasMore = data.Events.PageInfo.HasNextPage
		page.Cursor = data.Events.PageInfo.EndCursor
	}
	return page, nil
}

func parseEvent(n eventNode) (ChainEvent, error) {
	if n.Transaction.Digest == "" || n.Contents.Type.Repr == "" || len(n.Contents.JSON) == 0 {
		return ChainEvent{}, fmt.Errorf("event node missing digest/type/json")
	}
	seq, ok := toUint64(n.SequenceNumber)
	if !ok {
		return ChainEvent{}, fmt.Errorf("event node bad sequenceNumber")
	}
	ts, err := parseRFC3339Millis(n.Timestamp)
	if err != nil {
		return ChainEvent{}, fmt.Errorf("event node bad timestamp: %w", err)
	}
	return ChainEvent{
		TxDigest:    n.Transaction.Digest,
		EventSeq:    seq,
		TypeRepr:    n.Contents.Type.Repr,
		JSON:        n.Contents.JSON,
		Sender:      n.Sender.Address,
		TimestampMs: ts,
		Module:      n.TransactionModule.Name,
	}, nil
}

// GraphQL renders u64 as a JSON number OR a decimal string depending on
// magnitude; accept both.
func toUint64(v any) (uint64, bool) {
	switch x := v.(type) {
	case float64:
		if x < 0 || x != float64(uint64(x)) {
			return 0, false
		}
		return uint64(x), true
	case string:
		var n uint64
		_, err := fmt.Sscanf(x, "%d", &n)
		return n, err == nil
	case json.Number:
		var n uint64
		_, err := fmt.Sscanf(x.String(), "%d", &n)
		return n, err == nil
	default:
		return 0, false
	}
}

func parseRFC3339Millis(s string) (uint64, error) {
	t, err := time.Parse(time.RFC3339Nano, s)
	if err != nil {
		return 0, err
	}
	ms := t.UnixMilli()
	if ms < 0 {
		return 0, fmt.Errorf("negative timestamp")
	}
	return uint64(ms), nil
}

// --- package introspection -----------------------------------------------------

// StructField is one field of an introspected Move struct.
type StructField struct {
	Name string `json:"name"`
	Repr string `json:"repr"` // type.repr
}

// StructDef is one candidate struct in a module.
type StructDef struct {
	Name      string        `json:"name"`
	Abilities []string      `json:"abilities"`
	Fields    []StructField `json:"fields"`
}

// ModuleDef is one module of an introspected package.
type ModuleDef struct {
	Name    string      `json:"name"`
	Structs []StructDef `json:"structs"`
}

// PackageDef is the full introspection cached in tracked_packages.modules_json.
type PackageDef struct {
	Package string      `json:"package"`
	Modules []ModuleDef `json:"modules"`
}

// IsCandidateEvent applies the emit-bound heuristic: abilities include COPY
// and DROP and exclude KEY (what `event::emit<T>()` requires). GraphQL has
// no "is event" marker — the admin confirms from the candidates.
func (s StructDef) IsCandidateEvent() bool {
	var copy, drop, key bool
	for _, a := range s.Abilities {
		switch strings.ToLower(a) {
		case "copy":
			copy = true
		case "drop":
			drop = true
		case "key":
			key = true
		}
	}
	return copy && drop && !key
}

type pkgModulesData struct {
	Object struct {
		AsMovePackage *struct {
			Modules struct {
				PageInfo struct {
					HasNextPage bool    `json:"hasNextPage"`
					EndCursor   *string `json:"endCursor"`
				} `json:"pageInfo"`
				Nodes []struct {
					Name string `json:"name"`
				} `json:"nodes"`
			} `json:"modules"`
		} `json:"asMovePackage"`
	} `json:"object"`
}

// IntrospectPackage walks a published package: modules first (paginated),
// then each module's structs with abilities + typed fields (paginated).
// Returns an error naming the cause when the address is unknown or carries
// no modules.
func (c *Client) IntrospectPackage(ctx context.Context, packageAddress string) (*PackageDef, error) {
	out := &PackageDef{Package: packageAddress}

	const modulesQuery = `query($a: SuiAddress!, $n: Int!, $c: String) {
	  object(address: $a) {
	    asMovePackage {
	      modules(first: $n, after: $c) {
	        pageInfo { hasNextPage endCursor }
	        nodes { name }
	      }
	    }
	  }
	}`

	var cursor *string
	for {
		var d pkgModulesData
		vars := map[string]any{"a": packageAddress, "n": PageCap, "c": cursor}
		if err := c.query(ctx, modulesQuery, vars, &d); err != nil {
			return nil, err
		}
		mp := d.Object.AsMovePackage
		if mp == nil {
			return nil, fmt.Errorf("object %s is not a Move package", packageAddress)
		}
		for _, m := range mp.Modules.Nodes {
			out.Modules = append(out.Modules, ModuleDef{Name: m.Name})
		}
		if !mp.Modules.PageInfo.HasNextPage || mp.Modules.PageInfo.EndCursor == nil {
			break
		}
		cursor = mp.Modules.PageInfo.EndCursor
	}
	if len(out.Modules) == 0 {
		return nil, fmt.Errorf("package %s exposes no modules", packageAddress)
	}

	const structsQuery = `query($a: SuiAddress!, $m: String!, $n: Int!, $c: String) {
	  object(address: $a) {
	    asMovePackage {
	      module(name: $m) {
	        structs(first: $n, after: $c) {
	          pageInfo { hasNextPage endCursor }
	          nodes {
	            name
	            abilities
	            fields { name type { repr } }
	          }
	        }
	      }
	    }
	  }
	}`

	for i := range out.Modules {
		mod := &out.Modules[i]
		var scursor *string
		for {
			var sd struct {
				Object struct {
					AsMovePackage *struct {
						Module *struct {
							Structs struct {
								PageInfo struct {
									HasNextPage bool    `json:"hasNextPage"`
									EndCursor   *string `json:"endCursor"`
								} `json:"pageInfo"`
								Nodes []struct {
									Name      string   `json:"name"`
									Abilities []string `json:"abilities"`
									Fields    []struct {
										Name string `json:"name"`
										Type struct {
											Repr string `json:"repr"`
										} `json:"type"`
									} `json:"fields"`
								} `json:"nodes"`
							} `json:"structs"`
						} `json:"module"`
					} `json:"asMovePackage"`
				} `json:"object"`
			}
			vars := map[string]any{"a": packageAddress, "m": mod.Name, "n": PageCap, "c": scursor}
			if err := c.query(ctx, structsQuery, vars, &sd); err != nil {
				return nil, fmt.Errorf("structs(%s::%s): %w", packageAddress, mod.Name, err)
			}
			mp := sd.Object.AsMovePackage
			if mp == nil || mp.Module == nil {
				return nil, fmt.Errorf("module %s::%s not found", packageAddress, mod.Name)
			}
			for _, sn := range mp.Module.Structs.Nodes {
				def := StructDef{Name: sn.Name, Abilities: sn.Abilities}
				for _, f := range sn.Fields {
					def.Fields = append(def.Fields, StructField{Name: f.Name, Repr: f.Type.Repr})
				}
				mod.Structs = append(mod.Structs, def)
			}
			if !mp.Module.Structs.PageInfo.HasNextPage || mp.Module.Structs.PageInfo.EndCursor == nil {
				break
			}
			scursor = mp.Module.Structs.PageInfo.EndCursor
		}
	}
	return out, nil
}

// Candidate is a candidate-event struct with its owning module.
type Candidate struct {
	Module string
	Name   string
	Struct StructDef
}

// CanonicalType is the full type tag `0x<pkg>::<module>::<Name>` for this
// candidate, in canonical (padded) form.
func (c Candidate) CanonicalType(packageAddress string) string {
	return suiaddr.Normalize(packageAddress) + "::" + c.Module + "::" + c.Name
}

// CandidateEvents returns every candidate-event struct across the package.
func (p *PackageDef) CandidateEvents() []Candidate {
	var out []Candidate
	for _, m := range p.Modules {
		for _, s := range m.Structs {
			if s.IsCandidateEvent() {
				out = append(out, Candidate{Module: m.Name, Name: s.Name, Struct: s})
			}
		}
	}
	return out
}

// FindStruct locates a struct by module + name inside the introspection.
func (p *PackageDef) FindStruct(moduleName, structName string) (StructDef, bool) {
	for _, m := range p.Modules {
		if m.Name != moduleName {
			continue
		}
		for _, s := range m.Structs {
			if s.Name == structName {
				return s, true
			}
		}
	}
	return StructDef{}, false
}
