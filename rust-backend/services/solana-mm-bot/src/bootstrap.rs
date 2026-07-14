//! Chain bootstrap + inventory plumbing: the MmAccount PDA, wallet ATAs,
//! and MM-account deposits — the Solana port of the Sui bot's
//! `resolve_account` / faucet-replenish flow.
//!
//! - The MmAccount PDA is `create_account(salt, scheme=0, quote_pubkey)`
//!   under the wallet; the PDA address is deterministic, so "resolve" is a
//!   single account read (no event walking). A PDA that exists with a
//!   DIFFERENT registered quote key is a fatal misconfiguration — retrying
//!   can't fix a wrong key.
//! - Quote-flow funds live in ATAs owned by the MmAccount PDA
//!   (`account_deposit` creates them on first deposit). Auction bids fund
//!   from the wallet's own ATAs (the venue escrows from `bidder_source`).
//! - Non-mainnet replenish: the test mints' authority is the
//!   solana-gas-station faucet key, so the bot tops its wallet up over
//!   HTTP (`POST /faucet`) like any client, then deposits into the
//!   MmAccount. No `faucet_url` configured ⇒ replenish is skipped
//!   (mainnet: ops funds the wallet).

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

use solana_tx::{pda, SolanaClientWrapper};

/// Parse the `[mm_bot] quote_key` secret: a 32-byte ed25519 seed, hex
/// (`0x` prefix optional) or base58.
pub fn parse_quote_seed(raw: &str) -> Result<[u8; 32]> {
    let s = raw.trim();
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    let bytes: Vec<u8> =
        if stripped.len() == 64 && stripped.chars().all(|c| c.is_ascii_hexdigit()) {
            hex::decode(stripped).context("hex-decoding mm_bot.quote_key")?
        } else {
            bs58::decode(s)
                .into_vec()
                .context("base58-decoding mm_bot.quote_key")?
        };
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow!("mm_bot.quote_key must be 32 bytes, got {}", v.len()))
}

/// Ensure the MmAccount PDA for `(wallet, salt)` exists and is registered
/// with exactly our quote key (scheme 0 / ed25519). Creates it when
/// missing; bails on a key/scheme mismatch (fatal misconfiguration).
pub async fn ensure_mm_account(
    wrap: &SolanaClientWrapper,
    salt: u64,
    quote_pubkey: &[u8; 32],
) -> Result<Pubkey> {
    let owner = wrap.signer.pubkey();
    let mm_account = pda::mm_account(&options_core::ID, &owner, salt);
    match wrap
        .get_account_deserialized::<options_core::state::MmAccount>(&mm_account)
        .await
    {
        Ok(existing) => {
            if existing.signing_scheme != options_core::state::SCHEME_ED25519
                || existing.signing_pubkey != quote_pubkey.as_slice()
            {
                bail!(
                    "MmAccount {mm_account} exists but is registered with a different \
                     quote key/scheme — fix mm_bot.quote_key (or use a different \
                     mm_account_salt); re-registering is not supported"
                );
            }
            tracing::info!(%mm_account, salt, "adopted existing MmAccount");
            Ok(mm_account)
        }
        Err(_) => {
            tracing::info!(%mm_account, salt, "no MmAccount for this wallet/salt — creating");
            let ix = solana_tx::ix::create_account(
                &owner,
                salt,
                options_core::state::SCHEME_ED25519,
                quote_pubkey.to_vec(),
            );
            let signature = wrap
                .send_and_confirm(&[ix], &[], "create_account")
                .await
                .inspect_err(|e| {
                    tracing::error!(
                        alert_id = "tx-failed-solana-mm-bot-quote",
                        error = %format!("{e:#}"),
                        "MmAccount create tx failed"
                    );
                })?;
            tracing::info!(%mm_account, %signature, "MmAccount created");
            Ok(mm_account)
        }
    }
}

/// The SPL Associated Token Account program's `CreateIdempotent`
/// instruction, hand-encoded (discriminant byte `1`): no-op when the ATA
/// already exists, creates it (payer-funded) otherwise.
pub fn create_ata_idempotent_ix(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey) -> Instruction {
    Instruction {
        program_id: anchor_spl::associated_token::ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(pda::ata(owner, mint), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(anchor_lang::system_program::ID, false),
            AccountMeta::new_readonly(anchor_spl::token::ID, false),
        ],
        data: vec![1],
    }
}

