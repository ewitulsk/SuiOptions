// Package store — embedded schema migrations, applied at boot via
// platform/db.
package store

import (
	"embed"
)

//go:embed all:migrations
var MigrationsFS embed.FS

// MigrationsDir is the FS-relative directory passed to goose.
const MigrationsDir = "migrations"
