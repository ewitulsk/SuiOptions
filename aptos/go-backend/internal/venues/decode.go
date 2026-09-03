package venues

import (
	"encoding/hex"
	"fmt"
	"hash"
	"strconv"
	"strings"

	"golang.org/x/crypto/sha3"
)

// This file is intentionally dependency-free (stdlib + x/crypto, which Go
// vendors via the toolchain). Keep it that way: every mapper shares it.

// parseU64 decodes a u64 from REST JSON (string or float64).
func parseU64(v any) (uint64, bool) {
	switch t := v.(type) {
	case string:
		n, err := strconv.ParseUint(strings.TrimSpace(t), 10, 64)
		return n, err == nil
	case float64:
		if t < 0 {
			return 0, false
		}
		return uint64(t), true
	case uint64:
		return t, true
	}
	return 0, false
}

// ParseU64 is the exported form for other packages.
func ParseU64(v any) (uint64, bool) { return parseU64(v) }

// firstVec returns the first element of an Aptos `vec` wrapper:
// {"vec": [...]}. Missing/empty → nil.
func firstVec(v any) any {
	m, ok := v.(map[string]any)
	if !ok {
		return nil
	}
	items, ok := m["vec"].([]any)
	if !ok || len(items) == 0 {
		return nil
	}
	return items[0]
}

// addrInVec extracts {"inner": "0x..."} from a vec-wrapped object field.
func addrInVec(v any) string {
	first := firstVec(v)
	m, ok := first.(map[string]any)
	if !ok {
		return ""
	}
	inner, _ := m["inner"].(string)
	return NormalizeAddr(inner)
}

// AddrInVec is the exported form for other packages.
func AddrInVec(v any) string { return addrInVec(v) }

// NormalizeAddr lowercases and 0x-prefixes a hex address.
func NormalizeAddr(s string) string {
	s = strings.ToLower(strings.TrimSpace(s))
	if s == "" {
		return ""
	}
	if !strings.HasPrefix(s, "0x") {
		s = "0x" + s
	}
	return s
}

// sha3New returns a fresh SHA3-256 hasher.
func sha3New() hash.Hash { return sha3.New256() }

// mustHex32 decodes a 32-byte address, left-padding short mainnet forms.
func mustHex32(s string) []byte {
	s = strings.TrimPrefix(strings.ToLower(strings.TrimSpace(s)), "0x")
	if len(s) < 64 {
		s = strings.Repeat("0", 64-len(s)) + s
	}
	b, err := hex.DecodeString(s)
	if err != nil || len(b) != 32 {
		panic(fmt.Sprintf("venues: bad address %q", s))
	}
	return b
}

// bcsStr encodes a BCS string (u8 length prefix, <128 bytes assert).
func bcsStr(s string) []byte {
	b := []byte(s)
	if len(b) >= 128 {
		panic(fmt.Sprintf("venues: bcs string too long (%d bytes)", len(b)))
	}
	return append([]byte{byte(len(b))}, b...)
}

// FirstVec is the exported form of firstVec.
func FirstVec(v any) any { return firstVec(v) }

// Sha3New is the exported form of sha3New.
func Sha3New() hash.Hash { return sha3New() }

// MustHex32 is the exported form of mustHex32.
func MustHex32(s string) []byte { return mustHex32(s) }

// BcsStr is the exported form of bcsStr.
func BcsStr(s string) []byte { return bcsStr(s) }