/// Ensure the wallet's ATA exists for every `mint` (one tx, idempotent).
pub async fn ensure_wallet_atas(wrap: &SolanaClientWrapper, mints: &[Pubkey]) -> Result<()> {
    if mints.is_empty() {
        return Ok(());
    }
    let owner = wrap.signer.pubkey();
    let ixs: Vec<Instruction> = mints
        .iter()
        .map(|m| create_ata_idempotent_ix(&owner, &owner, m))
        .collect();
    wrap.send_and_confirm(&ixs, &[], "ensure wallet ATAs")
        .await
        .inspect_err(|e| {
            tracing::error!(
                alert_id = "tx-failed-solana-mm-bot-quote",
                error = %format!("{e:#}"),
                "ensure-ATA tx failed"
            );
        })?;
    tracing::info!(mints = mints.len(), "wallet ATAs ensured");
    Ok(())
}

/// Token balance of `owner`'s ATA for `mint`; 0 when the ATA doesn't exist.
pub async fn ata_balance(
    wrap: &SolanaClientWrapper,
    owner: &Pubkey,
    mint: &Pubkey,
) -> Result<u64> {
    let ata = pda::ata(owner, mint);
    match wrap.client.get_token_account_balance(&ata).await {
        Ok(b) => b
            .amount
            .parse::<u64>()
            .with_context(|| format!("parsing token balance {:?}", b.amount)),
        Err(_) => Ok(0),
    }
}

// ── faucet (solana-gas-station) ─────────────────────────────────────────

#[derive(Serialize)]
struct FaucetRequest<'a> {
    recipient: String,
    ticker: &'a str,
}

#[derive(Deserialize)]
struct FaucetResponse {
    signature: String,
}

/// `POST /faucet` on solana-gas-station: mints the station-configured
/// per-request amount of `ticker` to the wallet (creating its ATA if
/// missing). Non-mainnet only — the caller gates on `faucet_url` presence.
pub async fn faucet_mint(
    http: &reqwest::Client,
    faucet_url: &str,
    recipient: &Pubkey,
    ticker: &str,
) -> Result<String> {
    let url = format!("{}/faucet", faucet_url.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .json(&FaucetRequest {
            recipient: recipient.to_string(),
            ticker,
        })
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("POST {url} -> {status}: {body}"));
    }
    let out: FaucetResponse = resp
        .json()
        .await
        .with_context(|| format!("decoding {url} response"))?;
    Ok(out.signature)
}

// ── inventory ───────────────────────────────────────────────────────────

/// Parameters for one mint's inventory floor on the MmAccount.
pub struct InventoryParams<'a> {
    pub mm_account: Pubkey,
    pub mint: Pubkey,
    pub symbol: &'a str,
    /// Deposit when the MmAccount's ATA balance falls below this. 0 = skip.
    pub floor: u64,
    /// Amount deposited per top-up.
    pub top_up: u64,
    /// Gas-station base URL for the non-mainnet faucet; `None` ⇒ never
    /// mint, only move what the wallet already holds.
    pub faucet_url: Option<&'a str>,
}

