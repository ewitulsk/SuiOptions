//! Vault tests — ports the core of `vault_tests.move` + `oracle_tests.move`
//! running the REAL three-program stack (core + venue + vault) with FORGED
//! Pyth `PriceUpdateV2` accounts. On Sui a `PriceInfoObject` couldn't be
//! forged, forcing spot-injecting test twins; on LiteSVM we write the
//! oracle account bytes directly, so the genuine oracle-gated entrypoints
//! are exercised — feed pinning, staleness and confidence included.

use anchor_lang::{
    prelude::{Clock, Pubkey},
    solana_program::instruction::Instruction,
    AccountDeserialize, InstructionData, ToAccountMetas,
};
use litesvm::{types::FailedTransactionMetadata, LiteSVM};
use litesvm_token::{CreateAssociatedTokenAccount, CreateMint, MintTo};
use options_vault::oracle::{PRICE_UPDATE_V2_DISCRIMINATOR, PYTH_RECEIVER_ID};
use options_vault::state::{Phase, RoundState, Vault, VaultConfig};
use solana_account::Account as SolanaAccount;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

const GENESIS_SECS: i64 = 1_000_000;
const GENESIS_MS: u64 = GENESIS_SECS as u64 * 1000;
const DAY_MS: u64 = 86_400_000;

const U_FEED: [u8; 32] = [1u8; 32];
const S_FEED: [u8; 32] = [2u8; 32];

fn core_id() -> Pubkey {
    options_core::id()
}
fn venue_id() -> Pubkey {
    auction_venue::id()
}
fn vault_prog() -> Pubkey {
    options_vault::id()
}
fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    anchor_spl::associated_token::get_associated_token_address(owner, mint)
}
fn ea(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], program).0
}
fn pda(seeds: &[&[u8]], program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}

fn test_config() -> VaultConfig {
    VaultConfig {
        mgmt_fee_bps_annual: 200,
        perf_fee_bps: 2_000,
        round_ms: 604_800_000, // 7d
        selling_window_ms: DAY_MS,
        min_strike_bps_over_spot: 500,
        max_strike_bps_over_spot: 2_000,
        min_expiry_lead_ms: 3_600_000,
        max_expiry_lead_ms: 14 * DAY_MS,
        min_reserve_premium_bps: 50,
        max_slice_amount: 1_000_000,
        max_open_rfqs: 5,
        rfq_duration_ms: 600_000,
        rfq_snipe_window_ms: 60_000,
        rfq_snipe_extension_ms: 120_000,
        rfq_max_extension_ms: 600_000,
        rfq_min_increment_bps: 500,
        hold_premium_in_settlement: false,
        max_swap_slippage_bps: 100,
        underlying_feed_id: U_FEED,
        settlement_feed_id: S_FEED,
        max_price_age_secs: 3_600,
        max_conf_bps: 500,
        underlying_decimals: 8,
        settlement_decimals: 6,
    }
}

struct Ctx {
    svm: LiteSVM,
    admin: Keypair,
    user: Keypair,
    mm: Keypair,
    underlying: Pubkey,
    settlement: Pubkey,
    core_config: Pubkey,
    core_treasury: Pubkey,
    vault: Pubkey,
    share_mint: Pubkey,
    u_price: Pubkey,
    s_price: Pubkey,
}

struct VaultVaults {
    deployable: Pubkey,
    pending: Pubkey,
    proceeds: Pubkey,
    withdrawal: Pubkey,
    claimable: Pubkey,
    queued: Pubkey,
}

impl Ctx {
    fn vaults(&self) -> VaultVaults {
        VaultVaults {
            deployable: pda(&[b"deployable", self.vault.as_ref()], &vault_prog()),
            pending: pda(&[b"pending", self.vault.as_ref()], &vault_prog()),
            proceeds: pda(&[b"proceeds", self.vault.as_ref()], &vault_prog()),
            withdrawal: pda(&[b"withdrawal", self.vault.as_ref()], &vault_prog()),
            claimable: pda(&[b"claimable", self.vault.as_ref()], &vault_prog()),
            queued: pda(&[b"queued", self.vault.as_ref()], &vault_prog()),
        }
    }

