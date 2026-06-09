//! PTB template matching for the gas station.
//!
//! The frontend (`frontend/src/tx/*`) produces a small, closed set of PTB
//! shapes. Rather than sponsor anything that only touches an allow-listed
//! *package* — which, because SUI's coin type lives in `0x2`, leaks the whole
//! framework into the allowlist — we match each incoming `TransactionKind`
//! against the exact shapes we ship. Anything that matches no template is
//! refused.
//!
//! What we pin: the ordered subsequence of Move-call targets
//! (`package::module::function`), a closed set of targets that may appear at
//! all, and the type-argument arity of the anchor calls. What we let vary: the
//! `coinWithBalance` coin-selection prelude (a non-deterministic run of
//! `SplitCoins`/`MergeCoins`/`MergeCoins`), since its shape depends on the
//! user's coins. Argument wiring is left to the on-chain dry run plus the Move
//! contract's own invariants — the station's job is only to refuse paying for
//! calls into functions it does not endorse.

use std::fmt;

use sui_types::base_types::ObjectID;
use sui_types::transaction::{Command, ProgrammableMoveCall, ProgrammableTransaction};

/// A fully-qualified Move call target (`package::module::function`).
#[derive(Clone, PartialEq, Eq)]
pub struct MoveTarget {
    pub package: ObjectID,
    pub module: String,
    pub function: String,
}

impl MoveTarget {
    pub fn new(package: ObjectID, module: &str, function: &str) -> Self {
        Self {
            package,
            module: module.to_owned(),
            function: function.to_owned(),
        }
    }

    fn matches_call(&self, call: &ProgrammableMoveCall) -> bool {
        call.package == self.package && call.module == self.module && call.function == self.function
    }
}

impl fmt::Display for MoveTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{}::{}", self.package, self.module, self.function)
    }
}

/// One sponsored PTB shape.
pub struct PtbTemplate {
    pub name: String,
    /// Move-call targets that must appear, in this order, as a subsequence of
    /// the PTB's Move calls.
    pub required: Vec<MoveTarget>,
    /// Every Move call in the PTB must target one of these. Superset of
    /// `required` (e.g. adds `0x2::coin::zero`).
    pub allowed: Vec<MoveTarget>,
    /// Expected type-argument count, keyed by target. Targets absent here are
    /// not arity-checked.
    pub arities: Vec<(MoveTarget, usize)>,
}

impl PtbTemplate {
    /// Does `pt` match this template?
    pub fn matches(&self, pt: &ProgrammableTransaction) -> bool {
        // Collect Move calls in order; every non-Move-call command must be one
        // of the benign value-plumbing kinds the frontend emits (Publish /
        // Upgrade are rejected here and, redundantly, by the sponsor guard).
        let mut calls: Vec<&ProgrammableMoveCall> = Vec::new();
        for cmd in &pt.commands {
            match cmd {
                Command::MoveCall(call) => calls.push(call.as_ref()),
                Command::SplitCoins(..)
                | Command::MergeCoins(..)
                | Command::TransferObjects(..)
                | Command::MakeMoveVec(..) => {}
                Command::Publish(..) | Command::Upgrade(..) => return false,
            }
        }

        // (a) closed target set + (b) type-arg arity on anchor calls.
        for call in &calls {
            let Some(target) = self.allowed.iter().find(|t| t.matches_call(call)) else {
                return false;
            };
            if let Some((_, arity)) = self.arities.iter().find(|(t, _)| t == target) {
                if call.type_arguments.len() != *arity {
                    return false;
                }
            }
        }

        // (c) required targets appear, in order, as a subsequence.
        let mut req = self.required.iter();
        let mut want = req.next();
        for call in &calls {
            if let Some(t) = want {
                if t.matches_call(call) {
                    want = req.next();
                }
            }
        }
        want.is_none()
    }
}

/// Returns the name of the first template `pt` matches, if any.
pub fn match_any<'a>(templates: &'a [PtbTemplate], pt: &ProgrammableTransaction) -> Option<&'a str> {
    templates
        .iter()
        .find(|t| t.matches(pt))
        .map(|t| t.name.as_str())
}