/// Ensure the MmAccount's ATA for `mint` holds at least `floor`: pull the
/// top-up from the wallet's ATA, minting to the wallet via the gas-station
/// faucet first when it's short (and configured). Returns whether a
/// deposit happened.
pub async fn ensure_account_inventory(
    wrap: &SolanaClientWrapper,
    http: &reqwest::Client,
    p: &InventoryParams<'_>,
) -> Result<bool> {
    if p.floor == 0 {
        return Ok(false);
    }
    let balance = ata_balance(wrap, &p.mm_account, &p.mint).await?;
    if balance >= p.floor {
        tracing::trace!(symbol = %p.symbol, balance, floor = p.floor, "inventory ok");
        return Ok(false);
    }
    let wallet = wrap.signer.pubkey();
    let mut wallet_balance = ata_balance(wrap, &wallet, &p.mint).await?;
    if wallet_balance < p.top_up {
        match p.faucet_url {
            Some(url) => {
                // The station mints its configured per-request amount; loop
                // a few times if one request doesn't cover the top-up.
                for _ in 0..5 {
                    let signature = faucet_mint(http, url, &wallet, p.symbol)
                        .await
                        .with_context(|| format!("faucet mint of {}", p.symbol))?;
                    tracing::info!(symbol = %p.symbol, %signature, "faucet minted to wallet");
                    wallet_balance = ata_balance(wrap, &wallet, &p.mint).await?;
                    if wallet_balance >= p.top_up {
                        break;
                    }
                }
            }
            None => {
                tracing::warn!(
                    symbol = %p.symbol,
                    wallet_balance,
                    top_up = p.top_up,
                    "wallet short of the top-up and no faucet configured; depositing what's there"
                );
            }
        }
    }
    let deposit = p.top_up.min(wallet_balance);
    if deposit == 0 {
        tracing::warn!(symbol = %p.symbol, "nothing to deposit; inventory stays under floor");
        return Ok(false);
    }
    let ix = solana_tx::ix::account_deposit(
        &wallet,
        &p.mm_account,
        &p.mint,
        &pda::ata(&wallet, &p.mint),
        deposit,
    );
    let signature = wrap
        .send_and_confirm(&[ix], &[], "account_deposit")
        .await
        .inspect_err(|e| {
            tracing::error!(
                alert_id = "tx-failed-solana-mm-bot-quote",
                symbol = %p.symbol,
                error = %format!("{e:#}"),
                "account_deposit tx failed"
            );
        })?;
    tracing::info!(symbol = %p.symbol, deposit, %signature, "inventory deposited into MmAccount");
    Ok(true)
}

/// Owned form of [`InventoryParams`] for the background replenish task.
pub struct ReplenishParams {
    pub secrets: runtime_config::Secrets,
    pub network: solana_tx::Network,
    pub mm_account: Pubkey,
    pub mint: Pubkey,
    pub symbol: String,
    pub floor: u64,
    pub top_up: u64,
    pub faucet_url: Option<String>,
    pub interval_secs: u64,
}

/// Periodically re-check one mint's inventory floor and top it up. Runs in
/// its own task with its own RPC client so it doesn't contend with the WS
/// serve loop. Transient errors are logged and retried on the next tick.
pub fn spawn_replenish_task(p: ReplenishParams) {
    tokio::spawn(async move {
        let wrap = match SolanaClientWrapper::connect(&p.secrets, p.network) {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "replenish: failed to connect; task exiting");
                return;
            }
        };
        let http = reqwest::Client::new();
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_secs(p.interval_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let params = InventoryParams {
                mm_account: p.mm_account,
                mint: p.mint,
                symbol: &p.symbol,
                floor: p.floor,
                top_up: p.top_up,
                faucet_url: p.faucet_url.as_deref(),
            };
            if let Err(e) = ensure_account_inventory(&wrap, &http, &params).await {
                tracing::warn!(
                    symbol = %p.symbol,
                    error = %format!("{e:#}"),
                    "replenish tick failed; retrying next tick"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_seed_parses_hex_and_base58_to_same_bytes() {
        let seed = [7u8; 32];
        let hex_plain = hex::encode(seed);
        let hex_prefixed = format!("0x{hex_plain}");
        let b58 = bs58::encode(seed).into_string();
        assert_eq!(parse_quote_seed(&hex_plain).unwrap(), seed);
        assert_eq!(parse_quote_seed(&hex_prefixed).unwrap(), seed);
        assert_eq!(parse_quote_seed(&b58).unwrap(), seed);
    }

    #[test]
    fn quote_seed_rejects_bad_lengths_and_garbage() {
        assert!(parse_quote_seed(&hex::encode([1u8; 31])).is_err());
        assert!(parse_quote_seed(&bs58::encode([1u8; 33]).into_string()).is_err());
        assert!(parse_quote_seed("not-a-key-0OIl").is_err());
    }

    #[test]
    fn ata_idempotent_ix_shape() {
        let payer = Pubkey::new_from_array([1; 32]);
        let owner = Pubkey::new_from_array([2; 32]);
        let mint = Pubkey::new_from_array([3; 32]);
        let ix = create_ata_idempotent_ix(&payer, &owner, &mint);
        assert_eq!(ix.program_id, anchor_spl::associated_token::ID);
        assert_eq!(ix.data, vec![1]); // CreateIdempotent discriminant
        assert_eq!(ix.accounts.len(), 6);
        assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable);
        assert_eq!(ix.accounts[1].pubkey, pda::ata(&owner, &mint));
    }
}
