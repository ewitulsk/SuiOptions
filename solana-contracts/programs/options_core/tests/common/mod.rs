//! Test helpers — the port of `contracts/tests/test_helpers.move`.
//!
//! Sui's `test_scenario` maps to LiteSVM: `next_tx(sender)` → send a tx
//! signed by that keypair; `clock::increment_for_testing` → set the Clock
//! sysvar; `#[expected_failure(abort_code = …)]` → `assert_core_err`.

#![allow(dead_code)]

use anchor_lang::{
    prelude::{Clock, Pubkey},
    solana_program::instruction::Instruction,
    AccountDeserialize, AnchorSerialize, InstructionData, ToAccountMetas,
};
use litesvm::{types::FailedTransactionMetadata, LiteSVM};
use litesvm_token::{CreateAssociatedTokenAccount, CreateMint, MintTo};
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

use options_core::error::CoreError;
use options_core::state::*;

/// Underlying uses 8 decimals (wBTC-style), settlement 6 (USDC-style) —
/// same shapes the Move tests assume.
pub const UNDERLYING_DECIMALS: u8 = 8;
pub const SETTLEMENT_DECIMALS: u8 = 6;

/// Test genesis: a fixed, known clock so expiry math is deterministic.
pub const GENESIS_SECS: i64 = 1_000_000;
pub const GENESIS_MS: u64 = GENESIS_SECS as u64 * 1000;

pub struct TestCtx {
    pub svm: LiteSVM,
    pub admin: Keypair,
    pub writer: Keypair,
    pub trader: Keypair,
    pub trader_mm: Keypair,
    pub writer_mm: Keypair,
    pub stranger: Keypair,
    pub underlying_mint: Pubkey,
    pub settlement_mint: Pubkey,
    pub config: Pubkey,
    pub treasury: Pubkey,
    pub event_authority: Pubkey,
}

pub struct BucketKeys {
    pub bucket: Pubkey,
    pub call_mint: Pubkey,
    pub underlying_vault: Pubkey,
    pub settlement_vault: Pubkey,
}

pub fn program_id() -> Pubkey {
    options_core::id()
}

pub fn event_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &program_id()).0
}

pub fn config_pda() -> Pubkey {
    Pubkey::find_program_address(&[CONFIG_SEED], &program_id()).0
}

pub fn treasury_pda() -> Pubkey {
    Pubkey::find_program_address(&[TREASURY_SEED], &program_id()).0
}

pub fn bucket_pda(underlying_mint: &Pubkey, settlement_mint: &Pubkey, salt: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[
            BUCKET_SEED,
            underlying_mint.as_ref(),
            settlement_mint.as_ref(),
            &salt.to_le_bytes(),
        ],
        &program_id(),
    )
    .0
}

pub fn call_mint_pda(bucket: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[CALL_MINT_SEED, bucket.as_ref()], &program_id()).0
}

pub fn mm_account_pda(owner: &Pubkey, salt: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[MM_ACCOUNT_SEED, owner.as_ref(), &salt.to_le_bytes()],
        &program_id(),
    )
    .0
}

pub fn nonce_pda(mm_account: &Pubkey, nonce: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[NONCE_SEED, mm_account.as_ref(), &nonce.to_le_bytes()],
        &program_id(),
    )
    .0
}

pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    anchor_spl::associated_token::get_associated_token_address(owner, mint)
}