/// The Sui framework package (`0x2`), home of `coin::zero`.
fn framework() -> ObjectID {
    ObjectID::from_hex_literal("0x2").expect("0x2 is a valid ObjectID")
}

/// Build the sponsored-PTB templates for the protocol frontend.
///
/// Mirrors the builders in `frontend/src/tx/{composer,dashboard,faucet}.ts`.
/// `test_tokens` is the `(package, module)` of each faucet token (e.g.
/// `(0xpkg, "tbtc")`), only used when `allow_faucet` is set (dev/staging).
pub fn protocol_templates(
    protocol: ObjectID,
    test_tokens: &[(ObjectID, String)],
    allow_faucet: bool,
) -> Vec<PtbTemplate> {
    let t = |module: &str, function: &str| MoveTarget::new(protocol, module, function);
    let coin_zero = MoveTarget::new(framework(), "coin", "zero");

    // write / buy differ only by writer_flow vs trader_flow.
    let execute_write_flow = |name: &str, flow: &str| {
        let targets = vec![
            t("quote", "new_quote"),
            t("quote", "new_signed_quote"),
            t("bucket", flow),
            coin_zero.clone(),
            t("bucket", "execute_write"),
        ];
        PtbTemplate {
            name: name.to_owned(),
            required: targets.clone(),
            allowed: targets,
            arities: vec![(t("bucket", "execute_write"), 3)],
        }
    };

    let mut templates = vec![
        execute_write_flow("write", "writer_flow"),
        execute_write_flow("buy", "trader_flow"),
        PtbTemplate {
            name: "exercise".to_owned(),
            required: vec![t("bucket", "exercise")],
            allowed: vec![t("bucket", "exercise")],
            arities: vec![(t("bucket", "exercise"), 3)],
        },
        PtbTemplate {
            name: "redeem".to_owned(),
            required: vec![t("bucket", "redeem_position")],
            allowed: vec![t("bucket", "redeem_position")],
            arities: vec![(t("bucket", "redeem_position"), 3)],
        },
    ];

    if allow_faucet {
        for (pkg, module) in test_tokens {
            let mint = MoveTarget::new(*pkg, module, "mint_to_sender");
            templates.push(PtbTemplate {
                name: format!("faucet_mint:{module}"),
                required: vec![mint.clone()],
                allowed: vec![mint.clone()],
                arities: vec![(mint, 0)],
            });
        }
    }

    templates
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
    use sui_types::transaction::TransactionKind;
    use sui_types::Identifier;
    use sui_types::TypeTag;

    fn pkg() -> ObjectID {
        ObjectID::from_hex_literal("0xabc").unwrap()
    }

    /// Build a PTB from a list of `(target, type-arg count)` Move calls, with an
    /// optional benign coin-prep prelude inserted before the last call (mimics
    /// `coinWithBalance`).
    fn build(calls: &[(MoveTarget, usize)], coin_prep_before_last: bool) -> ProgrammableTransaction {
        let mut b = ProgrammableTransactionBuilder::new();
        for (i, (target, n_types)) in calls.iter().enumerate() {
            if coin_prep_before_last && i + 1 == calls.len() {
                // A split off the gas coin — a benign non-Move-call command.
                let amt = b.pure(1u64).unwrap();
                b.command(Command::SplitCoins(
                    sui_types::transaction::Argument::GasCoin,
                    vec![amt],
                ));
            }
            let type_args: Vec<TypeTag> = (0..*n_types).map(|_| TypeTag::U64).collect();
            b.programmable_move_call(
                target.package,
                Identifier::new(target.module.clone()).unwrap(),
                Identifier::new(target.function.clone()).unwrap(),
                type_args,
                vec![],
            );
        }
        b.finish()
    }

    fn templates() -> Vec<PtbTemplate> {
        protocol_templates(pkg(), &[(pkg(), "tbtc".to_owned())], true)
    }

    fn target(module: &str, function: &str) -> MoveTarget {
        MoveTarget::new(pkg(), module, function)
    }

    #[test]
    fn write_flow_matches() {
        let pt = build(
            &[
                (target("quote", "new_quote"), 0),
                (target("quote", "new_signed_quote"), 0),
                (target("bucket", "writer_flow"), 0),
                (MoveTarget::new(framework(), "coin", "zero"), 1),
                (target("bucket", "execute_write"), 3),
            ],
            true,
        );
        assert_eq!(match_any(&templates(), &pt), Some("write"));
    }

    #[test]
    fn buy_flow_matches() {
        let pt = build(
            &[
                (target("quote", "new_quote"), 0),
                (target("quote", "new_signed_quote"), 0),
                (target("bucket", "trader_flow"), 0),
                (MoveTarget::new(framework(), "coin", "zero"), 1),
                (target("bucket", "execute_write"), 3),
            ],
            true,
        );
        assert_eq!(match_any(&templates(), &pt), Some("buy"));
    }

    #[test]
    fn exercise_and_redeem_match() {
        let ex = build(&[(target("bucket", "exercise"), 3)], true);
        assert_eq!(match_any(&templates(), &ex), Some("exercise"));
        let rd = build(&[(target("bucket", "redeem_position"), 3)], false);
        assert_eq!(match_any(&templates(), &rd), Some("redeem"));
    }

    #[test]
    fn faucet_mint_matches() {
        let pt = build(&[(target("tbtc", "mint_to_sender"), 0)], false);
        assert_eq!(match_any(&templates(), &pt), Some("faucet_mint:tbtc"));
    }

    #[test]
    fn faucet_rejected_when_disabled() {
        let no_faucet = protocol_templates(pkg(), &[(pkg(), "tbtc".to_owned())], false);
        let pt = build(&[(target("tbtc", "mint_to_sender"), 0)], false);
        assert_eq!(match_any(&no_faucet, &pt), None);
    }

    #[test]
    fn foreign_call_injection_rejected() {
        let evil = ObjectID::from_hex_literal("0xdead").unwrap();
        let pt = build(
            &[
                (target("quote", "new_quote"), 0),
                (target("quote", "new_signed_quote"), 0),
                (target("bucket", "writer_flow"), 0),
                (MoveTarget::new(framework(), "coin", "zero"), 1),
                (MoveTarget::new(evil, "drain", "all"), 0),
                (target("bucket", "execute_write"), 3),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &pt), None);
    }

    #[test]
    fn framework_call_other_than_coin_zero_rejected() {
        // Proves we whitelist `0x2::coin::zero`, not all of `0x2`.
        let pt = build(
            &[
                (target("quote", "new_quote"), 0),
                (target("quote", "new_signed_quote"), 0),
                (target("bucket", "writer_flow"), 0),
                (MoveTarget::new(framework(), "transfer", "public_transfer"), 1),
                (target("bucket", "execute_write"), 3),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &pt), None);
    }

    #[test]
    fn wrong_type_arg_arity_rejected() {
        let pt = build(&[(target("bucket", "exercise"), 2)], false);
        assert_eq!(match_any(&templates(), &pt), None);
    }

    #[test]
    fn admin_call_rejected() {
        let pt = build(&[(target("admin", "set_fee_bps"), 0)], false);
        assert_eq!(match_any(&templates(), &pt), None);
        let withdraw = build(&[(target("treasury", "withdraw"), 1)], false);
        assert_eq!(match_any(&templates(), &withdraw), None);
    }

    #[test]
    fn publish_rejected() {
        let mut b = ProgrammableTransactionBuilder::new();
        b.command(Command::Publish(vec![vec![0u8]], vec![]));
        let pt = b.finish();
        assert_eq!(match_any(&templates(), &pt), None);
        // sanity: a TransactionKind wrapper round-trips
        let _ = TransactionKind::ProgrammableTransaction(pt);
    }
}
