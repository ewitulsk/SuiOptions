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

/// Compact, log-safe summary of a PTB's command sequence: each Move call as
/// `package::module::function<type_arg_count>`, each other command by kind,
/// joined by `; `. The matcher is otherwise opaque on refusal — this turns a
/// bare "matches no template" into something we can diff against the frontend
/// builders.
pub fn describe_ptb(pt: &ProgrammableTransaction) -> String {
    pt.commands
        .iter()
        .map(|cmd| match cmd {
            Command::MoveCall(c) => format!(
                "{}::{}::{}<{}>",
                c.package,
                c.module,
                c.function,
                c.type_arguments.len()
            ),
            Command::SplitCoins(..) => "SplitCoins".to_owned(),
            Command::MergeCoins(..) => "MergeCoins".to_owned(),
            Command::TransferObjects(..) => "TransferObjects".to_owned(),
            Command::MakeMoveVec(..) => "MakeMoveVec".to_owned(),
            Command::Publish(..) => "Publish".to_owned(),
            Command::Upgrade(..) => "Upgrade".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The Sui framework package (`0x2`), home of `coin::zero`.
fn framework() -> ObjectID {
    ObjectID::from_hex_literal("0x2").expect("0x2 is a valid ObjectID")
}

/// Build the sponsored-PTB templates for the protocol frontend.
///
/// Mirrors the builders in `frontend/src/tx/{composer,dashboard,faucet,deepbook,session}.ts`.
/// `test_tokens` is the `(package, module)` of each faucet token (e.g.
/// `(0xpkg, "tbtc")`), only used when `allow_faucet` is set (dev/staging).
/// `deepbook` is DeepBook's UPGRADED package id (the one Move calls target,
/// from token-info); `None` on networks without a DeepBook deployment —
/// no DeepBook PTBs are sponsored there. `session` is the siws_session
/// package id; `None` where session login isn't deployed — no session PTBs
/// are sponsored there.
pub fn protocol_templates(
    protocol: ObjectID,
    test_tokens: &[(ObjectID, String)],
    allow_faucet: bool,
    deepbook: Option<ObjectID>,
    session: Option<ObjectID>,
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

    // Wallet-facing covered-call vault flows (doc 03). Each is a single call
    // with the vault's 3 type args; deposit/initiate_withdraw ride a
    // `coinWithBalance` prelude. Every asset moved is the user's own (their
    // own coins in, receipts/shares/refunds back to them), so the sponsor only
    // risks gas — same posture as the `write`/`buy`/`exercise` wallet flows.
    // The session twins are sponsored separately under `session_vault:*`.
    for function in [
        "deposit",
        "claim_shares",
        "initiate_withdraw",
        "complete_withdraw",
        "instant_withdraw_pending",
    ] {
        let target = t("vault", function);
        templates.push(PtbTemplate {
            name: format!("vault:{function}"),
            required: vec![target.clone()],
            allowed: vec![target.clone()],
            arities: vec![(target, 3)],
        });
    }

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

    // Session-login PTB shapes (siws_session integration). A session user's
    // ephemeral key holds no gas, so EVERY session interaction is sponsored:
    // sign-in/revoke against the session package, and the `_with_session`
    // twins of the protocol flows. Funds only ever move under the on-chain
    // SessionCap the station never holds; the sponsor risks gas alone.
    if let Some(sess) = session {
        let s = |function: &str| MoveTarget::new(sess, "session", function);

        for open in ["verify_and_open_session", "verify_and_open_session_eth"] {
            let open = s(open);
            templates.push(PtbTemplate {
                name: format!("session_open:{}", open.function),
                required: vec![open.clone()],
                allowed: vec![open.clone()],
                arities: vec![(open, 0)],
            });
        }
        for revoke in ["revoke_all", "revoke_all_eth"] {
            let revoke = s(revoke);
            templates.push(PtbTemplate {
                name: format!("session_revoke:{}", revoke.function),
                required: vec![revoke.clone()],
                allowed: vec![revoke.clone()],
                arities: vec![(revoke, 0)],
            });
        }

        let create_account = t("session_account", "create_and_share_account_with_session");
        templates.push(PtbTemplate {
            name: "session_account_create".to_owned(),
            required: vec![create_account.clone()],
            allowed: vec![create_account.clone()],
            arities: vec![(create_account, 0)],
        });

        // write / buy twins: same quote prelude, no executor coins (the
        // account custody funds the trade), so no `coin::zero`.
        let session_write_flow = |name: &str, flow: &str| {
            let targets = vec![
                t("quote", "new_quote"),
                t("quote", "new_signed_quote"),
                t("bucket", flow),
                t("session_bucket", "execute_write_with_session"),
            ];
            PtbTemplate {
                name: name.to_owned(),
                required: targets.clone(),
                allowed: targets,
                arities: vec![(t("session_bucket", "execute_write_with_session"), 3)],
            }
        };
        templates.push(session_write_flow("session_write", "writer_flow"));
        templates.push(session_write_flow("session_buy", "trader_flow"));

        let exercise = t("session_bucket", "exercise_with_session");
        templates.push(PtbTemplate {
            name: "session_exercise".to_owned(),
            required: vec![exercise.clone()],
            allowed: vec![exercise.clone()],
            arities: vec![(exercise, 3)],
        });
        let redeem = t("session_bucket", "redeem_position_with_session");
        templates.push(PtbTemplate {
            name: "session_redeem".to_owned(),
            required: vec![redeem.clone()],
            allowed: vec![redeem.clone()],
            arities: vec![(redeem, 3)],
        });
        let burn = t("session_bucket", "burn_expired_option_with_session");
        templates.push(PtbTemplate {
            name: "session_burn_expired".to_owned(),
            required: vec![burn.clone()],
            allowed: vec![burn.clone()],
            arities: vec![(burn, 3)],
        });

        // Withdraw from custody to an external address. Authorization is a
        // fresh host-wallet signature passed as args (verified on-chain); the
        // entry pays the signed recipient directly, so there is no returned
        // coin / TransferObjects. We still sponsor gas for the PTB shape.
        for function in ["withdraw_with_root_sig", "withdraw_with_root_sig_eth"] {
            let withdraw = t("session_account", function);
            templates.push(PtbTemplate {
                name: format!("session_{function}"),
                required: vec![withdraw.clone()],
                allowed: vec![withdraw.clone()],
                arities: vec![(withdraw, 1)],
            });
        }

        // Deposit into an options account (permissionless on-chain; only
        // moves the sender's own coins in).
        let deposit = t("account", "deposit");
        templates.push(PtbTemplate {
            name: "session_deposit".to_owned(),
            required: vec![deposit.clone()],
            allowed: vec![deposit.clone()],
            arities: vec![(deposit.clone(), 1)],
        });

        // Testnet funding in one PTB: faucet `mint` (returns the coin) →
        // `account::deposit` into custody.
        if allow_faucet {
            for (pkg, module) in test_tokens {
                let mint = MoveTarget::new(*pkg, module, "mint");
                templates.push(PtbTemplate {
                    name: format!("session_fund:{module}"),
                    required: vec![mint.clone(), deposit.clone()],
                    allowed: vec![mint.clone(), deposit.clone()],
                    arities: vec![(mint, 0), (deposit.clone(), 1)],
                });
            }
        }

        // Vault session twins (covered-call vault, doc 03): each is a single
        // custody-funded call with the vault's 3 type args.
        for function in [
            "deposit_with_session",
            "claim_shares_with_session",
            "initiate_withdraw_with_session",
            "complete_withdraw_with_session",
            "instant_withdraw_pending_with_session",
        ] {
            let target = t("session_vault", function);
            templates.push(PtbTemplate {
                name: format!("session_vault:{function}"),
                required: vec![target.clone()],
                allowed: vec![target.clone()],
                arities: vec![(target, 3)],
            });
        }

        // DeepBook session twins: single custody-funded calls — the wrapper
        // does the BalanceManager deposit / order / settle internally, so the
        // PTB is one Move call (unlike the wallet DeepBook shapes below, which
        // build the deposit/proof/order steps as separate commands). Enable
        // takes no type args; the market order carries the pool's (Base, Quote).
        let enable = t("session_deepbook", "enable_trading_with_session");
        templates.push(PtbTemplate {
            name: "session_deepbook:enable_trading".to_owned(),
            required: vec![enable.clone()],
            allowed: vec![enable.clone()],
            arities: vec![(enable, 0)],
        });
        let market = t("session_deepbook", "place_market_order_with_session");
        templates.push(PtbTemplate {
            name: "session_deepbook:place_market_order".to_owned(),
            required: vec![market.clone()],
            allowed: vec![market.clone()],
            arities: vec![(market, 2)],
        });
    }

    // DeepBook PTB shapes (SO-154 venue creation + SO-157 trading). Coin
    // funding rides in via `coinWithBalance` (benign SplitCoins/MergeCoins
    // preludes); the sponsor only ever risks gas — every asset moved is the
    // user's own. Closed `allowed` sets keep other DeepBook functions from
    // tagging along.
    if let Some(db) = deepbook {
        let d = |module: &str, function: &str| MoveTarget::new(db, module, function);
        let proof = d("balance_manager", "generate_proof_as_owner");
        let deposit = d("balance_manager", "deposit");
        let share = MoveTarget::new(framework(), "transfer", "public_share_object");

        let create = d("pool", "create_permissionless_pool");
        templates.push(PtbTemplate {
            name: "deepbook_create_pool".to_owned(),
            required: vec![create.clone()],
            allowed: vec![create.clone()],
            arities: vec![(create, 2)],
        });

        // Enable trading: new → register (emits the discovery event) → share.
        let bm_new = d("balance_manager", "new");
        let bm_register = d("balance_manager", "register_balance_manager");
        templates.push(PtbTemplate {
            name: "deepbook_bm_create".to_owned(),
            required: vec![bm_new.clone(), bm_register.clone(), share.clone()],
            allowed: vec![bm_new, bm_register, share.clone()],
            arities: vec![(share, 1)],
        });

        // Orders: optional exact-amount deposit, owner proof, place.
        let place_limit = d("pool", "place_limit_order");
        templates.push(PtbTemplate {
            name: "deepbook_place_limit".to_owned(),
            required: vec![proof.clone(), place_limit.clone()],
            allowed: vec![deposit.clone(), proof.clone(), place_limit.clone()],
            arities: vec![(place_limit, 2), (deposit.clone(), 1)],
        });
        let place_market = d("pool", "place_market_order");
        templates.push(PtbTemplate {
            name: "deepbook_place_market".to_owned(),
            required: vec![proof.clone(), place_market.clone()],
            allowed: vec![deposit.clone(), proof.clone(), place_market.clone()],
            arities: vec![(place_market.clone(), 2), (deposit.clone(), 1)],
        });

        // Cancels.
        let cancel = d("pool", "cancel_order");
        templates.push(PtbTemplate {
            name: "deepbook_cancel_order".to_owned(),
            required: vec![proof.clone(), cancel.clone()],
            allowed: vec![proof.clone(), cancel.clone()],
            arities: vec![(cancel, 2)],
        });
        let cancel_all = d("pool", "cancel_all_orders");
        templates.push(PtbTemplate {
            name: "deepbook_cancel_all".to_owned(),
            required: vec![proof.clone(), cancel_all.clone()],
            allowed: vec![proof.clone(), cancel_all.clone()],
            arities: vec![(cancel_all, 2)],
        });

        // Settle + drain assets back to the wallet (TransferObjects is a benign
        // command). Covers both "withdraw all" (base + quote) and the
        // single-asset "withdraw this position token" — the latter is the same
        // shape with one `withdraw_all` instead of two.
        let settle = d("pool", "withdraw_settled_amounts");
        let withdraw_all = d("balance_manager", "withdraw_all");
        templates.push(PtbTemplate {
            name: "deepbook_withdraw".to_owned(),
            required: vec![proof.clone(), settle.clone()],
            allowed: vec![proof.clone(), settle.clone(), withdraw_all.clone()],
            arities: vec![(settle.clone(), 2), (withdraw_all.clone(), 1)],
        });

        // Market buy/sell that delivers proceeds to the wallet in one PTB
        // (frontend `buildPlaceMarketOrderTx` with a `recipient`): fills settle
        // into the BM, so the same tx mints a fresh proof, settles, and drains
        // both assets back out (proof → place_market → proof → settle →
        // withdraw_all ×1-2). The session twin folds this into a single
        // `_with_session` call, which is why session market orders were
        // sponsored and wallet ones were not. Every asset moved is the user's
        // own — same posture as `deepbook_withdraw` — so the sponsor only risks
        // gas. Kept separate from `deepbook_place_market` so a plain order still
        // can't smuggle a withdraw.
        templates.push(PtbTemplate {
            name: "deepbook_place_market_withdraw".to_owned(),
            required: vec![
                proof.clone(),
                place_market.clone(),
                settle.clone(),
                withdraw_all.clone(),
            ],
            allowed: vec![
                deposit.clone(),
                proof.clone(),
                place_market.clone(),
                settle.clone(),
                withdraw_all.clone(),
            ],
            arities: vec![
                (place_market.clone(), 2),
                (deposit.clone(), 1),
                (settle.clone(), 2),
                (withdraw_all.clone(), 1),
            ],
        });

        // Exercise an option whose coin the user parked in their DeepBook
        // trading account: settle + withdraw the option coin out of the BM, then
        // `bucket::exercise`. This is the one shape that legitimately crosses
        // the protocol/DeepBook boundary — every asset moved is still the
        // user's own (their BM coin out, underlying back to them), so the
        // sponsor only risks gas. The closed `allowed` set keeps anything else
        // from riding along.
        let exercise = t("bucket", "exercise");
        templates.push(PtbTemplate {
            name: "exercise_with_bm_withdraw".to_owned(),
            required: vec![proof.clone(), settle.clone(), withdraw_all.clone(), exercise.clone()],
            allowed: vec![proof, settle.clone(), withdraw_all.clone(), exercise.clone()],
            arities: vec![(settle, 2), (withdraw_all, 1), (exercise, 3)],
        });
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

    fn deepbook_pkg() -> ObjectID {
        ObjectID::from_hex_literal("0x22be4c").unwrap()
    }

    fn session_pkg() -> ObjectID {
        ObjectID::from_hex_literal("0x5e55").unwrap()
    }

    fn templates() -> Vec<PtbTemplate> {
        protocol_templates(
            pkg(),
            &[(pkg(), "tbtc".to_owned())],
            true,
            Some(deepbook_pkg()),
            Some(session_pkg()),
        )
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
        let no_faucet =
            protocol_templates(pkg(), &[(pkg(), "tbtc".to_owned())], false, None, None);
        let pt = build(&[(target("tbtc", "mint_to_sender"), 0)], false);
        assert_eq!(match_any(&no_faucet, &pt), None);
    }

    #[test]
    fn deepbook_create_pool_matches_with_coin_prelude() {
        // Mirrors frontend buildCreateVenueTx: a coinWithBalance prelude
        // (benign SplitCoins) then create_permissionless_pool<CALL, TUSDC>.
        let pt = build(
            &[(
                MoveTarget::new(deepbook_pkg(), "pool", "create_permissionless_pool"),
                2,
            )],
            true,
        );
        assert_eq!(match_any(&templates(), &pt), Some("deepbook_create_pool"));
    }

    #[test]
    fn deepbook_trading_templates_match_their_frontend_shapes() {
        let d = |module: &str, function: &str| MoveTarget::new(deepbook_pkg(), module, function);
        // Enable trading: new → register → public_share_object<BM>.
        let bm = build(
            &[
                (d("balance_manager", "new"), 0),
                (d("balance_manager", "register_balance_manager"), 0),
                (MoveTarget::new(framework(), "transfer", "public_share_object"), 1),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &bm), Some("deepbook_bm_create"));

        // Limit order with a coinWithBalance deposit prelude.
        let limit = build(
            &[
                (d("balance_manager", "deposit"), 1),
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "place_limit_order"), 2),
            ],
            true,
        );
        assert_eq!(match_any(&templates(), &limit), Some("deepbook_place_limit"));

        // Market order without a deposit.
        let market = build(
            &[
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "place_market_order"), 2),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &market), Some("deepbook_place_market"));

        // Market buy/sell that settles + drains fills back to the wallet in one
        // PTB (buildPlaceMarketOrderTx with a recipient): optional deposit →
        // proof → place_market → fresh proof → settle → withdraw_all ×2
        // (+ benign TransferObjects). This is the wallet "buy/sell on DeepBook"
        // shape the gas station was refusing.
        let market_withdraw = build(
            &[
                (d("balance_manager", "deposit"), 1),
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "place_market_order"), 2),
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "withdraw_settled_amounts"), 2),
                (d("balance_manager", "withdraw_all"), 1),
                (d("balance_manager", "withdraw_all"), 1),
            ],
            false,
        );
        assert_eq!(
            match_any(&templates(), &market_withdraw),
            Some("deepbook_place_market_withdraw"),
        );

        // Same without the optional deposit prelude (e.g. a market sell funded
        // entirely from the BM) still matches.
        let market_withdraw_no_deposit = build(
            &[
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "place_market_order"), 2),
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "withdraw_settled_amounts"), 2),
                (d("balance_manager", "withdraw_all"), 1),
                (d("balance_manager", "withdraw_all"), 1),
            ],
            false,
        );
        assert_eq!(
            match_any(&templates(), &market_withdraw_no_deposit),
            Some("deepbook_place_market_withdraw"),
        );

        // Cancels.
        let cancel = build(
            &[
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "cancel_order"), 2),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &cancel), Some("deepbook_cancel_order"));
        let cancel_all = build(
            &[
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "cancel_all_orders"), 2),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &cancel_all), Some("deepbook_cancel_all"));

        // Withdraw: proof → settle → withdraw_all ×2 (+ benign TransferObjects).
        let withdraw = build(
            &[
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "withdraw_settled_amounts"), 2),
                (d("balance_manager", "withdraw_all"), 1),
                (d("balance_manager", "withdraw_all"), 1),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &withdraw), Some("deepbook_withdraw"));

        // Single-asset withdraw (buildWithdrawBaseTx): proof → settle →
        // withdraw_all ×1 — the "withdraw this position token" button. Same
        // template, one fewer withdraw_all.
        let withdraw_base = build(
            &[
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "withdraw_settled_amounts"), 2),
                (d("balance_manager", "withdraw_all"), 1),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &withdraw_base), Some("deepbook_withdraw"));
    }

    #[test]
    fn exercise_with_bm_withdraw_matches_and_rejects_riders() {
        let d = |module: &str, function: &str| MoveTarget::new(deepbook_pkg(), module, function);
        // buildExerciseTx with bmWithdraw: settle + withdraw the option coin out
        // of the BM, then bucket::exercise (+ benign split/merge/transfer).
        let ex = build(
            &[
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "withdraw_settled_amounts"), 2),
                (d("balance_manager", "withdraw_all"), 1),
                (target("bucket", "exercise"), 3),
            ],
            true,
        );
        assert_eq!(match_any(&templates(), &ex), Some("exercise_with_bm_withdraw"));

        // A plain exercise (no BM withdraw) still matches the wallet `exercise`
        // template, not this one.
        let plain = build(&[(target("bucket", "exercise"), 3)], true);
        assert_eq!(match_any(&templates(), &plain), Some("exercise"));

        // Withdraw without the exercise must NOT match the combined template
        // (it requires `exercise`); it falls through to `deepbook_withdraw`.
        let no_exercise = build(
            &[
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "withdraw_settled_amounts"), 2),
                (d("balance_manager", "withdraw_all"), 1),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &no_exercise), Some("deepbook_withdraw"));

        // A foreign DeepBook call cannot ride along with the exercise.
        let rider = build(
            &[
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "withdraw_settled_amounts"), 2),
                (d("balance_manager", "withdraw_all"), 1),
                (d("pool", "place_market_order"), 2),
                (target("bucket", "exercise"), 3),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &rider), None);
    }

    #[test]
    fn deepbook_order_templates_reject_withdraw_riders() {
        let d = |module: &str, function: &str| MoveTarget::new(deepbook_pkg(), module, function);
        // A withdraw smuggled into an order PTB matches no template: the
        // order templates don't allow withdraw_all, and the withdraw
        // template doesn't allow place_limit_order.
        let evil = build(
            &[
                (d("balance_manager", "generate_proof_as_owner"), 0),
                (d("pool", "place_limit_order"), 2),
                (d("balance_manager", "withdraw_all"), 1),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &evil), None);
        // Sharing an arbitrary framework object only matches inside the
        // bm-create sequence — alone it matches nothing.
        let share_only = build(
            &[(MoveTarget::new(framework(), "transfer", "public_share_object"), 1)],
            false,
        );
        assert_eq!(match_any(&templates(), &share_only), None);
    }

    #[test]
    fn deepbook_create_pool_rejects_riders_arity_and_unconfigured() {
        // A second DeepBook function riding along is refused.
        let with_rider = build(
            &[
                (
                    MoveTarget::new(deepbook_pkg(), "pool", "create_permissionless_pool"),
                    2,
                ),
                (MoveTarget::new(deepbook_pkg(), "pool", "place_limit_order"), 2),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &with_rider), None);

        // Wrong type-arg arity is refused.
        let bad_arity = build(
            &[(
                MoveTarget::new(deepbook_pkg(), "pool", "create_permissionless_pool"),
                3,
            )],
            false,
        );
        assert_eq!(match_any(&templates(), &bad_arity), None);

        // No deepbook configured (devnet) → never sponsored.
        let no_db = protocol_templates(pkg(), &[], false, None, None);
        let pt = build(
            &[(
                MoveTarget::new(deepbook_pkg(), "pool", "create_permissionless_pool"),
                2,
            )],
            false,
        );
        assert_eq!(match_any(&no_db, &pt), None);
    }

    #[test]
    fn session_open_and_revoke_match() {
        let s = |function: &str| MoveTarget::new(session_pkg(), "session", function);
        for f in [
            "verify_and_open_session",
            "verify_and_open_session_eth",
            "revoke_all",
            "revoke_all_eth",
        ] {
            let pt = build(&[(s(f), 0)], false);
            assert!(match_any(&templates(), &pt).is_some(), "{f} should match");
        }
        // Type args on the open call are refused (the entrypoints are
        // non-generic since the multi-asset account rework).
        let pt = build(&[(s("verify_and_open_session"), 1)], false);
        assert_eq!(match_any(&templates(), &pt), None);
    }

    #[test]
    fn session_flows_match_their_frontend_shapes() {
        // account create
        let create = build(
            &[(target("session_account", "create_and_share_account_with_session"), 0)],
            false,
        );
        assert_eq!(match_any(&templates(), &create), Some("session_account_create"));

        // write / buy twins: quote prelude + execute_write_with_session, no
        // coin::zero.
        let write = build(
            &[
                (target("quote", "new_quote"), 0),
                (target("quote", "new_signed_quote"), 0),
                (target("bucket", "writer_flow"), 0),
                (target("session_bucket", "execute_write_with_session"), 3),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &write), Some("session_write"));
        let buy = build(
            &[
                (target("quote", "new_quote"), 0),
                (target("quote", "new_signed_quote"), 0),
                (target("bucket", "trader_flow"), 0),
                (target("session_bucket", "execute_write_with_session"), 3),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &buy), Some("session_buy"));

        // exercise / redeem / burn twins.
        let ex = build(&[(target("session_bucket", "exercise_with_session"), 3)], false);
        assert_eq!(match_any(&templates(), &ex), Some("session_exercise"));
        let rd = build(&[(target("session_bucket", "redeem_position_with_session"), 3)], false);
        assert_eq!(match_any(&templates(), &rd), Some("session_redeem"));
        let burn = build(
            &[(target("session_bucket", "burn_expired_option_with_session"), 3)],
            false,
        );
        assert_eq!(match_any(&templates(), &burn), Some("session_burn_expired"));

        // root-signed external withdrawal (Solana + Ethereum variants) / deposit.
        let wd = build(&[(target("session_account", "withdraw_with_root_sig"), 1)], false);
        assert_eq!(match_any(&templates(), &wd), Some("session_withdraw_with_root_sig"));
        let wd_eth =
            build(&[(target("session_account", "withdraw_with_root_sig_eth"), 1)], false);
        assert_eq!(
            match_any(&templates(), &wd_eth),
            Some("session_withdraw_with_root_sig_eth"),
        );
        let dep = build(&[(target("account", "deposit"), 1)], true);
        assert_eq!(match_any(&templates(), &dep), Some("session_deposit"));

        // testnet funding: faucet mint (returning) → deposit.
        let fund = build(
            &[
                (target("tbtc", "mint"), 0),
                (target("account", "deposit"), 1),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &fund), Some("session_fund:tbtc"));

        // vault twins: one custody-funded call each, 3 type args.
        for f in [
            "deposit_with_session",
            "claim_shares_with_session",
            "initiate_withdraw_with_session",
            "complete_withdraw_with_session",
            "instant_withdraw_pending_with_session",
        ] {
            let pt = build(&[(target("session_vault", f), 3)], false);
            assert_eq!(
                match_any(&templates(), &pt),
                Some(format!("session_vault:{f}").as_str()),
            );
            // wrong arity refused
            let bad = build(&[(target("session_vault", f), 2)], false);
            assert_eq!(match_any(&templates(), &bad), None);
        }
        // the wallet-facing vault functions are sponsored under `vault:*`.
        for f in [
            "deposit",
            "claim_shares",
            "initiate_withdraw",
            "complete_withdraw",
            "instant_withdraw_pending",
        ] {
            let pt = build(&[(target("vault", f), 3)], false);
            assert_eq!(match_any(&templates(), &pt), Some(format!("vault:{f}").as_str()));
            // wrong arity refused
            let bad = build(&[(target("vault", f), 2)], false);
            assert_eq!(match_any(&templates(), &bad), None);
        }

        // DeepBook session twins: enable (no type args) + market order
        // (2 type args, the pool's Base/Quote).
        let enable = build(&[(target("session_deepbook", "enable_trading_with_session"), 0)], false);
        assert_eq!(
            match_any(&templates(), &enable),
            Some("session_deepbook:enable_trading"),
        );
        let market =
            build(&[(target("session_deepbook", "place_market_order_with_session"), 2)], false);
        assert_eq!(
            match_any(&templates(), &market),
            Some("session_deepbook:place_market_order"),
        );
        // wrong arity refused
        let bad = build(&[(target("session_deepbook", "place_market_order_with_session"), 3)], false);
        assert_eq!(match_any(&templates(), &bad), None);
    }

    #[test]
    fn session_templates_reject_riders_and_unconfigured() {
        // A withdraw smuggled into a session buy PTB matches no template.
        let evil = build(
            &[
                (target("quote", "new_quote"), 0),
                (target("quote", "new_signed_quote"), 0),
                (target("bucket", "trader_flow"), 0),
                (target("session_bucket", "execute_write_with_session"), 3),
                (target("session_account", "withdraw_with_root_sig"), 1),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &evil), None);

        // The wallet `execute_write` cannot be smuggled under a session
        // template (and vice versa — the shapes are disjoint).
        let mixed = build(
            &[
                (target("quote", "new_quote"), 0),
                (target("quote", "new_signed_quote"), 0),
                (target("bucket", "writer_flow"), 0),
                (target("bucket", "execute_write"), 3),
                (target("session_bucket", "execute_write_with_session"), 3),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &mixed), None);

        // No session package configured → session PTBs are never sponsored.
        let no_session = protocol_templates(pkg(), &[], false, None, None);
        let open = build(
            &[(MoveTarget::new(session_pkg(), "session", "verify_and_open_session"), 0)],
            false,
        );
        assert_eq!(match_any(&no_session, &open), None);
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
