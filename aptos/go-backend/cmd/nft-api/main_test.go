package main

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/ewitulsk/SuiOptions/aptos/go-backend/internal/platform/config"
)

// The deploy workflow renders nft.toml with the full shared shape
// (fullnode_url, database_url, bind_addr, our_venue_address,
// start_version). LoadTOML rejects unknown keys, so the API's file struct
// must accept every key in that shape or the service fatals at boot.
func TestAPIFileConfigAcceptsRenderedShape(t *testing.T) {
	rendered := "fullnode_url = \"https://fullnode.mainnet.aptoslabs.com/v1\"\n" +
		"database_url = \"postgres://nft:secret@postgres:5432/nft?sslmode=disable\"\n" +
		"bind_addr = \"127.0.0.1:8090\"\n" +
		"our_venue_address = \"0x5b388f4c8f4957aae4bf29196bb67c22498c149bb114f7b350f5727fba91df44\"\n" +
		"start_version = 0\n"
	path := filepath.Join(t.TempDir(), "nft.toml")
	if err := os.WriteFile(path, []byte(rendered), 0o600); err != nil {
		t.Fatal(err)
	}
	var got apiFileConfig
	if err := config.LoadTOML(path, &got); err != nil {
		t.Fatalf("rendered nft.toml rejected: %v", err)
	}
	if got.FullnodeURL == "" || got.DatabaseURL == "" {
		t.Fatalf("missing required fields: %+v", got)
	}
}
