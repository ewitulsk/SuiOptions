//! Vault-share coin package codegen.
//!
//! The per-roll CALL/PUT coin packages this module used to generate are
//! RETIRED (SO-393/SO-394): option coins are now runtime currencies
//! registered through `sui::coin_registry` inside
//! `bucket::create_bucket_any_strike` — no publish, no OTW, no caps to
//! harvest. What remains is the vault-share (`vshare::VSHARE`) OTW package,
//! which is still a genuine one-coin-per-vault publish at `create_vault`.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

/// Pinned to match the protocol package's framework revision so the generated
/// package links against the same Sui framework.
const FRAMEWORK_REV: &str = "framework/mainnet";

static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

/// A generated, ready-to-compile coin package on disk.
pub struct GeneratedPackage {
    /// Package root (holds `Move.toml` and `sources/`). Caller compiles this
    /// then should delete it (see [`GeneratedPackage::cleanup`]).
    pub dir: PathBuf,
}

impl GeneratedPackage {
    pub fn cleanup(&self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Generate a single-module share-coin package for a vault's `VShare` type.
///
/// Mirrors [`generate`] but emits one `vshare::VSHARE` One-Time-Witness coin
/// instead of the `call_<i>` grid. The vault takes ownership of the harvested
/// `TreasuryCap<VShare>` at `create_vault`; `decimals` is purely cosmetic
/// (share accounting is a pure ratio at `PPS_SCALE`), fixed at 9 by the
/// caller. `label` is woven into the coin name (e.g. `"TBTC-TUSDC"`) and
/// `symbol` is the wallet ticker (e.g. `"vTBTC"`).
pub fn generate_share(decimals: u8, label: &str, symbol: &str) -> Result<GeneratedPackage> {
    let unique = format!(
        "opt-vsharepkg-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let dir = std::env::temp_dir().join(unique);
    let sources = dir.join("sources");
    fs::create_dir_all(&sources)
        .with_context(|| format!("creating vshare-package dir {}", sources.display()))?;

    fs::write(dir.join("Move.toml"), move_toml())
        .with_context(|| format!("writing Move.toml in {}", dir.display()))?;

    let safe_label = sanitize_label(label);
    let safe_symbol = sanitize_label(symbol);
    let src = share_module_source(decimals, &safe_label, &safe_symbol);
    let path = sources.join("vshare.move");
    fs::write(&path, src).with_context(|| format!("writing module {}", path.display()))?;

    Ok(GeneratedPackage { dir })
}

/// The vault share OTW coin module. Struct name == module name uppercased, as
/// `coin::create_currency`'s OTW check requires.
fn share_module_source(decimals: u8, label: &str, symbol: &str) -> String {
    let name = format!("{label} Vault Share");
    format!(
        r#"#[allow(deprecated_usage, lint(self_transfer))]
module gen_coin::vshare;

public struct VSHARE has drop {{}}

fun init(witness: VSHARE, ctx: &mut TxContext) {{
    let (treasury, metadata) = sui::coin::create_currency(
        witness,
        {decimals},
        b"{symbol}",
        b"{name}",
        b"Covered-call vault share (auto-generated per vault)",
        std::option::none(),
        ctx,
    );
    sui::transfer::public_freeze_object(metadata);
    sui::transfer::public_transfer(treasury, ctx.sender());
}}
"#
    )
}

fn move_toml() -> String {
    format!(
        r#"[package]
name = "gen_coin"
version = "0.0.1"
edition = "2024.beta"

[dependencies]
Sui = {{ git = "https://github.com/MystenLabs/sui.git", subdir = "crates/sui-framework/packages/sui-framework", rev = "{FRAMEWORK_REV}" }}

[addresses]
gen_coin = "0x0"
"#
    )
}

/// Keep only characters safe inside a Move `b"..."` byte-string literal:
/// ASCII alphanumerics plus a few separators. Everything else becomes `-`.
fn sanitize_label(label: &str) -> String {
    let s: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.' | ' ') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Bound the length so coin names stay sane.
    s.chars().take(48).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_unsafe_chars() {
        assert_eq!(sanitize_label("BTC/USDC@123"), "BTC/USDC-123");
        assert_eq!(sanitize_label("a\"b\\c"), "a-b-c");
    }

    #[test]
    fn generate_share_writes_vshare_module() {
        // Hermetic shape check: one `vshare::VSHARE` OTW module with the
        // struct name == module uppercased (the OTW rule). The full compile is
        // exercised by the localnet E2E.
        let pkg = generate_share(9, "TBTC-TUSDC", "vTBTC").unwrap();
        let src = std::fs::read_to_string(pkg.dir.join("sources/vshare.move")).unwrap();
        assert!(src.contains("module gen_coin::vshare;"));
        assert!(src.contains("public struct VSHARE has drop"));
        assert!(src.contains("fun init(witness: VSHARE"));
        assert!(src.contains("b\"vTBTC\""));
        assert!(src.contains('9'));
        assert!(pkg.dir.join("Move.toml").exists());
        pkg.cleanup();
        assert!(!pkg.dir.exists());
    }
}
