//! Transaction template matching — the port of sui-tx's `tx::template`
//! (the security core, see the validation section of
//! docs/solana/backend/08-solana-gas-station.md).
//!
//! The frontend produces a small, closed set of transaction shapes. Each
//! [`TxTemplate`] pins, per named flow: `required` — an ordered
//! subsequence of `(program_id, anchor_discriminator)` pairs — and
//! `allowed` — a closed set of `(program_id, IxKind)` every non-benign
//! instruction must belong to. Anything matching no template is refused.
//!
//! What we let vary (the analog of Sui's SplitCoins/MergeCoins plumbing):
//! ComputeBudget instructions, spl-associated-token-account
//! `create`/`create_idempotent`, and memo — value-neutral prelude the SDKs
//! inject. The **Ed25519SigVerify precompile is NOT globally benign**: it
//! is an `allowed` entry on the quote-flow templates only, so it cannot
//! ride along on flows that don't need it.
//!
//! Discriminators come from the program crates' generated instruction
//! types (`anchor_lang::Discriminator`), never hardcoded bytes — zero
//! drift with the deployed programs.

use std::collections::HashSet;
use std::fmt::Write as _;

use anchor_lang::Discriminator;
use solana_sdk::pubkey::Pubkey;

pub use options_core::quote::ED25519_PROGRAM_ID;

/// spl-memo program ids (legacy v1 + current).
const MEMO_V1: Pubkey = Pubkey::from_str_const("Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo");
const MEMO: Pubkey = Pubkey::from_str_const("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

/// One instruction as compiled in a message: resolved program id + raw
/// instruction data. Account wiring is left to the fee-payer guards in
/// `sponsor.rs` plus the programs' own constraints — the station's job is
/// only to refuse paying for instructions it does not endorse.
#[derive(Debug, Clone)]
pub struct IxView {
    pub program: Pubkey,
    pub data: Vec<u8>,
}

/// How an `allowed` entry matches an instruction of its program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IxKind {
    /// Any instruction of the program (used for the Ed25519SigVerify
    /// precompile, whose data is a signature payload, not a dispatch tag).
    Program,
    /// An Anchor instruction with this 8-byte discriminator.
    Anchor([u8; 8]),
    /// A classic spl-token instruction tag (the first data byte —
    /// 3 = Transfer, 12 = TransferChecked, 4 = Approve, …). No flow
    /// currently needs raw spl-token instructions (the programs move
    /// tokens via CPI under the user's own signature), so no template
    /// lists any; the kind exists so a future flow can allow exactly the
    /// tags it needs instead of the whole token program.
    SplToken(u8),
}

/// One sponsored transaction shape.
pub struct TxTemplate {
    pub name: String,
    /// `(program, discriminator)` pairs that must appear, in this order,
    /// as a subsequence of the non-benign instructions.
    pub required: Vec<(Pubkey, [u8; 8])>,
    /// Every non-benign instruction must match one of these.
    pub allowed: HashSet<(Pubkey, IxKind)>,
}

/// The 8-byte Anchor discriminator of a generated instruction type.
pub fn disc<T: Discriminator>() -> [u8; 8] {
    T::DISCRIMINATOR
        .try_into()
        .expect("anchor instruction discriminators are 8 bytes")
}

/// Value-neutral plumbing skipped by every template: ComputeBudget (any
/// instruction), ATA `create` (empty data or tag 0) / `create_idempotent`
/// (tag 1) — NOT `recover_nested` (tag 2) — and memo.
fn is_benign(ix: &IxView) -> bool {
    if ix.program == solana_sdk_ids::compute_budget::ID {
        return true;
    }
    if ix.program == anchor_spl::associated_token::ID {
        return matches!(ix.data.first(), None | Some(0) | Some(1));
    }
    ix.program == MEMO || ix.program == MEMO_V1
}

