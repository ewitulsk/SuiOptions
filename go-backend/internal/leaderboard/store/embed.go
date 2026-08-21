// Package store — embedded schema migrations, applied at boot via
// platform/db (the diesel embed_migrations! analogue).
package store

import (
	"embed"
)

//go:embed all:migrations
var MigrationsFS embed.FS

// MigrationsDir is the FS-relative directory passed to goose.
const MigrationsDir = "migrations"
