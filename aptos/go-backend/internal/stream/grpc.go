// Package stream gRPC client: follows the Aptos Transaction Stream
// (plan §4.7) instead of polling REST. The server pushes only transactions
// matching our venue event filter, so a quiet tip is a quiet stream and
// catch-up is a fast replay — REST polling caps at ~100 txs per
// ~2s round-trip per connection and cannot track mainnet (~190 tps).
package stream

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"strings"

	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials"
	"google.golang.org/grpc/metadata"

	indexerv1 "github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/indexerpb/streamv1"
	txnv1 "github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/indexerpb/txnv1"
	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/venues"
)

// DefaultGRPCEndpoint is the Labs-hosted mainnet Transaction Stream Service.
const DefaultGRPCEndpoint = "grpc.mainnet.aptoslabs.com:443"

// Stream auth mirrors the processor SDK: Bearer [REDACTED] plus a request-name
// identifying this destination.
const (
	authHeader        = "authorization"
	requestNameHeader = "x-aptos-request-name"
	requestName       = "nft-indexer"
)

// GRPCClient streams filtered transactions and maps them to
// venues.Transaction, the same shape the REST client produces — mappers
// are transport-agnostic.
type GRPCClient struct {
	endpoint string
	apiKey   string
	addrs    []string
	batch    uint64
	conn     *grpc.ClientConn
	raw      indexerv1.RawDataClient
}

// NewGRPC dials the Transaction Stream Service. addrs are the venue
// contract addresses to filter server-side (mappers' ContractAddress);
// an empty apiKey dials without auth (the hosted service rejects it).
func NewGRPC(endpoint, apiKey string, addrs []string) (*GRPCClient, error) {
	if endpoint == "" {
		endpoint = DefaultGRPCEndpoint
	}
	conn, err := grpc.NewClient(endpoint,
		grpc.WithTransportCredentials(credentials.NewTLS(nil)),
	)
	if err != nil {
		return nil, fmt.Errorf("stream: grpc dial %s: %w", endpoint, err)
	}
	lowered := make([]string, 0, len(addrs))
	for _, a := range addrs {
		if a = strings.ToLower(a); a != "" {
			lowered = append(lowered, a)
		}
	}
	return &GRPCClient{
		endpoint: endpoint,
		apiKey:   apiKey,
		addrs:    lowered,
		batch:    100,
		conn:     conn,
		raw:      indexerv1.NewRawDataClient(conn),
	}, nil
}

// Close releases the connection.
func (c *GRPCClient) Close() error { return c.conn.Close() }

// eventFilter ORs one EventFilter per venue address: reference venues only
// emit module "events", Tradeport uses listings/listings_v2, so its filter
// leaves the module open (the mapper decides).
func (c *GRPCClient) eventFilter() *indexerv1.BooleanTransactionFilter {
	or := &indexerv1.LogicalOrFilters{}
	for _, a := range c.addrs {
		addr := a
		st := &indexerv1.MoveStructTagFilter{Address: &addr}
		if !strings.EqualFold(a, venues.AddrTradeport) {
			st.Module = &[]string{"events"}[0]
		}
		or.Filters = append(or.Filters, &indexerv1.BooleanTransactionFilter{
			Filter: &indexerv1.BooleanTransactionFilter_ApiFilter{
				ApiFilter: &indexerv1.APIFilter{
					Filter: &indexerv1.APIFilter_EventFilter{
						EventFilter: &indexerv1.EventFilter{StructType: st},
					},
				},
			},
		})
	}
	return &indexerv1.BooleanTransactionFilter{
		Filter: &indexerv1.BooleanTransactionFilter_LogicalOr{LogicalOr: or},
	}
}

// Stream opens an infinite filtered stream at start (inclusive) and calls
// apply for every response batch in order. It returns only on error or
// context cancel; resume from the last applied version.
func (c *GRPCClient) Stream(ctx context.Context, start uint64, apply func([]venues.Transaction) (uint64, error)) error {
	ctx = metadata.AppendToOutgoingContext(ctx,
		authHeader, "Bearer "+c.apiKey, requestNameHeader, requestName)
	req := &indexerv1.GetTransactionsRequest{
		StartingVersion:   &start,
		BatchSize:         &c.batch,
		TransactionFilter: c.eventFilter(),
	}
	stream, err := c.raw.GetTransactions(ctx, req)
	if err != nil {
		return fmt.Errorf("stream: grpc GetTransactions: %w", err)
	}
	for {
		resp, err := stream.Recv()
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return fmt.Errorf("stream: grpc recv: %w", err)
		}
		txs := make([]venues.Transaction, 0, len(resp.Transactions))
		for _, t := range resp.Transactions {
			// Always appended (even unmappable) so the caller cursor
			// advances past filtered versions uniformly.
			tx, _ := mapProtoTransaction(t)
			txs = append(txs, tx)
		}
		if len(txs) == 0 {
			continue
		}
		if _, err := apply(txs); err != nil {
			return err
		}
	}
}

// mapProtoTransaction converts one stream transaction. The second return
// is false for non-user transactions (no mappable events); versions still
// advance the caller cursor past them.
func mapProtoTransaction(t *txnv1.Transaction) (venues.Transaction, bool) {
	u := t.GetUser()
	if u == nil {
		return venues.Transaction{Version: t.GetVersion()}, false
	}
	tx := venues.Transaction{
		Version:         t.GetVersion(),
		TimestampMicros: uint64(t.GetTimestamp().GetSeconds())*1e6 + uint64(t.GetTimestamp().GetNanos())/1e3,
		Sender:          venues.NormalizeAddr(u.GetRequest().GetSender()),
		Success:         t.GetInfo().GetSuccess(),
	}
	for _, e := range u.GetEvents() {
		typ := e.GetTypeStr()
		if typ == "" {
			continue
		}
		var data map[string]any
		if err := json.Unmarshal([]byte(e.GetData()), &data); err != nil || data == nil {
			continue
		}
		tx.Events = append(tx.Events, venues.Event{
			Type:           typ,
			SequenceNumber: e.GetSequenceNumber(),
			Data:           data,
		})
	}
	return tx, true
}