impl TxTemplate {
    /// Does this instruction sequence match the template?
    pub fn matches(&self, ixs: &[IxView]) -> bool {
        let mut req = self.required.iter();
        let mut want = req.next();
        for ix in ixs {
            if is_benign(ix) {
                continue;
            }
            // (a) closed allowed set.
            let allowed = self.allowed.contains(&(ix.program, IxKind::Program))
                || ix
                    .data
                    .first()
                    .is_some_and(|tag| self.allowed.contains(&(ix.program, IxKind::SplToken(*tag))))
                || (ix.data.len() >= 8
                    && self.allowed.contains(&(
                        ix.program,
                        IxKind::Anchor(ix.data[..8].try_into().expect("checked length")),
                    )));
            if !allowed {
                return false;
            }
            // (b) required pairs appear, in order, as a subsequence.
            if let Some((prog, d)) = want {
                if ix.program == *prog && ix.data.len() >= 8 && &ix.data[..8] == d {
                    want = req.next();
                }
            }
        }
        want.is_none()
    }
}

/// Returns the name of the first template `ixs` matches, if any.
pub fn match_any<'a>(templates: &'a [TxTemplate], ixs: &[IxView]) -> Option<&'a str> {
    templates
        .iter()
        .find(|t| t.matches(ixs))
        .map(|t| t.name.as_str())
}

/// Compact, log-safe summary of an instruction sequence — the port of
/// sui-tx's `describe_ptb`. The matcher is otherwise opaque on refusal;
/// this turns a bare "matches no template" into something a frontend dev
/// can diff against their builders without a redeploy.
pub fn describe_instructions(ixs: &[IxView]) -> String {
    ixs.iter().map(describe_ix).collect::<Vec<_>>().join("; ")
}

fn describe_ix(ix: &IxView) -> String {
    let p = ix.program;
    if p == solana_sdk_ids::compute_budget::ID {
        return format!("ComputeBudget(tag={:?})", ix.data.first());
    }
    if p == anchor_spl::associated_token::ID {
        return format!("AssociatedTokenAccount(tag={:?})", ix.data.first());
    }
    if p == anchor_spl::token::ID {
        return format!("SplToken(tag={:?})", ix.data.first());
    }
    if p == ED25519_PROGRAM_ID {
        return "Ed25519SigVerify".to_string();
    }
    if p == MEMO || p == MEMO_V1 {
        return "Memo".to_string();
    }
    if p == solana_sdk_ids::system_program::ID {
        return format!("System(tag={:?})", ix.data.first());
    }
    let mut s = format!("{p}#");
    for b in ix.data.iter().take(8) {
        let _ = write!(s, "{b:02x}");
    }
    let _ = write!(s, "({}B)", ix.data.len());
    s
}

