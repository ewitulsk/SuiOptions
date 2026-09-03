package venues

import (
	"encoding/json"
	"fmt"
	"os"
	"strconv"
)

// ParseRESTFile loads a fullnode REST transaction (archival or live) into a
// Transaction. Only version/timestamp/sender/success/events are kept; the
// payload, signatures and changes are irrelevant to mappers.
func ParseRESTFile(path string) (Transaction, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return Transaction{}, err
	}
	var body map[string]any
	if err := json.Unmarshal(raw, &body); err != nil {
		return Transaction{}, fmt.Errorf("venues: parse %s: %w", path, err)
	}
	return ParseRESTTransaction(body)
}

// ParseRESTTransaction converts a decoded REST transaction object.
func ParseRESTTransaction(body map[string]any) (Transaction, error) {
	version, ok := ParseU64(body["version"])
	if !ok {
		return Transaction{}, fmt.Errorf("venues: missing version")
	}
	ts, _ := ParseU64(body["timestamp"])
	sender, _ := body["sender"].(string)
	success := true
	if s, ok := body["success"].(bool); ok {
		success = s
	}
	tx := Transaction{
		Version:         version,
		TimestampMicros: ts,
		Sender:          NormalizeAddr(sender),
		Success:         success,
	}
	rawEvents, _ := body["events"].([]any)
	for i, re := range rawEvents {
		em, ok := re.(map[string]any)
		if !ok {
			continue
		}
		typ, _ := em["type"].(string)
		if typ == "" {
			continue
		}
		data, _ := em["data"].(map[string]any)
		if data == nil {
			data = map[string]any{}
		}
		tx.Events = append(tx.Events, Event{
			Type:           typ,
			SequenceNumber: uint64(i),
			Data:           data,
		})
	}
	return tx, nil
}

// FormatU64 renders for logs; strconv avoids float formatting entirely.
func FormatU64(n uint64) string { return strconv.FormatUint(n, 10) }
