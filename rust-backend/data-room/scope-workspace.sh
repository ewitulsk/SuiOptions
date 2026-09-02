#!/bin/sh
# Scope a copy of the rust-backend workspace root down to the data-room
# members. Run inside the data-room Docker builders (Dockerfile.collector,
# Dockerfile.batch) after `COPY Cargo.toml Cargo.lock ./` + `COPY data-room/`.
#
# Why: data-room shares the rust-backend workspace (SO-449) so the backtester
# can reuse the lake crates, but a `cargo build -p collector` against the
# full workspace still resolves every member — which clones the Sui git
# dependencies (GBs) even though nothing data-room compiles them. Rewriting
# `members` to the data-room subset keeps the image build to the lake crates
# and registry deps only. The protocol `path = "crates/..."` entries in
# `[workspace.dependencies]` stay: cargo only touches a workspace dependency
# when a member inherits it.
#
# Cargo.lock is the full-workspace lock; cargo prunes the packages the scoped
# members no longer reach and keeps every remaining version pinned, so the
# image builds the same dependency versions CI tested. (That pruning is also
# why the build must NOT pass `--locked`.)
set -eu

cd "$(dirname "$0")/.."

members=$(grep -oE '"data-room/[^"]+"' Cargo.toml | sort -u | paste -sd, -)
[ -n "$members" ] || { echo "scope-workspace: no data-room members in Cargo.toml" >&2; exit 1; }

MEMBERS="$members" perl -0pi -e 's/^members = \[[^\]]*\]/members = [$ENV{MEMBERS}]/m' Cargo.toml
grep -q '^members = \["data-room/' Cargo.toml || { echo "scope-workspace: rewrite failed" >&2; exit 1; }
echo "scope-workspace: members = $members"