/// Build the sponsored-transaction templates for the protocol frontend.
///
/// `core` / `venue` / `vault` are the deployed program ids from the
/// solana-token-info snapshot. Mirrors the frontend flows enumerated in
/// docs/solana/backend/08-solana-gas-station.md; any new frontend flow
/// needs a matching template here or the station refuses to sponsor it.
pub fn protocol_templates(core: Pubkey, venue: Pubkey, vault: Pubkey) -> Vec<TxTemplate> {
    use auction_venue::instruction as venue_ix;
    use options_core::instruction as core_ix;
    use options_vault::instruction as vault_ix;

    // Single-anchor wallet flow: one required instruction, nothing else
    // allowed (beyond the global benign skips).
    let single = |name: &str, program: Pubkey, d: [u8; 8]| TxTemplate {
        name: name.to_owned(),
        required: vec![(program, d)],
        allowed: HashSet::from([(program, IxKind::Anchor(d))]),
    };

    // Quote flow: execute_write (or its put twin) plus the Ed25519SigVerify
    // precompile carrying the MM's quote signature. The write and buy
    // frontend flows compile to the same instruction (the `FlowKind` arg
    // distinguishes them), so one template covers both names.
    let quote_flow = |name: &str, d: [u8; 8]| TxTemplate {
        name: name.to_owned(),
        required: vec![(core, d)],
        allowed: HashSet::from([
            (core, IxKind::Anchor(d)),
            (ED25519_PROGRAM_ID, IxKind::Program),
        ]),
    };

    let mut templates = vec![
        quote_flow("write/buy", disc::<core_ix::ExecuteWrite>()),
        single("exercise", core, disc::<core_ix::Exercise>()),
        single("redeem", core, disc::<core_ix::RedeemPosition>()),
        // Cash-secured put twins.
        quote_flow("put_write/put_buy", disc::<core_ix::ExecutePutWrite>()),
        single("put_exercise", core, disc::<core_ix::ExercisePut>()),
        single("put_redeem", core, disc::<core_ix::RedeemPutPosition>()),
        // Venue: escrowed ascending bid.
        single("venue:bid", venue, disc::<venue_ix::Bid>()),
    ];

    // Wallet-facing covered-call vault flows: each a single instruction.
    // Every asset moved is the user's own (their tokens in,
    // receipts/shares/refunds back), so the sponsor only risks the
    // fee/rent delta bounded by the lamport cap.
    templates.push(single("vault:deposit", vault, disc::<vault_ix::Deposit>()));
    templates.push(single(
        "vault:claim_shares",
        vault,
        disc::<vault_ix::ClaimShares>(),
    ));
    templates.push(single(
        "vault:initiate_withdraw",
        vault,
        disc::<vault_ix::InitiateWithdraw>(),
    ));
    templates.push(single(
        "vault:complete_withdraw",
        vault,
        disc::<vault_ix::CompleteWithdraw>(),
    ));
    templates.push(single(
        "vault:instant_withdraw_pending",
        vault,
        disc::<vault_ix::InstantWithdrawPending>(),
    ));

    templates
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::InstructionData;
    use solana_sdk::instruction::{AccountMeta, Instruction};
    use solana_tx::quote::{
        ed25519_verify_ix, quote_bytes, quote_pubkey, sign_quote, FlowKind, Quote,
    };

    fn templates() -> Vec<TxTemplate> {
        protocol_templates(options_core::ID, auction_venue::ID, options_vault::ID)
    }

    fn view(ix: &Instruction) -> IxView {
        IxView {
            program: ix.program_id,
            data: ix.data.clone(),
        }
    }

    fn views(ixs: &[Instruction]) -> Vec<IxView> {
        ixs.iter().map(view).collect()
    }

    fn metas(n: usize) -> Vec<AccountMeta> {
        (0..n)
            .map(|_| AccountMeta::new(Pubkey::new_unique(), false))
            .collect()
    }

    fn sample_quote() -> Quote {
        Quote {
            protocol_id: Pubkey::new_unique(),
            signer_account: Pubkey::new_unique(),
            signer_token_recipient: Pubkey::new_unique(),
            bucket: Pubkey::new_unique(),
            write_amount: 10_000_000,
            premium: 50_000_000,
            valid_until_ms: 1_800_000_000_000,
            nonce: 7,
        }
    }

    /// The real quote-verification precompile instruction, built exactly
    /// as the frontend does.
    fn ed25519_ix() -> Instruction {
        let quote = sample_quote();
        let seed = [9u8; 32];
        let sig = sign_quote(&seed, &quote).unwrap();
        let pk = quote_pubkey(&seed).unwrap();
        ed25519_verify_ix(&pk, &quote_bytes(&quote), &sig)
    }

    /// A real `execute_write` built with the solana-tx builder over the
    /// program crate's encoder.
    fn execute_write_ix() -> Instruction {
        let accounts = solana_tx::ix::ExecuteWrite {
            executor: Pubkey::new_unique(),
            bucket: Pubkey::new_unique(),
            underlying_mint: Pubkey::new_unique(),
            settlement_mint: Pubkey::new_unique(),
            call_dest: Pubkey::new_unique(),
            mm_account: Pubkey::new_unique(),
            mm_underlying: Some(Pubkey::new_unique()),
            executor_underlying: None,
            executor_settlement: Pubkey::new_unique(),
            position: Pubkey::new_unique(),
        };
        solana_tx::ix::execute_write(
            &accounts,
            sample_quote(),
            FlowKind::Trader,
            Pubkey::new_unique(),
            0,
        )
    }

    fn compute_budget_ix() -> Instruction {
        // SetComputeUnitLimit — any ComputeBudget ix is benign.
        Instruction::new_with_bytes(
            solana_sdk_ids::compute_budget::ID,
            &[2, 64, 66, 15, 0],
            vec![],
        )
    }

    fn ata_ix(tag: u8) -> Instruction {
        Instruction::new_with_bytes(anchor_spl::associated_token::ID, &[tag], metas(6))
    }

    fn system_transfer_ix() -> Instruction {
        // System transfer: enum tag 2 (u32 LE) + lamports (u64 LE).
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend_from_slice(&1_000_000_000u64.to_le_bytes());
        Instruction::new_with_bytes(solana_sdk_ids::system_program::ID, &data, metas(2))
    }

    fn spl_transfer_ix() -> Instruction {
        // spl-token Transfer: tag 3 + amount.
        let mut data = vec![3u8];
        data.extend_from_slice(&5u64.to_le_bytes());
        Instruction::new_with_bytes(anchor_spl::token::ID, &data, metas(3))
    }

    #[test]
    fn write_flow_matches_with_ed25519_and_compute_budget() {
        let ixs = views(&[compute_budget_ix(), ed25519_ix(), execute_write_ix()]);
        assert_eq!(match_any(&templates(), &ixs), Some("write/buy"));
    }

    #[test]
    fn exercise_and_redeem_match() {
        let ex = solana_tx::ix::exercise(
            &solana_tx::ix::Exercise {
                exerciser: Pubkey::new_unique(),
                bucket: Pubkey::new_unique(),
                underlying_mint: Pubkey::new_unique(),
                settlement_mint: Pubkey::new_unique(),
                exerciser_call: Pubkey::new_unique(),
                exerciser_settlement: Pubkey::new_unique(),
                exerciser_underlying: Pubkey::new_unique(),
            },
            5,
        );
        assert_eq!(match_any(&templates(), &views(&[ex])), Some("exercise"));

        let rd = solana_tx::ix::redeem_position(&solana_tx::ix::RedeemPosition {
            redeemer: Pubkey::new_unique(),
            bucket: Pubkey::new_unique(),
            underlying_mint: Pubkey::new_unique(),
            settlement_mint: Pubkey::new_unique(),
            position: Pubkey::new_unique(),
            redeemer_underlying: Pubkey::new_unique(),
            redeemer_settlement: Pubkey::new_unique(),
        });
        assert_eq!(match_any(&templates(), &views(&[rd])), Some("redeem"));
    }

    #[test]
    fn put_twins_match() {
        use options_core::instruction as core_ix;
        let put_write = Instruction::new_with_bytes(
            options_core::ID,
            &core_ix::ExecutePutWrite {
                quote: sample_quote(),
                flow: FlowKind::Writer,
                position_recipient: Pubkey::new_unique(),
                sig_ix_index: 0,
            }
            .data(),
            metas(8),
        );
        assert_eq!(
            match_any(&templates(), &views(&[ed25519_ix(), put_write])),
            Some("put_write/put_buy"),
        );

        let put_ex = Instruction::new_with_bytes(
            options_core::ID,
            &core_ix::ExercisePut { amount: 5 }.data(),
            metas(8),
        );
        assert_eq!(match_any(&templates(), &views(&[put_ex])), Some("put_exercise"));

        let put_rd = Instruction::new_with_bytes(
            options_core::ID,
            &core_ix::RedeemPutPosition {}.data(),
            metas(8),
        );
        assert_eq!(match_any(&templates(), &views(&[put_rd])), Some("put_redeem"));
    }

    #[test]
    fn vault_flows_match_and_ata_prelude_is_benign() {
        use options_vault::instruction as vault_ix;
        let v = |data: Vec<u8>| Instruction::new_with_bytes(options_vault::ID, &data, metas(6));
        for (name, data) in [
            ("vault:deposit", vault_ix::Deposit { amount: 5 }.data()),
            ("vault:claim_shares", vault_ix::ClaimShares {}.data()),
            (
                "vault:initiate_withdraw",
                vault_ix::InitiateWithdraw { shares: 5 }.data(),
            ),
            ("vault:complete_withdraw", vault_ix::CompleteWithdraw {}.data()),
            (
                "vault:instant_withdraw_pending",
                vault_ix::InstantWithdrawPending {}.data(),
            ),
        ] {
            // ATA create (0) / create_idempotent (1) preludes are benign.
            let ixs = views(&[ata_ix(1), ata_ix(0), v(data)]);
            assert_eq!(match_any(&templates(), &ixs), Some(name));
        }
    }

    #[test]
    fn venue_bid_matches() {
        let bid = solana_tx::ix::bid(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            Some(Pubkey::new_unique()),
            100,
            Pubkey::new_unique(),
        );
        assert_eq!(match_any(&templates(), &views(&[bid])), Some("venue:bid"));
    }

    #[test]
    fn system_transfer_smuggled_into_write_rejected() {
        let ixs = views(&[ed25519_ix(), execute_write_ix(), system_transfer_ix()]);
        assert_eq!(match_any(&templates(), &ixs), None);
    }

    #[test]
    fn ed25519_is_not_globally_benign() {
        // The precompile is allowed only where a quote-flow template lists
        // it — an exercise carrying one is refused.
        let ex = Instruction::new_with_bytes(
            options_core::ID,
            &options_core::instruction::Exercise { amount: 1 }.data(),
            metas(8),
        );
        assert_eq!(match_any(&templates(), &views(&[ed25519_ix(), ex])), None);
    }

    #[test]
    fn spl_token_transfer_smuggled_rejected() {
        use options_vault::instruction as vault_ix;
        let dep = Instruction::new_with_bytes(
            options_vault::ID,
            &vault_ix::Deposit { amount: 5 }.data(),
            metas(6),
        );
        assert_eq!(
            match_any(&templates(), &views(&[dep, spl_transfer_ix()])),
            None
        );
    }

    #[test]
    fn spl_token_kind_gates_on_the_exact_tag() {
        // A hypothetical template allowing only spl-token Approve (tag 4)
        // accepts approve and refuses transfer (tag 3).
        let t = TxTemplate {
            name: "approve_only".into(),
            required: vec![],
            allowed: HashSet::from([(anchor_spl::token::ID, IxKind::SplToken(4))]),
        };
        let mut approve = vec![4u8];
        approve.extend_from_slice(&5u64.to_le_bytes());
        let approve = Instruction::new_with_bytes(anchor_spl::token::ID, &approve, metas(3));
        assert!(t.matches(&views(&[approve])));
        assert!(!t.matches(&views(&[spl_transfer_ix()])));
    }

    #[test]
    fn ata_recover_nested_is_not_benign() {
        use options_vault::instruction as vault_ix;
        let dep = Instruction::new_with_bytes(
            options_vault::ID,
            &vault_ix::Deposit { amount: 5 }.data(),
            metas(6),
        );
        assert_eq!(match_any(&templates(), &views(&[ata_ix(2), dep])), None);
    }

    #[test]
    fn admin_and_foreign_calls_rejected() {
        let admin = Instruction::new_with_bytes(
            options_core::ID,
            &options_core::instruction::SetFeeBps { new_bps: 0 }.data(),
            metas(2),
        );
        assert_eq!(match_any(&templates(), &views(&[admin])), None);

        let foreign =
            Instruction::new_with_bytes(Pubkey::new_unique(), &[0u8; 16], metas(2));
        assert_eq!(match_any(&templates(), &views(&[foreign])), None);
    }

    #[test]
    fn required_subsequence_cannot_be_satisfied_by_allowed_alone() {
        // A tx with only the ed25519 precompile (allowed on the write
        // template) but no execute_write must not match.
        assert_eq!(match_any(&templates(), &views(&[ed25519_ix()])), None);
    }

    #[test]
    fn describe_names_known_programs() {
        let s = describe_instructions(&views(&[
            compute_budget_ix(),
            ata_ix(1),
            ed25519_ix(),
            system_transfer_ix(),
            spl_transfer_ix(),
        ]));
        assert!(s.contains("ComputeBudget"), "{s}");
        assert!(s.contains("AssociatedTokenAccount"), "{s}");
        assert!(s.contains("Ed25519SigVerify"), "{s}");
        assert!(s.contains("System"), "{s}");
        assert!(s.contains("SplToken"), "{s}");
    }
}
