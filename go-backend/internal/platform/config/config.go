// Package config loads the per-env TOML service config.
//
// Mirrors the Rust runtime_config convention: `${VAR}` placeholders in the
// file are expanded from the process environment BEFORE parsing, and a
// missing variable is a hard error (fail fast at boot, never run with an
// empty DB password). Only APP_ENV / LOG_LEVEL / OTEL_EXPORTER_OTLP_ENDPOINT /
// DB_PASSWORD / DB_HOST ever come from the environment; everything else is
// baked into the per-env TOML selected by the Docker ENTRYPOINT.
package config

import (
	"fmt"
	"os"
	"regexp"

	"github.com/BurntSushi/toml"
)

var envVarRe = regexp.MustCompile(`\$\{([A-Za-z_][A-Za-z0-9_]*)\}`)

// ExpandEnv replaces every `${VAR}` in s with os.Getenv("VAR"). An unset or
// empty variable is an error — configs must never silently expand to "".
func ExpandEnv(s string) (string, error) {
	var missing []string
	out := envVarRe.ReplaceAllStringFunc(s, func(match string) string {
		name := match[2 : len(match)-1]
		v, ok := os.LookupEnv(name)
		if !ok || v == "" {
			missing = append(missing, name)
			return match
		}
		return v
	})
	if len(missing) > 0 {
		return "", fmt.Errorf("config: unset environment variable(s): %v", missing)
	}
	return out, nil
}

// LoadTOML reads path, expands ${VAR} placeholders against the environment,
// and decodes the result into out (a pointer to a struct with `toml` tags).
func LoadTOML(path string, out any) error {
	raw, err := os.ReadFile(path)
	if err != nil {
		return fmt.Errorf("config: read %s: %w", path, err)
	}
	expanded, err := ExpandEnv(string(raw))
	if err != nil {
		return fmt.Errorf("config %s: %w", path, err)
	}
	meta, err := toml.Decode(expanded, out)
	if err != nil {
		return fmt.Errorf("config: decode %s: %w", path, err)
	}
	if undecoded := meta.Undecoded(); len(undecoded) > 0 {
		return fmt.Errorf("config %s: unknown keys: %v", path, undecoded)
	}
	return nil
}

// Env returns the APP_ENV selection ("dev"/"staging"/"prod"), defaulting to
// dev for bare local runs.
func Env() string {
	e := os.Getenv("APP_ENV")
	if e == "" {
		return "dev"
	}
	return e
}