    fn setup() -> Self {
        let mut svm = LiteSVM::new();
        for (id, so) in [
            (core_id(), &include_bytes!(concat!(env!("CARGO_TARGET_TMPDIR"), "/../deploy/options_core.so"))[..]),
            (venue_id(), &include_bytes!(concat!(env!("CARGO_TARGET_TMPDIR"), "/../deploy/auction_venue.so"))[..]),
            (vault_prog(), &include_bytes!(concat!(env!("CARGO_TARGET_TMPDIR"), "/../deploy/options_vault.so"))[..]),
        ] {
            svm.add_program(id, so).unwrap();
        }
        let admin = Keypair::new();
        let user = Keypair::new();
        let mm = Keypair::new();
        for kp in [&admin, &user, &mm] {
            svm.airdrop(&kp.pubkey(), 100_000_000_000).unwrap();
        }
        let mut clock: Clock = svm.get_sysvar();
        clock.unix_timestamp = GENESIS_SECS;
        svm.set_sysvar(&clock);

        let underlying = CreateMint::new(&mut svm, &admin).decimals(8).send().unwrap();
        let settlement = CreateMint::new(&mut svm, &admin).decimals(6).send().unwrap();

        let vault = pda(
            &[b"vault", underlying.as_ref(), settlement.as_ref(), &0u64.to_le_bytes()],
            &vault_prog(),
        );
        let mut ctx = Ctx {
            svm,
            admin,
            user,
            mm,
            underlying,
            settlement,
            core_config: pda(&[b"config"], &core_id()),
            core_treasury: pda(&[b"treasury"], &core_id()),
            vault,
            share_mint: pda(&[b"share_mint", vault.as_ref()], &vault_prog()),
            u_price: Pubkey::new_unique(),
            s_price: Pubkey::new_unique(),
        };

        // Initialize core.
        let admin_kp = ctx.admin.insecure_clone();
        let ix = Instruction::new_with_bytes(
            core_id(),
            &options_core::instruction::Initialize {}.data(),
            options_core::accounts::Initialize {
                admin: admin_kp.pubkey(),
                config: ctx.core_config,
                treasury: ctx.core_treasury,
                system_program: anchor_lang::system_program::ID,
                event_authority: ea(&core_id()),
                program: core_id(),
            }
            .to_account_metas(None),
        );
        ctx.send(&admin_kp, &[ix], &[]).unwrap();
        ctx.refresh_prices();
        ctx
    }

    fn send(
        &mut self,
        payer: &Keypair,
        ixs: &[Instruction],
        extra: &[&Keypair],
    ) -> Result<(), FailedTransactionMetadata> {
        self.svm.expire_blockhash();
        let blockhash = self.svm.latest_blockhash();
        let msg = Message::new_with_blockhash(ixs, Some(&payer.pubkey()), &blockhash);
        let mut signers: Vec<&Keypair> = vec![payer];
        signers.extend_from_slice(extra);
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
        self.svm.send_transaction(tx).map(|_| ())
    }

    fn warp_to_ms(&mut self, ms: u64) {
        let mut clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp = (ms / 1000) as i64;
        self.svm.set_sysvar(&clock);
    }

    fn now_secs(&self) -> i64 {
        let clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp
    }

    /// Forge a Pyth `PriceUpdateV2`: BTC-ish $100k underlying, $1
    /// settlement, published "now" — cross = 1e15 at scale 12 (1000
    /// settlement smallest-units per underlying smallest-unit).
    fn refresh_prices(&mut self) {
        let now = self.now_secs();
        self.write_price(self.u_price, U_FEED, 10_000_000_000_000, 1_000_000, -8, now);
        self.write_price(self.s_price, S_FEED, 100_000_000, 10_000, -8, now);
    }