impl TestCtx {
    /// `init_protocol` from test_helpers.move: load the program, fund the
    /// named actors, create the two mints, initialize config + treasury,
    /// and pin the clock to a known genesis.
    pub fn setup() -> Self {
        let mut svm = LiteSVM::new();
        let bytes = include_bytes!(concat!(
            env!("CARGO_TARGET_TMPDIR"),
            "/../deploy/options_core.so"
        ));
        svm.add_program(program_id(), bytes).unwrap();

        let admin = Keypair::new();
        let writer = Keypair::new();
        let trader = Keypair::new();
        let trader_mm = Keypair::new();
        let writer_mm = Keypair::new();
        let stranger = Keypair::new();
        for kp in [
            &admin, &writer, &trader, &trader_mm, &writer_mm, &stranger,
        ] {
            svm.airdrop(&kp.pubkey(), 100_000_000_000).unwrap();
        }

        let mut clock: Clock = svm.get_sysvar();
        clock.unix_timestamp = GENESIS_SECS;
        svm.set_sysvar(&clock);

        let underlying_mint = CreateMint::new(&mut svm, &admin)
            .decimals(UNDERLYING_DECIMALS)
            .send()
            .unwrap();
        let settlement_mint = CreateMint::new(&mut svm, &admin)
            .decimals(SETTLEMENT_DECIMALS)
            .send()
            .unwrap();

        let mut ctx = TestCtx {
            svm,
            admin,
            writer,
            trader,
            trader_mm,
            writer_mm,
            stranger,
            underlying_mint,
            settlement_mint,
            config: config_pda(),
            treasury: treasury_pda(),
            event_authority: event_authority(),
        };

        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::Initialize {}.data(),
            options_core::accounts::Initialize {
                admin: ctx.admin.pubkey(),
                config: ctx.config,
                treasury: ctx.treasury,
                system_program: anchor_lang::system_program::ID,
                event_authority: ctx.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        let admin = ctx.admin.insecure_clone();
        ctx.send(&admin, &[ix], &[]).unwrap();
        ctx
    }

    /// Send instructions signed by `payer` (+ extra signers). Expires the
    /// blockhash first so identical repeated transactions aren't deduped.
    pub fn send(
        &mut self,
        payer: &Keypair,
        ixs: &[Instruction],
        extra_signers: &[&Keypair],
    ) -> Result<(), FailedTransactionMetadata> {
        self.svm.expire_blockhash();
        let blockhash = self.svm.latest_blockhash();
        let msg = Message::new_with_blockhash(ixs, Some(&payer.pubkey()), &blockhash);
        let mut signers: Vec<&Keypair> = vec![payer];
        signers.extend_from_slice(extra_signers);
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
        self.svm.send_transaction(tx).map(|_| ())
    }

    /// Move the chain clock to an absolute millisecond timestamp.
    pub fn warp_to_ms(&mut self, ms: u64) {
        let mut clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp = (ms / 1000) as i64;
        self.svm.set_sysvar(&clock);
    }

    pub fn now_ms(&self) -> u64 {
        let clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp as u64 * 1000
    }

    pub fn read<T: AccountDeserialize>(&self, key: &Pubkey) -> T {
        let account = self.svm.get_account(key).unwrap();
        T::try_deserialize(&mut account.data.as_slice()).unwrap()
    }

    pub fn account_exists(&self, key: &Pubkey) -> bool {
        self.svm
            .get_account(key)
            .map(|a| !a.data.is_empty() || a.lamports > 0)
            .unwrap_or(false)
    }

    pub fn token_balance(&self, token_account: &Pubkey) -> u64 {
        let acc: anchor_spl::token::TokenAccount = self.read(token_account);
        acc.amount
    }

    /// Create an ATA for `owner` on `mint` and mint `amount` into it
    /// (admin is every test mint's authority).
    pub fn fund_token(&mut self, owner: &Pubkey, mint: &Pubkey, amount: u64) -> Pubkey {
        let admin = self.admin.insecure_clone();
        let ata_addr = CreateAssociatedTokenAccount::new(&mut self.svm, &admin, mint)
            .owner(owner)
            .send()
            .unwrap();
        if amount > 0 {
            MintTo::new(&mut self.svm, &admin, mint, &ata_addr, amount)
                .send()
                .unwrap();
        }
        ata_addr
    }

    /// `new_bucket` from test_helpers.move: admin creates a call bucket.
    pub fn new_bucket(
        &mut self,
        salt: u64,
        expiry_ms: u64,
        strike: u128,
        strike_scale: u8,
    ) -> Result<BucketKeys, FailedTransactionMetadata> {
        let bucket = bucket_pda(&self.underlying_mint, &self.settlement_mint, salt);
        let call_mint = call_mint_pda(&bucket);
        let keys = BucketKeys {
            bucket,
            call_mint,
            underlying_vault: ata(&bucket, &self.underlying_mint),
            settlement_vault: ata(&bucket, &self.settlement_mint),
        };
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::CreateBucket {
                salt,
                expiry_ms,
                strike,
                strike_scale,
            }
            .data(),
            options_core::accounts::CreateBucket {
                admin: self.admin.pubkey(),
                config: self.config,
                underlying_mint: self.underlying_mint,
                settlement_mint: self.settlement_mint,
                bucket,
                call_mint,
                underlying_vault: keys.underlying_vault,
                settlement_vault: keys.settlement_vault,
                token_program: anchor_spl::token::ID,
                associated_token_program: anchor_spl::associated_token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        let admin = self.admin.insecure_clone();
        self.send(&admin, &[ix], &[])?;
        Ok(keys)
    }

    /// Covered write: escrow `amount` underlying from `writer`'s ATA, mint
    /// the position (fresh keypair, like a Sui object id) + call coins to
    /// the writer's call ATA. Returns the position pubkey.
    pub fn write_collateralized(
        &mut self,
        writer: &Keypair,
        keys: &BucketKeys,
        amount: u64,
    ) -> Result<Pubkey, FailedTransactionMetadata> {
        let position = Keypair::new();
        let writer_underlying = ata(&writer.pubkey(), &self.underlying_mint);
        let call_dest = self.ensure_ata(&writer.pubkey(), &keys.call_mint);
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::WriteCollateralized {
                amount,
                position_owner: writer.pubkey(),
            }
            .data(),
            options_core::accounts::WriteCollateralized {
                payer: writer.pubkey(),
                writer: writer.pubkey(),
                bucket: keys.bucket,
                position: position.pubkey(),
                writer_underlying,
                underlying_vault: keys.underlying_vault,
                call_mint: keys.call_mint,
                call_dest,
                token_program: anchor_spl::token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        self.send(&writer.insecure_clone(), &[ix], &[&position])?;
        Ok(position.pubkey())
    }

    pub fn ensure_ata(&mut self, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
        let addr = ata(owner, mint);
        if !self.account_exists(&addr) {
            let admin = self.admin.insecure_clone();
            CreateAssociatedTokenAccount::new(&mut self.svm, &admin, mint)
                .owner(owner)
                .send()
                .unwrap();
        }
        addr
    }

    pub fn exercise(
        &mut self,
        exerciser: &Keypair,
        keys: &BucketKeys,
        amount: u64,
    ) -> Result<(), FailedTransactionMetadata> {
        let exerciser_call = ata(&exerciser.pubkey(), &keys.call_mint);
        let exerciser_settlement = ata(&exerciser.pubkey(), &self.settlement_mint);
        let underlying_mint = self.underlying_mint;
        let exerciser_underlying = self.ensure_ata(&exerciser.pubkey(), &underlying_mint);
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::Exercise { amount }.data(),
            options_core::accounts::Exercise {
                exerciser: exerciser.pubkey(),
                bucket: keys.bucket,
                call_mint: keys.call_mint,
                exerciser_call,
                exerciser_settlement,
                exerciser_underlying,
                underlying_vault: keys.underlying_vault,
                settlement_vault: keys.settlement_vault,
                token_program: anchor_spl::token::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        self.send(&exerciser.insecure_clone(), &[ix], &[])
    }

    pub fn toggle_validity(
        &mut self,
        keys: &BucketKeys,
        invalidate: bool,
        reason: &str,
    ) -> Result<(), FailedTransactionMetadata> {
        let data = if invalidate {
            options_core::instruction::InvalidateBucket {
                reason: reason.to_string(),
            }
            .data()
        } else {
            options_core::instruction::RevalidateBucket {
                reason: reason.to_string(),
            }
            .data()
        };
        let ix = Instruction::new_with_bytes(
            program_id(),
            &data,
            options_core::accounts::ToggleBucketValidity {
                admin: self.admin.pubkey(),
                config: self.config,
                bucket: keys.bucket,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        let admin = self.admin.insecure_clone();
        self.send(&admin, &[ix], &[])
    }

    pub fn transfer_position(
        &mut self,
        owner: &Keypair,
        position: &Pubkey,
        new_owner: &Pubkey,
    ) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::TransferPosition {
                new_owner: *new_owner,
            }
            .data(),
            options_core::accounts::TransferPosition {
                owner: owner.pubkey(),
                position: *position,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        self.send(&owner.insecure_clone(), &[ix], &[])
    }

    pub fn burn_expired(
        &mut self,
        burner: &Keypair,
        keys: &BucketKeys,
        amount: u64,
    ) -> Result<(), FailedTransactionMetadata> {
        let burner_call = ata(&burner.pubkey(), &keys.call_mint);
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::BurnExpiredOption { amount }.data(),
            options_core::accounts::BurnExpiredOption {
                burner: burner.pubkey(),
                bucket: keys.bucket,
                call_mint: keys.call_mint,
                burner_call,
                token_program: anchor_spl::token::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        self.send(&burner.insecure_clone(), &[ix], &[])
    }

    pub fn cleanup_bucket(&mut self, keys: &BucketKeys) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::CleanupBucket {}.data(),
            options_core::accounts::CleanupBucket {
                admin: self.admin.pubkey(),
                config: self.config,
                bucket: keys.bucket,
                call_mint: keys.call_mint,
                underlying_vault: keys.underlying_vault,
                settlement_vault: keys.settlement_vault,
                token_program: anchor_spl::token::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        let admin = self.admin.insecure_clone();
        self.send(&admin, &[ix], &[])
    }

    pub fn redeem(
        &mut self,
        redeemer: &Keypair,
        keys: &BucketKeys,
        position: &Pubkey,
    ) -> Result<(), FailedTransactionMetadata> {
        let (underlying_mint, settlement_mint) = (self.underlying_mint, self.settlement_mint);
        let redeemer_underlying = self.ensure_ata(&redeemer.pubkey(), &underlying_mint);
        let redeemer_settlement = self.ensure_ata(&redeemer.pubkey(), &settlement_mint);
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::RedeemPosition {}.data(),
            options_core::accounts::RedeemPosition {
                redeemer: redeemer.pubkey(),
                bucket: keys.bucket,
                position: *position,
                redeemer_underlying,
                redeemer_settlement,
                underlying_vault: keys.underlying_vault,
                settlement_vault: keys.settlement_vault,
                token_program: anchor_spl::token::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        self.send(&redeemer.insecure_clone(), &[ix], &[])
    }
}

impl TestCtx {
    /// `create_account` from test_helpers.move (Ed25519 scheme).
    pub fn create_mm_account(
        &mut self,
        owner: &Keypair,
        salt: u64,
        scheme: u8,
        pubkey: Vec<u8>,
    ) -> Result<Pubkey, FailedTransactionMetadata> {
        let mm_account = mm_account_pda(&owner.pubkey(), salt);
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::CreateAccount {
                salt,
                signing_scheme: scheme,
                signing_pubkey: pubkey,
            }
            .data(),
            options_core::accounts::CreateAccount {
                owner: owner.pubkey(),
                mm_account,
                system_program: anchor_lang::system_program::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        self.send(&owner.insecure_clone(), &[ix], &[])?;
        Ok(mm_account)
    }

    /// Deposit from the depositor's ATA into the MM account's ATA.
    pub fn mm_deposit(
        &mut self,
        depositor: &Keypair,
        mm_account: &Pubkey,
        mint: &Pubkey,
        amount: u64,
    ) -> Result<(), FailedTransactionMetadata> {
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::AccountDeposit { amount }.data(),
            options_core::accounts::DepositToAccount {
                depositor: depositor.pubkey(),
                mm_account: *mm_account,
                mint: *mint,
                from_token: ata(&depositor.pubkey(), mint),
                account_token: ata(mm_account, mint),
                token_program: anchor_spl::token::ID,
                associated_token_program: anchor_spl::associated_token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        self.send(&depositor.insecure_clone(), &[ix], &[])
    }

    /// Withdraw from the MM account's ATA to the signer's ATA.
    pub fn mm_withdraw(
        &mut self,
        signer: &Keypair,
        mm_account: &Pubkey,
        mint: &Pubkey,
        amount: u64,
    ) -> Result<(), FailedTransactionMetadata> {
        let to_token = self.ensure_ata(&signer.pubkey(), mint);
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::AccountWithdraw { amount }.data(),
            options_core::accounts::WithdrawFromAccount {
                owner: signer.pubkey(),
                mm_account: *mm_account,
                mint: *mint,
                account_token: ata(mm_account, mint),
                to_token,
                token_program: anchor_spl::token::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        self.send(&signer.insecure_clone(), &[ix], &[])
    }
}

// ── quote signing (the ed25519-dalek analog of Sui's test signing) ──

/// Deterministic MM signing key (RFC 8032-style fixed seed).
pub fn mm_signing_key() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[42u8; 32])
}

pub fn signing_pubkey(sk: &ed25519_dalek::SigningKey) -> Vec<u8> {
    sk.verifying_key().to_bytes().to_vec()
}

/// Canonical quote bytes: the Borsh encoding (what the program verifies).
pub fn quote_bytes(quote: &options_core::quote::Quote) -> Vec<u8> {
    let mut v = Vec::new();
    quote.serialize(&mut v).unwrap();
    v
}

/// Build the native Ed25519SigVerify instruction over the quote bytes —
/// prepended to the execute_write transaction (sig_ix_index 0).
pub fn ed25519_verify_ix(
    quote: &options_core::quote::Quote,
    sk: &ed25519_dalek::SigningKey,
) -> Instruction {
    ed25519_verify_ix_over(&quote_bytes(quote), sk)
}

pub fn ed25519_verify_ix_over(msg: &[u8], sk: &ed25519_dalek::SigningKey) -> Instruction {
    use ed25519_dalek::Signer as _;
    let sig = sk.sign(msg).to_bytes();
    let pk = sk.verifying_key().to_bytes();
    solana_ed25519_program::new_ed25519_instruction_with_signature(msg, &sig, &pk)
}

impl TestCtx {
    pub fn set_fee_bps(&mut self, bps: u64) {
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::SetFeeBps { new_bps: bps }.data(),
            options_core::accounts::AdminConfig {
                admin: self.admin.pubkey(),
                config: self.config,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        let admin = self.admin.insecure_clone();
        self.send(&admin, &[ix], &[]).unwrap();
    }

    /// Execute an MM-signed quote. Prepends the Ed25519 precompile ix
    /// (index 0) and sends both with the executor paying. Returns the new
    /// position pubkey.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_write(
        &mut self,
        executor: &Keypair,
        keys: &BucketKeys,
        mm_account: &Pubkey,
        quote: &options_core::quote::Quote,
        flow: options_core::quote::FlowKind,
        position_recipient: Pubkey,
        call_dest_owner: &Pubkey,
        sk: &ed25519_dalek::SigningKey,
    ) -> Result<Pubkey, FailedTransactionMetadata> {
        let sig_ix = ed25519_verify_ix(quote, sk);
        self.execute_write_with_sig_ix(
            executor,
            keys,
            mm_account,
            quote,
            flow,
            position_recipient,
            call_dest_owner,
            Some(sig_ix),
        )
    }

    /// Variant that takes an arbitrary (or no) signature instruction, for
    /// the negative-path signature tests.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_write_with_sig_ix(
        &mut self,
        executor: &Keypair,
        keys: &BucketKeys,
        mm_account: &Pubkey,
        quote: &options_core::quote::Quote,
        flow: options_core::quote::FlowKind,
        position_recipient: Pubkey,
        call_dest_owner: &Pubkey,
        sig_ix: Option<Instruction>,
    ) -> Result<Pubkey, FailedTransactionMetadata> {
        use options_core::quote::FlowKind;
        let position = Keypair::new();
        let call_dest = self.ensure_ata(call_dest_owner, &keys.call_mint);
        let (underlying_mint, settlement_mint) = (self.underlying_mint, self.settlement_mint);
        let executor_settlement = self.ensure_ata(&executor.pubkey(), &settlement_mint);
        let (mm_underlying, executor_underlying) = match flow {
            FlowKind::Writer => (None, Some(ata(&executor.pubkey(), &underlying_mint))),
            FlowKind::Trader => (Some(ata(mm_account, &underlying_mint)), None),
        };
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::ExecuteWrite {
                quote: quote.clone(),
                flow,
                position_recipient,
                sig_ix_index: 0,
            }
            .data(),
            options_core::accounts::ExecuteWrite {
                executor: executor.pubkey(),
                config: self.config,
                treasury: self.treasury,
                bucket: keys.bucket,
                settlement_mint,
                underlying_vault: keys.underlying_vault,
                call_mint: keys.call_mint,
                call_dest,
                mm_account: *mm_account,
                mm_settlement: ata(mm_account, &settlement_mint),
                mm_underlying,
                executor_underlying,
                executor_settlement,
                treasury_settlement: ata(&self.treasury, &settlement_mint),
                position: position.pubkey(),
                nonce_record: nonce_pda(mm_account, quote.nonce),
                instructions_sysvar: solana_sdk_ids::sysvar::instructions::ID,
                token_program: anchor_spl::token::ID,
                associated_token_program: anchor_spl::associated_token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        let ixs: Vec<Instruction> = match sig_ix {
            Some(sig) => vec![sig, ix],
            None => vec![ix],
        };
        self.send(&executor.insecure_clone(), &ixs, &[&position])?;
        Ok(position.pubkey())
    }
}

// ── puts (the analog of test_helpers.move::new_put_bucket etc.) ──

pub struct PutBucketKeys {
    pub bucket: Pubkey,
    pub put_mint: Pubkey,
    pub underlying_vault: Pubkey,
    pub settlement_vault: Pubkey,
}

pub fn put_bucket_pda(underlying_mint: &Pubkey, settlement_mint: &Pubkey, salt: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[
            PUT_BUCKET_SEED,
            underlying_mint.as_ref(),
            settlement_mint.as_ref(),
            &salt.to_le_bytes(),
        ],
        &program_id(),
    )
    .0
}

pub fn put_mint_pda(bucket: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[PUT_MINT_SEED, bucket.as_ref()], &program_id()).0
}

impl TestCtx {
    pub fn new_put_bucket(
        &mut self,
        salt: u64,
        expiry_ms: u64,
        strike: u128,
        strike_scale: u8,
    ) -> Result<PutBucketKeys, FailedTransactionMetadata> {
        let bucket = put_bucket_pda(&self.underlying_mint, &self.settlement_mint, salt);
        let put_mint = put_mint_pda(&bucket);
        let keys = PutBucketKeys {
            bucket,
            put_mint,
            underlying_vault: ata(&bucket, &self.underlying_mint),
            settlement_vault: ata(&bucket, &self.settlement_mint),
        };
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::CreatePutBucket {
                salt,
                expiry_ms,
                strike,
                strike_scale,
            }
            .data(),
            options_core::accounts::CreatePutBucket {
                admin: self.admin.pubkey(),
                config: self.config,
                underlying_mint: self.underlying_mint,
                settlement_mint: self.settlement_mint,
                bucket,
                put_mint,
                underlying_vault: keys.underlying_vault,
                settlement_vault: keys.settlement_vault,
                token_program: anchor_spl::token::ID,
                associated_token_program: anchor_spl::associated_token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        let admin = self.admin.insecure_clone();
        self.send(&admin, &[ix], &[])?;
        Ok(keys)
    }

    pub fn write_put(
        &mut self,
        writer: &Keypair,
        keys: &PutBucketKeys,
        write_amount: u64,
    ) -> Result<Pubkey, FailedTransactionMetadata> {
        let position = Keypair::new();
        let writer_settlement = ata(&writer.pubkey(), &self.settlement_mint);
        let put_dest = self.ensure_ata(&writer.pubkey(), &keys.put_mint);
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::WritePutCollateralized {
                write_amount,
                position_owner: writer.pubkey(),
            }
            .data(),
            options_core::accounts::WritePutCollateralized {
                payer: writer.pubkey(),
                writer: writer.pubkey(),
                bucket: keys.bucket,
                position: position.pubkey(),
                writer_settlement,
                settlement_vault: keys.settlement_vault,
                put_mint: keys.put_mint,
                put_dest,
                token_program: anchor_spl::token::ID,
                system_program: anchor_lang::system_program::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        self.send(&writer.insecure_clone(), &[ix], &[&position])?;
        Ok(position.pubkey())
    }

    pub fn exercise_put(
        &mut self,
        exerciser: &Keypair,
        keys: &PutBucketKeys,
        amount: u64,
    ) -> Result<(), FailedTransactionMetadata> {
        let (underlying_mint, settlement_mint) = (self.underlying_mint, self.settlement_mint);
        let exerciser_put = ata(&exerciser.pubkey(), &keys.put_mint);
        let exerciser_underlying = self.ensure_ata(&exerciser.pubkey(), &underlying_mint);
        let exerciser_settlement = self.ensure_ata(&exerciser.pubkey(), &settlement_mint);
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::ExercisePut { amount }.data(),
            options_core::accounts::ExercisePut {
                exerciser: exerciser.pubkey(),
                bucket: keys.bucket,
                put_mint: keys.put_mint,
                exerciser_put,
                exerciser_underlying,
                exerciser_settlement,
                underlying_vault: keys.underlying_vault,
                settlement_vault: keys.settlement_vault,
                token_program: anchor_spl::token::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        self.send(&exerciser.insecure_clone(), &[ix], &[])
    }

    pub fn redeem_put(
        &mut self,
        redeemer: &Keypair,
        keys: &PutBucketKeys,
        position: &Pubkey,
    ) -> Result<(), FailedTransactionMetadata> {
        let (underlying_mint, settlement_mint) = (self.underlying_mint, self.settlement_mint);
        let redeemer_underlying = self.ensure_ata(&redeemer.pubkey(), &underlying_mint);
        let redeemer_settlement = self.ensure_ata(&redeemer.pubkey(), &settlement_mint);
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::RedeemPutPosition {}.data(),
            options_core::accounts::RedeemPutPosition {
                redeemer: redeemer.pubkey(),
                bucket: keys.bucket,
                position: *position,
                redeemer_underlying,
                redeemer_settlement,
                underlying_vault: keys.underlying_vault,
                settlement_vault: keys.settlement_vault,
                token_program: anchor_spl::token::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        self.send(&redeemer.insecure_clone(), &[ix], &[])
    }

    pub fn burn_expired_put(
        &mut self,
        burner: &Keypair,
        keys: &PutBucketKeys,
        amount: u64,
    ) -> Result<(), FailedTransactionMetadata> {
        let burner_put = ata(&burner.pubkey(), &keys.put_mint);
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::BurnExpiredPut { amount }.data(),
            options_core::accounts::BurnExpiredPut {
                burner: burner.pubkey(),
                bucket: keys.bucket,
                put_mint: keys.put_mint,
                burner_put,
                token_program: anchor_spl::token::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        self.send(&burner.insecure_clone(), &[ix], &[])
    }

    pub fn cleanup_put_bucket(
        &mut self,
        keys: &PutBucketKeys,
    ) -> Result<(), FailedTransactionMetadata> {
        let admin = self.admin.insecure_clone();
        let settlement_mint = self.settlement_mint;
        let admin_settlement = self.ensure_ata(&admin.pubkey(), &settlement_mint);
        let ix = Instruction::new_with_bytes(
            program_id(),
            &options_core::instruction::CleanupPutBucket {}.data(),
            options_core::accounts::CleanupPutBucket {
                admin: admin.pubkey(),
                config: self.config,
                bucket: keys.bucket,
                put_mint: keys.put_mint,
                underlying_vault: keys.underlying_vault,
                settlement_vault: keys.settlement_vault,
                admin_settlement,
                token_program: anchor_spl::token::ID,
                event_authority: self.event_authority,
                program: program_id(),
            }
            .to_account_metas(None),
        );
        self.send(&admin, &[ix], &[])
    }
}

/// Assert a transaction failed with the given core error
/// (the analog of Move's `#[expected_failure(abort_code = …)]`).
pub fn assert_core_err(result: Result<(), FailedTransactionMetadata>, expected: CoreError) {
    use solana_instruction::error::InstructionError;
    use solana_transaction_error::TransactionError;
    let err = result.expect_err("expected transaction to fail");
    let expected_code = 6000 + expected as u32;
    match err.err {
        TransactionError::InstructionError(_, InstructionError::Custom(code)) => {
            assert_eq!(
                code, expected_code,
                "expected custom error {expected_code}, got {code}: {:?}",
                err.meta.logs
            );
        }
        other => panic!("expected custom error {expected_code}, got {other:?}"),
    }
}
