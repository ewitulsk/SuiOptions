//! Per-roll option-coin package codegen.
//!
//! Every bucket set gets its own freshly-published Move package containing one
//! One-Time-Witness coin module per strike (`call_0..call_{count-1}`). Each
//! module's `init` registers a fungible currency via `coin::create_currency`
//! and hands the resulting `TreasuryCap` to the publisher (the scheduler),
//! which then wires it into a bucket via `bucket::create_bucket`.
//!
//! Choosing to publish a package per roll is what lets us drop Sui's
//! pre-deployed "marker" registry entirely: we mint genuine OTW coin types at
//! runtime by compiling and publishing on the fly. The OTW rule (struct name ==
//! MODULE uppercased, created only in `init`) is satisfied by the generated
//! `CALL_<i>` struct in module `call_<i>`.

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

/// Generate a coin package with `count` OTW modules under a fresh temp dir.
///
/// `decimals` is the option coin's display decimals — set to the underlying's
/// decimals so one option-coin smallest-unit maps to one underlying
/// smallest-unit (matching the on-chain `write_amount` semantics). `label` is
/// a human tag woven into the coin name (e.g. `"BTC-USDC@1750000000000"`).
pub fn generate(count: u64, decimals: u8, label: &str) -> Result<GeneratedPackage> {
    assert!(count > 0, "coin package codegen requires count > 0");
    let unique = format!(
        "opt-coinpkg-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let dir = std::env::temp_dir().join(unique);
    let sources = dir.join("sources");
    fs::create_dir_all(&sources)
        .with_context(|| format!("creating coin-package dir {}", sources.display()))?;

    fs::write(dir.join("Move.toml"), move_toml())
        .with_context(|| format!("writing Move.toml in {}", dir.display()))?;

    let safe_label = sanitize_label(label);
    for i in 0..count {
        let src = module_source(i, decimals, &safe_label);
        let path = sources.join(format!("call_{i}.move"));
        fs::write(&path, src)
            .with_context(|| format!("writing module {}", path.display()))?;
    }

    Ok(GeneratedPackage { dir })
}

/// Cash-secured-put twin of [`generate`]: emits `put_<i>/PUT_<i>` OTW coin
/// modules (symbol `oPUT<i>`) instead of the `call_<i>` grid. Same on-disk
/// shape and `decimals`/`label` semantics; paired back to strikes via
/// [`put_index`].
pub fn generate_puts(count: u64, decimals: u8, label: &str) -> Result<GeneratedPackage> {
    assert!(count > 0, "coin package codegen requires count > 0");
    let unique = format!(
        "opt-putpkg-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let dir = std::env::temp_dir().join(unique);
    let sources = dir.join("sources");
    fs::create_dir_all(&sources)
        .with_context(|| format!("creating put-package dir {}", sources.display()))?;

    fs::write(dir.join("Move.toml"), move_toml())
        .with_context(|| format!("writing Move.toml in {}", dir.display()))?;

    let safe_label = sanitize_label(label);
    for i in 0..count {
        let src = put_module_source(i, decimals, &safe_label);
        let path = sources.join(format!("put_{i}.move"));
        fs::write(&path, src)
            .with_context(|| format!("writing module {}", path.display()))?;
    }

    Ok(GeneratedPackage { dir })
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

/// One OTW coin module. The struct name is the module name uppercased, which
/// is exactly what `sui::types::is_one_time_witness` (checked inside
/// `coin::create_currency`) requires.
fn module_source(i: u64, decimals: u8, label: &str) -> String {
    let symbol = format!("oCALL{i}");
    let name = format!("Option Call {i} {label}");
    format!(
        r#"#[allow(deprecated_usage, lint(self_transfer))]
module gen_coin::call_{i};

public struct CALL_{i} has drop {{}}

fun init(witness: CALL_{i}, ctx: &mut TxContext) {{
    let (treasury, metadata) = sui::coin::create_currency(
        witness,
        {decimals},
        b"{symbol}",
        b"{name}",
        b"Tokenized covered-call option (auto-generated per roll)",
        std::option::none(),
        ctx,
    );
    sui::transfer::public_freeze_object(metadata);
    sui::transfer::public_transfer(treasury, ctx.sender());
}}
"#
    )
}

/// One OTW put-coin module, mirroring [`module_source`] with `put_<i>/PUT_<i>`
/// and the `oPUT` symbol.
fn put_module_source(i: u64, decimals: u8, label: &str) -> String {
    let symbol = format!("oPUT{i}");
    let name = format!("Option Put {i} {label}");
    format!(
        r#"#[allow(deprecated_usage, lint(self_transfer))]
module gen_coin::put_{i};

public struct PUT_{i} has drop {{}}

fun init(witness: PUT_{i}, ctx: &mut TxContext) {{
    let (treasury, metadata) = sui::coin::create_currency(
        witness,
        {decimals},
        b"{symbol}",
        b"{name}",
        b"Tokenized cash-secured put option (auto-generated per roll)",
        std::option::none(),
        ctx,
    );
    sui::transfer::public_freeze_object(metadata);
    sui::transfer::public_transfer(treasury, ctx.sender());
}}
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

/// Parse the strike index out of a generated call type string, i.e. the `3`
/// from `0x…::call_3::CALL_3`. Used to pair a harvested `TreasuryCap` back to
/// the strike its module was generated for.
pub fn call_index(call_type: &str) -> Result<u64> {
    let module = call_type
        .split("::")
        .nth(1)
        .with_context(|| format!("call type missing module segment: {call_type}"))?;
    let idx = module
        .strip_prefix("call_")
        .with_context(|| format!("call type module not `call_<i>`: {call_type}"))?;
    idx.parse::<u64>()
        .with_context(|| format!("call type index not a number: {call_type}"))
}

/// Put twin of [`call_index`]: parse the strike index out of a generated put
/// type string, i.e. the `3` from `0x…::put_3::PUT_3`.
pub fn put_index(put_type: &str) -> Result<u64> {
    let module = put_type
        .split("::")
        .nth(1)
        .with_context(|| format!("put type missing module segment: {put_type}"))?;
    let idx = module
        .strip_prefix("put_")
        .with_context(|| format!("put type module not `put_<i>`: {put_type}"))?;
    idx.parse::<u64>()
        .with_context(|| format!("put type index not a number: {put_type}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_index_parses_module_segment() {
        assert_eq!(
            call_index("0x0000000000000000000000000000000000000000000000000000000000000abc::call_0::CALL_0").unwrap(),
            0
        );
        assert_eq!(call_index("0xabc::call_7::CALL_7").unwrap(), 7);
        assert!(call_index("0xabc::bucket::Bucket").is_err());
    }

    #[test]
    fn sanitize_strips_unsafe_chars() {
        assert_eq!(sanitize_label("BTC/USDC@123"), "BTC/USDC-123");
        assert_eq!(sanitize_label("a\"b\\c"), "a-b-c");
    }

    #[test]
    fn generates_count_modules_and_compiles_shape() {
        // Hermetic: assert the files are written with the right names and the
        // OTW struct matches the module. (A full `sui-move-build` compile is
        // exercised by the localnet E2E, which has the framework dep cached.)
        let pkg = generate(3, 8, "BTC-USDC@1").unwrap();
        for i in 0..3 {
            let src = std::fs::read_to_string(pkg.dir.join(format!("sources/call_{i}.move")))
                .unwrap();
            assert!(src.contains(&format!("module gen_coin::call_{i};")));
            assert!(src.contains(&format!("public struct CALL_{i} has drop")));
            assert!(src.contains(&format!("fun init(witness: CALL_{i}")));
        }
        assert!(pkg.dir.join("Move.toml").exists());
        pkg.cleanup();
        assert!(!pkg.dir.exists());
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