    fn write_price(
        &mut self,
        key: Pubkey,
        feed: [u8; 32],
        price: i64,
        conf: u64,
        expo: i32,
        publish_time: i64,
    ) {
        let mut data = Vec::with_capacity(134);
        data.extend_from_slice(&PRICE_UPDATE_V2_DISCRIMINATOR);
        data.extend_from_slice(&[0u8; 32]); // write_authority
        data.push(1); // VerificationLevel::Full
        data.extend_from_slice(&feed);
        data.extend_from_slice(&price.to_le_bytes());
        data.extend_from_slice(&conf.to_le_bytes());
        data.extend_from_slice(&expo.to_le_bytes());
        data.extend_from_slice(&publish_time.to_le_bytes());
        data.extend_from_slice(&publish_time.to_le_bytes()); // prev
        data.extend_from_slice(&price.to_le_bytes()); // ema
        data.extend_from_slice(&conf.to_le_bytes()); // ema conf
        data.extend_from_slice(&0u64.to_le_bytes()); // posted_slot
        self.svm
            .set_account(
                key,
                SolanaAccount {
                    lamports: 1_000_000,
                    data,
                    owner: PYTH_RECEIVER_ID,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
    }

    fn read<T: AccountDeserialize>(&self, key: &Pubkey) -> T {
        let account = self.svm.get_account(key).unwrap();
        T::try_deserialize(&mut account.data.as_slice()).unwrap()
    }

    fn balance(&self, token_account: &Pubkey) -> u64 {
        let acc: anchor_spl::token::TokenAccount = self.read(token_account);
        acc.amount
    }

    fn fund(&mut self, owner: &Pubkey, mint: &Pubkey, amount: u64) -> Pubkey {
        let admin = self.admin.insecure_clone();
        let addr = ata(owner, mint);
        if self.svm.get_account(&addr).map(|a| a.lamports == 0).unwrap_or(true) {
            CreateAssociatedTokenAccount::new(&mut self.svm, &admin, mint)
                .owner(owner)
                .send()
                .unwrap();
        }
        if amount > 0 {
            MintTo::new(&mut self.svm, &admin, mint, &addr, amount)
                .send()
                .unwrap();
        }
        addr
    }

    fn create_vault(&mut self) {
        let v = self.vaults();
        let admin = self.admin.insecure_clone();
        let ix = Instruction::new_with_bytes(
            vault_prog(),
            &options_vault::instruction::CreateVault {
                salt: 0,
                config: test_config(),
            }
            .data(),
            options_vault::accounts::CreateVault {
                admin: admin.pubkey(),
                underlying_mint: self.underlying,
                settlement_mint: self.settlement,
                vault: self.vault,
                share_mint: self.share_mint,
                deployable: v.deployable,
                pending: v.pending,
                proceeds: v.proceeds,
                withdrawal_pool: v.withdrawal,
                claimable_shares: v.claimable,
                queued_shares: v.queued,
                token_program: anchor_spl::token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: ea(&vault_prog()),
                program: vault_prog(),
            }
            .to_account_metas(None),
        );
        self.send(&admin, &[ix], &[]).unwrap();
    }

    fn new_call_bucket(&mut self, salt: u64, expiry_ms: u64, strike: u128) -> (Pubkey, Pubkey) {
        let bucket = pda(
            &[b"bucket", self.underlying.as_ref(), self.settlement.as_ref(), &salt.to_le_bytes()],
            &core_id(),
        );
        let call_mint = pda(&[b"call_mint", bucket.as_ref()], &core_id());
        let admin = self.admin.insecure_clone();
        let ix = Instruction::new_with_bytes(
            core_id(),
            &options_core::instruction::CreateBucket {
                salt,
                expiry_ms,
                strike,
                strike_scale: 0,
            }
            .data(),
            options_core::accounts::CreateBucket {
                admin: admin.pubkey(),
                config: self.core_config,
                underlying_mint: self.underlying,
                settlement_mint: self.settlement,
                bucket,
                call_mint,
                underlying_vault: ata(&bucket, &self.underlying),
                settlement_vault: ata(&bucket, &self.settlement),
                token_program: anchor_spl::token::ID,
                associated_token_program: anchor_spl::associated_token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: ea(&core_id()),
                program: core_id(),
            }
            .to_account_metas(None),
        );
        self.send(&admin, &[ix], &[]).unwrap();
        (bucket, call_mint)
    }

    fn deposit(&mut self, amount: u64) -> Pubkey {
        let user = self.user.insecure_clone();
        let receipt = Keypair::new();
        let v = self.vaults();
        let ix = Instruction::new_with_bytes(
            vault_prog(),
            &options_vault::instruction::Deposit { amount }.data(),
            options_vault::accounts::Deposit {
                depositor: user.pubkey(),
                vault: self.vault,
                pending: v.pending,
                depositor_token: ata(&user.pubkey(), &self.underlying),
                receipt: receipt.pubkey(),
                token_program: anchor_spl::token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: ea(&vault_prog()),
                program: vault_prog(),
            }
            .to_account_metas(None),
        );
        self.send(&user, &[ix], &[&receipt]).unwrap();
        receipt.pubkey()
    }

    fn finalize(&mut self) -> Result<(), FailedTransactionMetadata> {
        self.refresh_prices();
        let vault_state: Vault = self.read(&self.vault);
        let round = vault_state.round;
        let v = self.vaults();
        let admin = self.admin.insecure_clone();
        let treasury_token = self.fund(&self.core_treasury.clone(), &self.underlying.clone(), 0);
        let round_state = pda(
            &[b"round", self.vault.as_ref(), &round.to_le_bytes()],
            &vault_prog(),
        );
        let prev = if round > 0 {
            Some(pda(
                &[b"round", self.vault.as_ref(), &(round - 1).to_le_bytes()],
                &vault_prog(),
            ))
        } else {
            None
        };
        let ix = Instruction::new_with_bytes(
            vault_prog(),
            &options_vault::instruction::FinalizeRound {}.data(),
            options_vault::accounts::FinalizeRound {
                cranker: admin.pubkey(),
                vault: self.vault,
                share_mint: self.share_mint,
                deployable: v.deployable,
                pending: v.pending,
                proceeds: v.proceeds,
                withdrawal_pool: v.withdrawal,
                claimable_shares: v.claimable,
                queued_shares: v.queued,
                round_state,
                prev_round_state: prev,
                core_treasury_token: treasury_token,
                underlying_price: self.u_price,
                settlement_price: self.s_price,
                token_program: anchor_spl::token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: ea(&vault_prog()),
                program: vault_prog(),
            }
            .to_account_metas(None),
        );
        self.send(&admin, &[ix], &[])
    }
}

fn assert_vault_err(
    result: Result<(), FailedTransactionMetadata>,
    expected: options_vault::error::VaultError,
) {
    use solana_instruction::error::InstructionError;
    use solana_transaction_error::TransactionError;
    let err = result.expect_err("expected failure");
    let expected_code = 6000 + expected as u32;
    match err.err {
        TransactionError::InstructionError(_, InstructionError::Custom(code)) => {
            assert_eq!(code, expected_code, "logs: {:?}", err.meta.logs)
        }
        other => panic!("expected custom error {expected_code}, got {other:?}"),
    }
}

// ── oracle unit vectors (ports oracle_tests.move cross math) ──

#[test]
fn oracle_cross_math_vectors() {
    use options_vault::oracle::{cross_from_prices, ParsedPrice};
    let p = |price: i64, expo: i32| ParsedPrice {
        feed_id: [0; 32],
        price,
        conf: 0,
        exponent: expo,
        publish_time: 0,
    };
    // BTC $100k (expo −8) / USDC $1 (expo −8), 8→6 decimals:
    // 1000 settlement-units per underlying-unit → 1e15 at scale 12.
    let (cross, scale) = cross_from_prices(&p(10_000_000_000_000, -8), &p(100_000_000, -8), 8, 6).unwrap();
    assert_eq!(scale, 12);
    assert_eq!(cross, 1_000_000_000_000_000);
    // Sub-cent cross: SUI $3.50 vs $1, 9→6 decimals → 3.5e-3 × 1e12.
    let (cross, _) = cross_from_prices(&p(350_000_000, -8), &p(100_000_000, -8), 9, 6).unwrap();
    assert_eq!(cross, 3_500_000_000);
}

// ── the golden path: two full rounds through the three-program stack ──

#[test]
fn vault_full_round_lifecycle() {
    let mut ctx = Ctx::setup();
    ctx.create_vault();
    let v = ctx.vaults();
    let user = ctx.user.insecure_clone();
    let mm = ctx.mm.insecure_clone();
    let admin = ctx.admin.insecure_clone();
    ctx.fund(&user.pubkey(), &ctx.underlying.clone(), 1_000);
    ctx.fund(&mm.pubkey(), &ctx.settlement.clone(), 1_000_000);
    ctx.fund(&mm.pubkey(), &ctx.underlying.clone(), 1_000);

    // Round 0 (genesis): queue a 1000 deposit, finalize immediately —
    // pps[0] is the identity scale, shares minted 1:1.
    let receipt = ctx.deposit(1_000);
    ctx.finalize().unwrap();
    let rs0: RoundState = ctx.read(&pda(
        &[b"round", ctx.vault.as_ref(), &0u64.to_le_bytes()],
        &vault_prog(),
    ));
    assert_eq!(rs0.pps, options_math::PPS_SCALE);
    assert_eq!(ctx.balance(&v.claimable), 1_000);
    assert_eq!(ctx.balance(&v.deployable), 1_000);

    // Claim the shares at pps[0].
    let user_shares = ctx.fund(&user.pubkey(), &ctx.share_mint.clone(), 0);
    let ix = Instruction::new_with_bytes(
        vault_prog(),
        &options_vault::instruction::ClaimShares {}.data(),
        options_vault::accounts::ClaimShares {
            claimer: user.pubkey(),
            vault: ctx.vault,
            receipt,
            round_state: pda(&[b"round", ctx.vault.as_ref(), &0u64.to_le_bytes()], &vault_prog()),
            claimable_shares: v.claimable,
            claimer_shares: user_shares,
            token_program: anchor_spl::token::ID,
            event_authority: ea(&vault_prog()),
            program: vault_prog(),
        }
        .to_account_metas(None),
    );
    ctx.send(&user, &[ix], &[]).unwrap();
    assert_eq!(ctx.balance(&user_shares), 1_000);

    // Queue a 500-share withdrawal for this round (Ribbon semantics: it
    // stays exposed to round-1 P&L).
    let w_receipt = Keypair::new();
    let ix = Instruction::new_with_bytes(
        vault_prog(),
        &options_vault::instruction::InitiateWithdraw { shares: 500 }.data(),
        options_vault::accounts::InitiateWithdraw {
            withdrawer: user.pubkey(),
            vault: ctx.vault,
            queued_shares: v.queued,
            withdrawer_shares: user_shares,
            receipt: w_receipt.pubkey(),
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: ea(&vault_prog()),
            program: vault_prog(),
        }
        .to_account_metas(None),
    );
    ctx.send(&user, &[ix], &[&w_receipt]).unwrap();

    // Round 1: select a bucket 10% over the $ 1000/unit spot.
    let expiry = GENESIS_MS + 2 * DAY_MS;
    let (bucket, call_mint) = ctx.new_call_bucket(0, expiry, 1_100);
    ctx.refresh_prices();
    let ix = Instruction::new_with_bytes(
        vault_prog(),
        &options_vault::instruction::SelectBucket {}.data(),
        options_vault::accounts::SelectBucket {
            cranker: admin.pubkey(),
            vault: ctx.vault,
            bucket,
            underlying_price: ctx.u_price,
            settlement_price: ctx.s_price,
            event_authority: ea(&vault_prog()),
            program: vault_prog(),
        }
        .to_account_metas(None),
    );
    ctx.send(&admin, &[ix], &[]).unwrap();
    let vs: Vault = ctx.read(&ctx.vault.clone());
    assert_eq!(vs.current_bucket, Some(bucket));
    assert_eq!(vs.phase, Phase::Active);

    // Open a 500-unit RFQ slice (auction nonce 0).
    let auction = pda(
        &[b"auction", ctx.vault.as_ref(), &0u64.to_le_bytes()],
        &venue_id(),
    );
    let escrow_vault = pda(&[b"escrow", auction.as_ref()], &venue_id());
    let bid_vault = pda(&[b"bids", auction.as_ref()], &venue_id());
    let ix = Instruction::new_with_bytes(
        vault_prog(),
        &options_vault::instruction::OpenRfq { slice_amount: 500 }.data(),
        options_vault::accounts::OpenRfq {
            cranker: admin.pubkey(),
            vault: ctx.vault,
            bucket,
            underlying_mint: ctx.underlying,
            settlement_mint: ctx.settlement,
            deployable: v.deployable,
            proceeds: v.proceeds,
            underlying_price: ctx.u_price,
            settlement_price: ctx.s_price,
            auction,
            escrow_vault,
            bid_vault,
            venue_event_authority: ea(&venue_id()),
            venue_program: venue_id(),
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: ea(&vault_prog()),
            program: vault_prog(),
        }
        .to_account_metas(None),
    );
    ctx.send(&admin, &[ix], &[]).unwrap();
    assert_eq!(ctx.balance(&escrow_vault), 500);
    // Reserve = 50bps of the 500 × 1000 notional = 2_500.
    let a: auction_venue::state::Auction = ctx.read(&auction);
    assert_eq!(a.reserve_bid, 2_500);

    // MM bids 10_000 premium directly on the venue.
    let ix = Instruction::new_with_bytes(
        venue_id(),
        &auction_venue::instruction::Bid {
            amount: 10_000,
            token_recipient: mm.pubkey(),
        }
        .data(),
        auction_venue::accounts::Bid {
            bidder: mm.pubkey(),
            auction,
            bid_vault,
            bidder_source: ata(&mm.pubkey(), &ctx.settlement),
            previous_bidder_refund: None,
            token_program: anchor_spl::token::ID,
            event_authority: ea(&venue_id()),
            program: venue_id(),
        }
        .to_account_metas(None),
    );
    ctx.send(&mm, &[ix], &[]).unwrap();

    // Settle after the deadline: vault absorbs Position + net premium.
    ctx.warp_to_ms(GENESIS_MS + 600_000);
    let position = Keypair::new();
    let call_dest = ctx.fund(&mm.pubkey(), &call_mint, 0);
    let treasury_settlement = ctx.fund(&ctx.core_treasury.clone(), &ctx.settlement.clone(), 0);
    let vault_pos_0 = pda(
        &[b"vault_pos", ctx.vault.as_ref(), &0u64.to_le_bytes()],
        &vault_prog(),
    );
    let ix = Instruction::new_with_bytes(
        vault_prog(),
        &options_vault::instruction::SettleRfq {}.data(),
        options_vault::accounts::SettleRfq {
            cranker: admin.pubkey(),
            vault: ctx.vault,
            auction,
            escrow_vault,
            bid_vault,
            deployable: v.deployable,
            proceeds: v.proceeds,
            vault_position: vault_pos_0,
            bucket,
            position: position.pubkey(),
            bucket_underlying_vault: ata(&bucket, &ctx.underlying),
            call_mint,
            call_dest,
            core_config: ctx.core_config,
            core_treasury_token: treasury_settlement,
            core_event_authority: ea(&core_id()),
            core_program: core_id(),
            venue_event_authority: ea(&venue_id()),
            venue_program: venue_id(),
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: ea(&vault_prog()),
            program: vault_prog(),
        }
        .to_account_metas(None),
    );
    ctx.send(&admin, &[ix], &[&position]).unwrap();
    assert_eq!(ctx.balance(&v.proceeds), 10_000);
    assert_eq!(ctx.balance(&call_dest), 500);
    let vs: Vault = ctx.read(&ctx.vault.clone());
    assert_eq!(vs.round_premium_collected, 10_000);
    assert_eq!(vs.positions_tail, 1);
    assert_eq!(vs.open_rfqs, 0);

    // Finalize is blocked while the position is unredeemed.
    ctx.warp_to_ms(expiry);
    let result = ctx.finalize();
    assert_vault_err(result, options_vault::error::VaultError::PositionsPending);

    // Crank the FIFO: unexercised position returns 500 underlying.
    let ix = Instruction::new_with_bytes(
        vault_prog(),
        &options_vault::instruction::CrankRedeem {}.data(),
        options_vault::accounts::CrankRedeem {
            cranker: admin.pubkey(),
            vault: ctx.vault,
            bucket,
            vault_position: vault_pos_0,
            position: position.pubkey(),
            deployable: v.deployable,
            proceeds: v.proceeds,
            bucket_underlying_vault: ata(&bucket, &ctx.underlying),
            bucket_settlement_vault: ata(&bucket, &ctx.settlement),
            core_event_authority: ea(&core_id()),
            core_program: core_id(),
            token_program: anchor_spl::token::ID,
            event_authority: ea(&vault_prog()),
            program: vault_prog(),
        }
        .to_account_metas(None),
    );
    ctx.send(&admin, &[ix], &[]).unwrap();
    assert_eq!(ctx.balance(&v.deployable), 1_000);

    // Proceeds must convert before finalize (hold_premium = false).
    let result = ctx.finalize();
    assert_vault_err(result, options_vault::error::VaultError::ProceedsUnswapped);

    // Swap auction: 10_000 settlement for underlying bids (nonce 1).
    ctx.refresh_prices();
    let swap_auction = pda(
        &[b"auction", ctx.vault.as_ref(), &1u64.to_le_bytes()],
        &venue_id(),
    );
    let swap_escrow = pda(&[b"escrow", swap_auction.as_ref()], &venue_id());
    let swap_bids = pda(&[b"bids", swap_auction.as_ref()], &venue_id());
    let ix = Instruction::new_with_bytes(
        vault_prog(),
        &options_vault::instruction::OpenSwapRfq { amount_s: 10_000 }.data(),
        options_vault::accounts::OpenSwapRfq {
            cranker: admin.pubkey(),
            vault: ctx.vault,
            underlying_mint: ctx.underlying,
            settlement_mint: ctx.settlement,
            deployable: v.deployable,
            proceeds: v.proceeds,
            underlying_price: ctx.u_price,
            settlement_price: ctx.s_price,
            auction: swap_auction,
            escrow_vault: swap_escrow,
            bid_vault: swap_bids,
            venue_event_authority: ea(&venue_id()),
            venue_program: venue_id(),
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: ea(&vault_prog()),
            program: vault_prog(),
        }
        .to_account_metas(None),
    );
    ctx.send(&admin, &[ix], &[]).unwrap();
    // Reserve floor: 10_000 settlement ≙ 10 underlying × 99% = 9.
    let a: auction_venue::state::Auction = ctx.read(&swap_auction);
    assert_eq!(a.reserve_bid, 9);

    // MM bids 10 underlying; settle after deadline with a fresh cross.
    let ix = Instruction::new_with_bytes(
        venue_id(),
        &auction_venue::instruction::Bid {
            amount: 10,
            token_recipient: mm.pubkey(),
        }
        .data(),
        auction_venue::accounts::Bid {
            bidder: mm.pubkey(),
            auction: swap_auction,
            bid_vault: swap_bids,
            bidder_source: ata(&mm.pubkey(), &ctx.underlying),
            previous_bidder_refund: None,
            token_program: anchor_spl::token::ID,
            event_authority: ea(&venue_id()),
            program: venue_id(),
        }
        .to_account_metas(None),
    );
    ctx.send(&mm, &[ix], &[]).unwrap();

    ctx.warp_to_ms(expiry + 600_000);
    ctx.refresh_prices();
    let ix = Instruction::new_with_bytes(
        vault_prog(),
        &options_vault::instruction::SettleSwapRfq {}.data(),
        options_vault::accounts::SettleSwapRfq {
            cranker: admin.pubkey(),
            vault: ctx.vault,
            auction: swap_auction,
            escrow_vault: swap_escrow,
            bid_vault: swap_bids,
            deployable: v.deployable,
            proceeds: v.proceeds,
            underlying_price: ctx.u_price,
            settlement_price: ctx.s_price,
            winner_dest: Some(ata(&mm.pubkey(), &ctx.settlement)),
            bidder_refund: Some(ata(&mm.pubkey(), &ctx.underlying)),
            venue_event_authority: ea(&venue_id()),
            venue_program: venue_id(),
            token_program: anchor_spl::token::ID,
            event_authority: ea(&vault_prog()),
            program: vault_prog(),
        }
        .to_account_metas(None),
    );
    ctx.send(&admin, &[ix], &[]).unwrap();
    assert_eq!(ctx.balance(&v.deployable), 1_010);
    assert_eq!(ctx.balance(&v.proceeds), 0);

    // Finalize round 1: aum 1010, profit 10 → mgmt floors to 0, perf =
    // 20% of the 10-underlying realized premium = 2. pps[1] = 1.008.
    ctx.finalize().unwrap();
    let rs1: RoundState = ctx.read(&pda(
        &[b"round", ctx.vault.as_ref(), &1u64.to_le_bytes()],
        &vault_prog(),
    ));
    assert_eq!(rs1.pps, 1_008_000_000_000);
    let treasury_u = ata(&ctx.core_treasury, &ctx.underlying);
    assert_eq!(ctx.balance(&treasury_u), 2);

    // Withdrawal queue paid at pps[1]: 500 shares → 504 underlying.
    assert_eq!(ctx.balance(&v.withdrawal), 504);
    let ix = Instruction::new_with_bytes(
        vault_prog(),
        &options_vault::instruction::CompleteWithdraw {}.data(),
        options_vault::accounts::CompleteWithdraw {
            withdrawer: user.pubkey(),
            vault: ctx.vault,
            receipt: w_receipt.pubkey(),
            round_state: pda(&[b"round", ctx.vault.as_ref(), &1u64.to_le_bytes()], &vault_prog()),
            withdrawal_pool: v.withdrawal,
            withdrawer_token: ata(&user.pubkey(), &ctx.underlying),
            token_program: anchor_spl::token::ID,
            event_authority: ea(&vault_prog()),
            program: vault_prog(),
        }
        .to_account_metas(None),
    );
    ctx.send(&user, &[ix], &[]).unwrap();
    assert_eq!(ctx.balance(&ata(&user.pubkey(), &ctx.underlying)), 504);

    // Share supply: 1000 − 500 burned = 500 outstanding.
    let mint: anchor_spl::token::Mint = ctx.read(&ctx.share_mint.clone());
    assert_eq!(mint.supply, 500);
}

#[test]
fn select_bucket_enforces_strike_band_and_freshness() {
    let mut ctx = Ctx::setup();
    ctx.create_vault();
    let admin = ctx.admin.insecure_clone();
    ctx.finalize().unwrap(); // genesis → round 1 Active

    let expiry = GENESIS_MS + 2 * DAY_MS;
    // Strike only 2% over the 1000 spot — below the 5% band floor.
    let (bucket, _) = ctx.new_call_bucket(0, expiry, 1_020);
    ctx.refresh_prices();
    let select = |ctx: &Ctx, bucket| {
        Instruction::new_with_bytes(
            vault_prog(),
            &options_vault::instruction::SelectBucket {}.data(),
            options_vault::accounts::SelectBucket {
                cranker: admin.pubkey(),
                vault: ctx.vault,
                bucket,
                underlying_price: ctx.u_price,
                settlement_price: ctx.s_price,
                event_authority: ea(&vault_prog()),
                program: vault_prog(),
            }
            .to_account_metas(None),
        )
    };
    let ix = select(&ctx, bucket);
    let result = ctx.send(&admin, &[ix], &[]);
    assert_vault_err(result, options_vault::error::VaultError::StrikeOutOfBand);

    // In-band strike but a stale price (2h old vs the 1h max age).
    let (bucket2, _) = ctx.new_call_bucket(1, expiry, 1_100);
    let stale_time = ctx.now_secs() - 7_200;
    ctx.write_price(ctx.u_price, U_FEED, 10_000_000_000_000, 1_000_000, -8, stale_time);
    let ix = select(&ctx, bucket2);
    let result = ctx.send(&admin, &[ix], &[]);
    assert_vault_err(result, options_vault::error::VaultError::OraclePriceStale);

    // Wrong feed id in the account.
    ctx.write_price(ctx.u_price, [9u8; 32], 10_000_000_000_000, 1_000_000, -8, ctx.now_secs());
    let ix = select(&ctx, bucket2);
    let result = ctx.send(&admin, &[ix], &[]);
    assert_vault_err(result, options_vault::error::VaultError::OracleFeedMismatch);

    // Fresh + correct feed works.
    ctx.refresh_prices();
    let ix = select(&ctx, bucket2);
    ctx.send(&admin, &[ix], &[]).unwrap();
}

#[test]
fn deposit_pause_and_instant_withdraw() {
    let mut ctx = Ctx::setup();
    ctx.create_vault();
    let user = ctx.user.insecure_clone();
    let admin = ctx.admin.insecure_clone();
    ctx.fund(&user.pubkey(), &ctx.underlying.clone(), 1_000);

    let receipt = ctx.deposit(400);

    // Cancel before the round starts: full refund, receipt closed.
    let v = ctx.vaults();
    let ix = Instruction::new_with_bytes(
        vault_prog(),
        &options_vault::instruction::InstantWithdrawPending {}.data(),
        options_vault::accounts::InstantWithdrawPending {
            withdrawer: user.pubkey(),
            vault: ctx.vault,
            receipt,
            pending: v.pending,
            withdrawer_token: ata(&user.pubkey(), &ctx.underlying),
            token_program: anchor_spl::token::ID,
            event_authority: ea(&vault_prog()),
            program: vault_prog(),
        }
        .to_account_metas(None),
    );
    ctx.send(&user, &[ix], &[]).unwrap();
    assert_eq!(ctx.balance(&ata(&user.pubkey(), &ctx.underlying)), 1_000);

    // Pause blocks new deposits.
    let ix = Instruction::new_with_bytes(
        vault_prog(),
        &options_vault::instruction::SetPaused { paused: true }.data(),
        options_vault::accounts::VaultAdmin {
            admin: admin.pubkey(),
            vault: ctx.vault,
            event_authority: ea(&vault_prog()),
            program: vault_prog(),
        }
        .to_account_metas(None),
    );
    ctx.send(&admin, &[ix], &[]).unwrap();
    let receipt2 = Keypair::new();
    let ix = Instruction::new_with_bytes(
        vault_prog(),
        &options_vault::instruction::Deposit { amount: 100 }.data(),
        options_vault::accounts::Deposit {
            depositor: user.pubkey(),
            vault: ctx.vault,
            pending: v.pending,
            depositor_token: ata(&user.pubkey(), &ctx.underlying),
            receipt: receipt2.pubkey(),
            token_program: anchor_spl::token::ID,
            system_program: anchor_lang::system_program::ID,
            event_authority: ea(&vault_prog()),
            program: vault_prog(),
        }
        .to_account_metas(None),
    );
    let result = ctx.send(&user, &[ix], &[&receipt2]);
    assert_vault_err(result, options_vault::error::VaultError::DepositsPaused);
}
