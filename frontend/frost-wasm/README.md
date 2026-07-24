# frost-wasm

Browser-side FROST 2-of-2 threshold-ed25519 participant (the curator half of
the ceremony; the service half is `rust-backend/services/hedge-signer`). Both
halves pin **`frost-ed25519 = "=3.0.0"`** — the serialized DKG/signing
packages cross the signer's HTTP API, so the versions must match exactly.

## Build artifact is committed

The compiled package in `frontend/src/frost/pkg/` (`frost_wasm.js`,
`frost_wasm_bg.wasm`, `.d.ts`) is **checked into git** so the frontend
builds on Vercel with no Rust toolchain. The wasm is loaded lazily at
ceremony time (`src/frost/frost.ts`), so it costs nothing on normal pages.

## Rebuilding (after any change to `src/lib.rs` or the frost pin)

```sh
# prerequisites: rustup target add wasm32-unknown-unknown; cargo install wasm-pack
cd frontend/frost-wasm
cargo test                      # native-target unit test of the full DKG+sign loop
wasm-pack build --release --target web --out-dir ../src/frost/pkg --no-pack
rm -f ../src/frost/pkg/.gitignore   # wasm-pack drops a `*` .gitignore; the pkg must be committed
```

Commit the regenerated `frontend/src/frost/pkg/` contents together with the
crate change.
