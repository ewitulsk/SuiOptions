// Package venues defines the venue-mapper contract: every marketplace venue
// turns raw chain transactions into normalized activities. One mapper per
// venue family; the registry fans transactions out to all of them.
package venues

// Transaction is the minimal view of a chain transaction the mappers need.
// The stream client builds it from REST today and from the gRPC Transaction
// Stream tomorrow without changing any mapper.
type Transaction struct {
	Version          uint64
	TimestampMicros  uint64
	Sender           string
	Success          bool
	Events           []Event
}

// Event is a single chain event with decoded JSON data.
type Event struct {
	// Full type, e.g. "0x584b...::events::ListingPlacedEvent".
	Type string
	// SequenceNumber is the event index within the transaction.
	SequenceNumber uint64
	// Data is the decoded `data` object.
	Data map[string]any
}

// Standard event kinds shared by every venue.
const (
	KindCreated   = "created"
	KindCancelled = "cancelled"
	KindFilled    = "filled"
	KindOffer     = "offer"
)

// Activity is one normalized marketplace action.
type Activity struct {
	Version     uint64
	EventIndex  uint64
	TimestampUs uint64
	Marketplace string
	Kind        string
	// RawEventType is the full chain event type, for forensics.
	RawEventType string
	// ListingID is the listing/offer object address, or the v1 TokenId hash.
	ListingID string
	// TokenDataID is the v2 token object address, or the v1 TokenDataId hash.
	TokenDataID string
	Creator     string
	Collection  string
	TokenName   string
	PropertyVer *uint64
	Price       *uint64
	QuoteToken  string
	Buyer       string
	Seller      string
	Commission  *uint64
	Royalty     *uint64
	Remaining   *uint64
}

// Mapper turns transactions into normalized activities for one venue family.
type Mapper interface {
	// Marketplace is the canonical venue slug (wapal, rarible, ...).
	Marketplace() string
	// ContractAddress is the venue's on-chain package address.
	ContractAddress() string
	// Map extracts activities; unparseable events are skipped, never fatal.
	Map(tx Transaction) ([]Activity, error)
}

// SplitType splits "0xaddr::module::Name" into its parts.
func SplitType(t string) (addr, module, name string, ok bool) {
	parts := splitN(t, "::", 3)
	if len(parts) != 3 {
		return "", "", "", false
	}
	return parts[0], parts[1], parts[2], true
}

func splitN(s, sep string, n int) []string {
	var out []string
	for len(out) < n-1 {
		i := indexOf(s, sep)
		if i < 0 {
			break
		}
		out = append(out, s[:i])
		s = s[i+len(sep):]
	}
	return append(out, s)
}

func indexOf(s, sep string) int {
	for i := 0; i+len(sep) <= len(s); i++ {
		if s[i:i+len(sep)] == sep {
			return i
		}
	}
	return -1
}
