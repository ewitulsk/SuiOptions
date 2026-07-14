//! Test-token faucet: create the recipient's ATA (idempotent) and
//! `mint_to` the configured per-request amount. Replaces the Sui on-chain
//! faucet flow — no faucet program exists on the Solana side; the station
//! key is the test mints' mint authority (set by solana-deploy
//! `--faucet-authority`). Non-mainnet only.

use std::collections::HashMap;

use anyhow::{Context, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_tx::Network;
use tracing::warn;

/// One mintable test token, resolved at boot from the solana-token-info
/// snapshot's testTokens block + the `faucet_amounts` config.
#[derive(Debug, Clone)]
pub struct FaucetToken {
    pub mint: Pubkey,
    /// Per-request mint amount in raw units; `None` when the ticker has
    /// no `faucet_amounts` entry (mint refused with 422).
    pub amount: Option<u64>,
    /// Whether the snapshot reports the station key as the mint
    /// authority. Checked at boot: warn if not, 503 on use.
    pub authority_ok: bool,
}

/// The faucet is force-disabled on mainnet-beta regardless of config.
pub fn faucet_allowed(enabled: bool, network: Network) -> bool {
    enabled && network != Network::MainnetBeta
}

/// Build the per-ticker faucet map from the snapshot's testTokens block.
pub fn build_faucet_tokens(
    snapshot: &solana_token_info_client::Snapshot,
    amounts: &std::collections::BTreeMap<String, u64>,
    station: &Pubkey,
) -> Result<HashMap<String, FaucetToken>> {
    let mut out = HashMap::new();
    let Ok(test_tokens) = snapshot.test_tokens() else {
        warn!("faucet enabled but solana-token-info serves no testTokens block");
        return Ok(out);
    };
    let station_b58 = station.to_string();
    for (ticker, tt) in test_tokens {
        let mint: Pubkey = tt
            .mint
            .parse()
            .with_context(|| format!("test token {ticker} mint {} is not base58", tt.mint))?;
        let authority_ok = tt.mint_authority == station_b58;
        if !authority_ok {
            warn!(
                ticker,
                mint_authority = %tt.mint_authority,
                station = %station_b58,
                "station key is NOT this test mint's authority; /faucet will 503 for it"
            );
        }
        let amount = amounts.get(&ticker.to_ascii_uppercase()).copied();
        if amount.is_none() {
            warn!(ticker, "no faucet_amounts entry; /faucet will refuse this ticker");
        }
        out.insert(
            ticker.to_ascii_uppercase(),
            FaucetToken {
                mint,
                amount,
                authority_ok,
            },
        );
    }
    Ok(out)
}

/// The two faucet instructions: idempotent ATA creation (station pays the
/// rent) + `mint_to` with the station as mint authority.
pub fn faucet_ixs(
    station: &Pubkey,
    recipient: &Pubkey,
    mint: &Pubkey,
    amount: u64,
) -> Result<Vec<Instruction>> {
    use anchor_spl::associated_token::spl_associated_token_account;
    use anchor_spl::token::spl_token;

    let ata = anchor_spl::associated_token::get_associated_token_address(recipient, mint);
    let create = spl_associated_token_account::instruction::create_associated_token_account_idempotent(
        station,
        recipient,
        mint,
        &anchor_spl::token::ID,
    );
    let mint_to = spl_token::instruction::mint_to(
        &anchor_spl::token::ID,
        mint,
        &ata,
        station,
        &[],
        amount,
    )
    .context("building spl_token mint_to")?;
    Ok(vec![create, mint_to])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use solana_token_info_client::{ProgramInfo, Snapshot, TestToken};

    #[test]
    fn faucet_force_disabled_on_mainnet_beta() {
        assert!(faucet_allowed(true, Network::Devnet));
        assert!(faucet_allowed(true, Network::Testnet));
        assert!(!faucet_allowed(true, Network::MainnetBeta));
        assert!(!faucet_allowed(false, Network::Devnet));
        assert!(!faucet_allowed(false, Network::MainnetBeta));
    }

    fn snapshot(station: &Pubkey, other_authority: &Pubkey) -> Snapshot {
        let mut test_tokens = BTreeMap::new();
        test_tokens.insert(
            "TBTC".to_string(),
            TestToken {
                mint: Pubkey::new_unique().to_string(),
                decimals: 8,
                mint_authority: station.to_string(),
            },
        );
        test_tokens.insert(
            "TUSDC".to_string(),
            TestToken {
                mint: Pubkey::new_unique().to_string(),
                decimals: 6,
                mint_authority: other_authority.to_string(),
            },
        );
        Snapshot {
            program_info: ProgramInfo {
                options_core_program_id: options_core::ID.to_string(),
                auction_venue_program_id: Pubkey::new_unique().to_string(),
                options_vault_program_id: Pubkey::new_unique().to_string(),
                config_pda: "cfg".into(),
                treasury_pda: "treas".into(),
                admin: "adm".into(),
                network: "devnet".into(),
                deployed_at: String::new(),
                initialize_signature: None,
                test_tokens: Some(test_tokens),
            },
            tokens: vec![],
        }
    }

    #[test]
    fn faucet_map_flags_authority_and_missing_amounts() {
        let station = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let snap = snapshot(&station, &other);
        // TBTC has an amount; TUSDC deliberately omitted.
        let amounts = BTreeMap::from([("TBTC".to_string(), 100_000_000u64)]);

        let map = build_faucet_tokens(&snap, &amounts, &station).unwrap();
        let tbtc = &map["TBTC"];
        assert!(tbtc.authority_ok);
        assert_eq!(tbtc.amount, Some(100_000_000));

        let tusdc = &map["TUSDC"];
        assert!(!tusdc.authority_ok, "station is not TUSDC's mint authority");
        assert_eq!(tusdc.amount, None);
    }

    #[test]
    fn faucet_ixs_are_ata_idempotent_then_mint_to() {
        let station = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let ixs = faucet_ixs(&station, &recipient, &mint, 42).unwrap();
        assert_eq!(ixs.len(), 2);
        assert_eq!(ixs[0].program_id, anchor_spl::associated_token::ID);
        assert_eq!(ixs[0].data, vec![1], "create_idempotent tag");
        assert_eq!(ixs[1].program_id, anchor_spl::token::ID);
        assert_eq!(ixs[1].data[0], 7, "spl-token MintTo tag");
        // mint_to amount rides in the data tail.
        assert_eq!(&ixs[1].data[1..9], &42u64.to_le_bytes());
    }
}
