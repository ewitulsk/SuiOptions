package suiaddr

import "testing"

const hex64 = "ab8d1b5a5311c9400e3eaf5c3b641f10fb48b43cc30d365fa8a98a6ca6bd4865"

func TestNormalizePadsAndLowercases(t *testing.T) {
	want := "0x00000000000000000000000000000000000000000000000000000000000000ab"
	if got := Normalize("0xAB"); got != want {
		t.Fatalf("Normalize(0xAB) = %q, want %q", got, want)
	}
	if got := Normalize("ab"); got != want {
		t.Fatalf("Normalize(ab) = %q, want %q", got, want)
	}
	full := "0xAB8D1B5A5311C9400E3EAF5C3B641F10FB48B43CC30D365FA8A98A6CA6BD4865"
	lower := "0x" + hex64
	if Normalize(full) != lower {
		t.Fatalf("full-width address mangled: %q", Normalize(full))
	}
}

func TestValidHex(t *testing.T) {
	for _, ok := range []string{"0xab", "AB", "0x" + hex64} {
		if !ValidHex(ok) {
			t.Errorf("ValidHex(%q) = false, want true", ok)
		}
	}
	for _, bad := range []string{"", "0x", "xyz", "0xzz", "0x" + hex64 + "a"} {
		if ValidHex(bad) {
			t.Errorf("ValidHex(%q) = true, want false", bad)
		}
	}
}

func TestCanonicalType(t *testing.T) {
	long := "0x" + hex64 + "::settlement::FillEvent"
	shortPadded := "0x00000000000000000000000000000000000000000000000000000000000000ab::settlement::FillEvent"

	if CanonicalType(long) != long {
		t.Fatalf("CanonicalType(long) = %q", CanonicalType(long))
	}
	if got := CanonicalType("0xAB::settlement::FillEvent"); got != shortPadded {
		t.Fatalf("short pkg not padded: %q", got)
	}
	if got := CanonicalType("0xAB::settlement::FillEvent<0x2::sui::SUI>"); got != shortPadded {
		t.Fatalf("generics not stripped: %q", got)
	}
}
