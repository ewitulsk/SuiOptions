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
//! `coinWithBalance` coin-selection plumbing (a non-deterministic run of
//! `SplitCoins`/`MergeCoins` plus the value-neutral `0x2::coin::{zero,
//! destroy_zero}` cleanup calls the SDK injects around it), since its shape
//! depends on the user's coins. Argument wiring is left to the on-chain dry run plus the Move
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

/// What a `required`/`allowed` slot may match: an exact pinned target, or —
/// for the collateral abstraction's MM-specified `release` implementation —
/// ANY package/module whose function is named exactly `release` with exactly
/// one type argument (plan §6). At most ONE call per PTB may match via the
/// wildcard, and a wildcard never substitutes for a pinned target (exact
/// matches are tried first).
#[derive(Clone, PartialEq, Eq)]
pub enum TargetMatcher {
    Exact(MoveTarget),
    /// Any package, any module — function name `release`, exactly 1 type arg.
    AnyRelease,
}

impl TargetMatcher {
    fn matches_call(&self, call: &ProgrammableMoveCall) -> bool {
        match self {
            TargetMatcher::Exact(t) => t.matches_call(call),
            TargetMatcher::AnyRelease => {
                call.function.as_str() == "release" && call.type_arguments.len() == 1
            }
        }
    }
}

impl fmt::Display for TargetMatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetMatcher::Exact(t) => t.fmt(f),
            TargetMatcher::AnyRelease => write!(f, "*::*::release<1>"),
        }
    }
}

impl From<MoveTarget> for TargetMatcher {
    fn from(t: MoveTarget) -> Self {
        TargetMatcher::Exact(t)
    }
}

/// One sponsored PTB shape.
pub struct PtbTemplate {
    pub name: String,
    /// Matchers that must be satisfied, in this order, as a subsequence of
    /// the PTB's Move calls.
    pub required: Vec<TargetMatcher>,
    /// Every Move call in the PTB must satisfy one of these. Superset of
    /// `required` (e.g. adds an optional exact target).
    pub allowed: Vec<TargetMatcher>,
    /// Expected type-argument count, keyed by exact target. Targets absent
    /// here are not arity-checked (`AnyRelease` pins its own name + arity).
    pub arities: Vec<(MoveTarget, usize)>,
}

impl PtbTemplate {
    /// Template with only exact targets (no wildcard slot).
    fn exact_only(
        name: String,
        required: Vec<MoveTarget>,
        allowed: Vec<MoveTarget>,
        arities: Vec<(MoveTarget, usize)>,
    ) -> Self {
        Self {
            name,
            required: required.into_iter().map(TargetMatcher::Exact).collect(),
            allowed: allowed.into_iter().map(TargetMatcher::Exact).collect(),
            arities,
        }
    }

    /// Does `pt` match this template?
    ///
    /// Wildcard posture (plan §6): the `AnyRelease` slot sponsors a call into
    /// an UNREVIEWED package, but that call only ever receives the potato
    /// reference and the MM's own collateral object — it cannot touch sponsor
    /// or executor assets not passed to it, so the sponsor risks gas alone
    /// (bounded by `max_gas_budget_mist`). To keep that surface minimal, at
    /// most one call per PTB may match via the wildcard, and exact matchers
    /// always win over the wildcard so a foreign `release` can never stand in
    /// for a pinned protocol call.
    pub fn matches(&self, pt: &ProgrammableTransaction) -> bool {
        // Collect Move calls in order; every non-Move-call command must be one
        // of the benign value-plumbing kinds the frontend emits (Publish /
        // Upgrade are rejected here and, redundantly, by the sponsor guard).
        // The value-neutral `0x2::coin::{zero,destroy_zero}` calls the
        // `coinWithBalance` resolver injects are skipped here too — like the
        // plumbing commands, they can appear in any template without being
        // listed in its `allowed` set.
        let mut calls: Vec<&ProgrammableMoveCall> = Vec::new();
        for cmd in &pt.commands {
            match cmd {
                Command::MoveCall(call) if is_benign_coin_primitive(call.as_ref()) => {}
                Command::MoveCall(call) => calls.push(call.as_ref()),
                Command::SplitCoins(..)
                | Command::MergeCoins(..)
                | Command::TransferObjects(..)
                | Command::MakeMoveVec(..) => {}
                Command::Publish(..) | Command::Upgrade(..) => return false,
            }
        }

        // (a) closed target set + (b) type-arg arity on anchor calls. Exact
        // matchers are tried before the wildcard so a pinned call is never
        // "used up" by the AnyRelease slot; wildcard matches are capped at 1.
        let mut wildcard_matches = 0usize;
        for call in &calls {
            let exact = self.allowed.iter().find(|m| match m {
                TargetMatcher::Exact(t) => t.matches_call(call),
                TargetMatcher::AnyRelease => false,
            });
            match exact {
                Some(TargetMatcher::Exact(target)) => {
                    if let Some((_, arity)) = self.arities.iter().find(|(t, _)| t == target) {
                        if call.type_arguments.len() != *arity {
                            return false;
                        }
                    }
                }
                _ => {
                    let wildcard_allowed = self
                        .allowed
                        .iter()
                        .any(|m| matches!(m, TargetMatcher::AnyRelease) && m.matches_call(call));
                    if !wildcard_allowed {
                        return false;
                    }
                    wildcard_matches += 1;
                    if wildcard_matches > 1 {
                        return false;
                    }
                }
            }
        }

        // (c) required matchers are satisfied, in order, as a subsequence.
        // The same exact-first discipline applies: a call that matches a
        // pinned target satisfies only that pinned slot, never a pending
        // AnyRelease slot.
        let mut req = self.required.iter();
        let mut want = req.next();
        for call in &calls {
            if let Some(m) = want {
                let satisfied = match m {
                    TargetMatcher::Exact(_) => m.matches_call(call),
                    TargetMatcher::AnyRelease => {
                        // Don't let a pinned call double as the wildcard slot.
                        m.matches_call(call)
                            && !self.allowed.iter().any(|a| {
                                matches!(a, TargetMatcher::Exact(t) if t.matches_call(call))
                            })
                    }
                };
                if satisfied {
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

/// The Move stdlib package (`0x1`), home of `option::{some,none}`.
fn stdlib() -> ObjectID {
    ObjectID::from_hex_literal("0x1").expect("0x1 is a valid ObjectID")
}

/// The value-neutral `0x2::coin` primitives the `coinWithBalance` intent
/// resolver injects around its split/merge prelude: `zero<T>()` mints an empty
/// coin, `destroy_zero<T>(c)` aborts unless `c` is empty. Neither can move
/// value, so — exactly like the `SplitCoins`/`MergeCoins` plumbing — they may
/// appear in any template without being listed in its `allowed` set. The
/// single-type-arg check keeps a forged call (wrong generics) from sneaking
/// through this skip; it would then fail the closed-target-set check instead.
fn is_benign_coin_primitive(call: &ProgrammableMoveCall) -> bool {
    call.package == framework()
        && call.module.as_str() == "coin"
        && matches!(call.function.as_str(), "zero" | "destroy_zero")
        && call.type_arguments.len() == 1
}

/// Build the sponsored-PTB templates for the protocol frontend.
///
/// Mirrors the builders in `frontend/src/tx/{composer,dashboard,faucet,deepbook}.ts`.
/// `protocol` is the `options_core` package id; `vault_pkg` is the
/// `options_vault` package id the deprecated covered-call vault flows target
/// (SO-332) — `None` on any deployment that no longer publishes it, which
/// simply drops the `vault:*` templates.
/// `test_tokens` is the `(package, module)` of each faucet token (e.g.
/// `(0xpkg, "tbtc")`), only used when `allow_faucet` is set (dev/staging).
/// `exchange` is the hybrid-exchange family (SO-416): the settlement
/// package plus, when deployed, the exchange-adapter (direct-escrow vault
/// maker legs) and the exchange-listing leaf (permissionless option-market
/// listing); `None` drops every exchange template. `cctp` is Circle's
/// TokenMessengerMinter package — `None` where the bridge isn't configured —
/// mirroring frontend tx/bridge.ts.
/// The hybrid-exchange package family (SO-416): the settlement package
/// plus the optional adapter/listing leaves its sponsored PTBs may target.
#[derive(Debug, Clone, Copy)]
pub struct ExchangeSponsorPkgs {
    /// `exchange` settlement package (fills, escrow, cancels).
    pub exchange: ObjectID,
    /// exchange-adapter package (SO-372) — direct-escrow vault-maker fill
    /// legs. `None` drops those legs from the route-fill allowlist.
    pub adapter: Option<ObjectID>,
    /// exchange-listing package (SO-415) — permissionless option-market
    /// listing. `None` drops the list-market templates.
    pub listing: Option<ObjectID>,
}

/// The trading-vault package family (SO-282). All-or-nothing per deploy;
/// `None` disables its templates.
#[derive(Debug, Clone, Copy)]
pub struct TradingVaultPkgs {
    pub trading_vault: ObjectID,
    pub oracle_pyth: ObjectID,
    pub deepbook_adapter: Option<ObjectID>,
    pub options_adapter: Option<ObjectID>,
    /// exchange-adapter package (SO-370): vaults with hybrid-exchange
    /// custodies carry its `begin/value/finalize_custody_appraisal` legs
    /// in their deposit appraisals, same shape as the DeepBook adapter's.
    pub exchange_adapter: Option<ObjectID>,
    /// equity-oracle package (SO-299): deposits on external-configured
    /// vaults carry an extra `equity_oracle::record` appraisal leg.
    pub equity_oracle: Option<ObjectID>,
    /// Pyth + Wormhole (latest upgraded) package ids: enables the Pyth
    /// price-update prefix legs on attestation-bearing deposits. `None`
    /// leaves those deposits unsponsorable.
    pub pyth: Option<PythPkgs>,
    /// Switchboard adapter + `on_demand` package ids (SO-335).
    ///
    /// Registered ALONGSIDE Pyth, never instead of it: the template set
    /// is a static allowlist evaluated per PTB, so covering both
    /// providers costs nothing and means a provider switch needs no
    /// gas-station redeploy. `None` only where the adapter is not
    /// deployed.
    pub switchboard: Option<SwitchboardPkgs>,
}

/// Our Switchboard adapter plus Switchboard's own package.
#[derive(Debug, Clone, Copy)]
pub struct SwitchboardPkgs {
    /// `oracle_switchboard` (ours) — exposes `attest`.
    pub adapter: ObjectID,
    /// Switchboard's `on_demand` package — exposes the quote-submit
    /// action that produces the in-PTB `Quotes` bundle.
    pub switchboard: ObjectID,
}

/// The on-chain Pyth deployment the price-update prefix calls target.
#[derive(Debug, Clone, Copy)]
pub struct PythPkgs {
    pub pyth: ObjectID,
    pub wormhole: ObjectID,
}

/// `run_1` … `run_6` — every arity Switchboard's quote-submit action
/// exposes. All are allowlisted because the oracle count is a runtime
/// property of the bundle Crossbar returns.
const SWITCHBOARD_MAX_ORACLES: usize = 6;

pub fn protocol_templates(
    protocol: ObjectID,
    vault_pkg: Option<ObjectID>,
    test_tokens: &[(ObjectID, String)],
    allow_faucet: bool,
    exchange: Option<ExchangeSponsorPkgs>,
    cctp: Option<ObjectID>,
    trading_vault: Option<TradingVaultPkgs>,
) -> Vec<PtbTemplate> {
    let t = |module: &str, function: &str| MoveTarget::new(protocol, module, function);

    // write / buy are now distinguished by their request/execute pair (the
    // FlowKind markers are gone): new_quote → new_signed_quote →
    // request_{writer,trader}_flow → <wildcard release> →
    // execute_{writer,trader}_flow. The executor's `coin::zero` /
    // `coinWithBalance` plumbing is skipped as benign (see
    // `is_benign_coin_primitive`), so it need not be pinned here. `module` is
    // `bucket` for covered calls / `put_bucket` for cash-secured puts.
    //
    // The single `AnyRelease` slot sponsors the MM-specified collateral
    // release — a call into an unreviewed package. Risk posture (plan §6):
    // that call receives only the CollateralRequest reference and the MM's
    // own collateral object, so it cannot touch sponsor or executor assets;
    // a pathological `release` that burns compute is bounded by the existing
    // `max_gas_budget_mist` cap. The sponsor risks gas alone, as today.
    // Any-strike creation legs (SO-394): a write/buy PTB may PREPEND
    // `create_*_any_strike` (12 type args: U, S + 10 byte markers) and
    // APPEND the mandatory `share_*` — creating the bucket atomically with
    // the first write at a fresh strike. Allowed, never required, so plain
    // writes against existing buckets still match the same template. The
    // creation entry is permissionless + ingress-gated on-chain and mints
    // nothing to anyone; the sponsor's exposure stays gas-only.
    let create_legs = |module: &str| {
        let (create_fn, share_fn) = if module == "put_bucket" {
            ("create_put_bucket_any_strike", "share_put_bucket")
        } else {
            ("create_bucket_any_strike", "share_bucket")
        };
        (t(module, create_fn), t(module, share_fn))
    };

    let execute_flow = |name: &str, request_fn: &str, execute_fn: &str, module: &str| {
        let request = t(module, request_fn);
        let execute = t(module, execute_fn);
        let (create, share) = create_legs(module);
        let matchers = vec![
            TargetMatcher::Exact(t("quote", "new_quote")),
            TargetMatcher::Exact(t("quote", "new_signed_quote")),
            TargetMatcher::Exact(request.clone()),
            TargetMatcher::AnyRelease,
            TargetMatcher::Exact(execute.clone()),
        ];
        let mut allowed = matchers.clone();
        allowed.push(TargetMatcher::Exact(create.clone()));
        allowed.push(TargetMatcher::Exact(share.clone()));
        PtbTemplate {
            name: name.to_owned(),
            required: matchers,
            allowed,
            // `new_quote<U, S>` carries the pair as type args (SO-408), so its
            // arity is pinned too — an unpinned call would let a sponsored PTB
            // name any pair in the signed payload.
            arities: vec![
                (t("quote", "new_quote"), 2),
                (request, 3),
                (execute, 3),
                (create, 12),
                (share, 3),
            ],
        }
    };

    // Standalone bucket creation (create → share, nothing else): the
    // two-step frontend flow creates first, then quotes against the derived
    // bucket id.
    let create_only = |name: &str, module: &str| {
        let (create, share) = create_legs(module);
        PtbTemplate::exact_only(
            name.to_owned(),
            vec![create.clone(), share.clone()],
            vec![create.clone(), share.clone()],
            vec![(create, 12), (share, 3)],
        )
    };

    // Single-anchor wallet flow (exercise / redeem) for either option module.
    let single_call = |name: &str, module: &str, function: &str| {
        let target = t(module, function);
        PtbTemplate::exact_only(
            name.to_owned(),
            vec![target.clone()],
            vec![target.clone()],
            vec![(target, 3)],
        )
    };

    let mut templates = vec![
        execute_flow("write", "request_writer_flow", "execute_writer_flow", "bucket"),
        execute_flow("buy", "request_trader_flow", "execute_trader_flow", "bucket"),
        single_call("exercise", "bucket", "exercise"),
        single_call("redeem", "bucket", "redeem_position"),
        // Cash-secured put wallet flows (put_bucket.move). Same PTB shapes as
        // their call twins above; mirrors frontend tx/composer_put.ts and
        // tx/dashboard_put.ts.
        execute_flow("put_write", "request_writer_flow", "execute_writer_flow", "put_bucket"),
        execute_flow("put_buy", "request_trader_flow", "execute_trader_flow", "put_bucket"),
        single_call("put_exercise", "put_bucket", "exercise"),
        single_call("put_redeem", "put_bucket", "redeem_position"),
        create_only("create_bucket", "bucket"),
        create_only("put_create_bucket", "put_bucket"),
    ];

    // Wallet-facing covered-call vault flows (doc 03). Each is a single call
    // with the vault's 3 type args; deposit/initiate_withdraw ride a
    // `coinWithBalance` prelude. Every asset moved is the user's own (their
    // own coins in, receipts/shares/refunds back to them), so the sponsor only
    // risks gas — same posture as the `write`/`buy`/`exercise` wallet flows.
    //
    // Deprecated (SO-332): kept so a deployment still carrying the package can
    // sponsor exits, but skipped entirely once options_vault is gone.
    if let Some(vault_pkg) = vault_pkg {
        for function in [
            "deposit",
            "claim_shares",
            "initiate_withdraw",
            "complete_withdraw",
            "instant_withdraw_pending",
        ] {
            let target = MoveTarget::new(vault_pkg, "vault", function);
            templates.push(PtbTemplate::exact_only(format!("vault:{function}"), vec![target.clone()], vec![target.clone()], vec![(target, 3)]));
        }
    }

    // CCTP bridge burn (frontend tx/bridge.ts): a single call straight into
    // Circle's `deposit_for_burn` entry fun, which burns the user's USDC and
    // sends the cross-chain message. The coin comes from a coinWithBalance
    // prelude. Only the user's own USDC moves, so the sponsor risks gas only.
    if let Some(token_messenger_minter) = cctp {
        let burn = MoveTarget::new(token_messenger_minter, "deposit_for_burn", "deposit_for_burn");
        templates.push(PtbTemplate::exact_only(
            "cctp_bridge".to_owned(),
            vec![burn.clone()],
            vec![burn.clone()],
            vec![(burn, 1)],
        ));
    }

    if allow_faucet {
        for (pkg, module) in test_tokens {
            let mint = MoveTarget::new(*pkg, module, "mint_to_sender");
            templates.push(PtbTemplate::exact_only(format!("faucet_mint:{module}"), vec![mint.clone()], vec![mint.clone()], vec![(mint, 0)]));
        }
    }

    // Hybrid-exchange PTB shapes (SO-416): the options secondary market.
    // Mirrors frontend `tx/{exchangeFill,exchangeEscrow,exchangeListing}.ts`.
    // Coin funding rides in via `coinWithBalance` (benign SplitCoins /
    // MergeCoins / TransferObjects plumbing); every asset moved is the
    // user's own — the sponsor only ever risks gas. Limit orders and
    // cancels are personal-message signatures POSTed to the orderbook, not
    // transactions, so they need no template.
    if let Some(ex) = exchange {
        let e = |module: &str, function: &str| MoveTarget::new(ex.exchange, module, function);

        // Taker route fill: N ≥ 1 fill legs (settlement, or the adapter's
        // direct-escrow vault-maker variants) closed by ONE strict
        // `assert_coin_min` slippage guard. The guard anchors the template;
        // the closed `allowed` set keeps escrow withdraws or anything else
        // from riding along with a fill.
        let fill = e("settlement", "fill_limit_order");
        let fill_rev = e("settlement", "fill_limit_order_reverse");
        let assert_min = e("settlement", "assert_coin_min");
        let mut fill_allowed = vec![fill.clone(), fill_rev.clone(), assert_min.clone()];
        let mut fill_arities = vec![
            (fill.clone(), 2),
            (fill_rev.clone(), 2),
            (assert_min.clone(), 1),
        ];
        if let Some(adapter) = ex.adapter {
            let va = |function: &str| MoveTarget::new(adapter, "exchange_adapter", function);
            for f in ["fill_vault_order", "fill_vault_order_reverse"] {
                fill_allowed.push(va(f));
                fill_arities.push((va(f), 2));
            }
        }
        templates.push(PtbTemplate::exact_only(
            "exchange_route_fill".to_owned(),
            vec![assert_min.clone()],
            fill_allowed,
            fill_arities,
        ));

        // Maker escrow lifecycle: `new` shares the BalanceManager itself
        // (no share command follows); deposit takes the ingress whitelist.
        let bm_new = e("balance_manager", "new");
        templates.push(PtbTemplate::exact_only(
            "exchange_enable_escrow".to_owned(),
            vec![bm_new.clone()],
            vec![bm_new.clone()],
            vec![(bm_new, 0)],
        ));
        let deposit = e("balance_manager", "deposit");
        templates.push(PtbTemplate::exact_only(
            "exchange_escrow_deposit".to_owned(),
            vec![deposit.clone()],
            vec![deposit.clone()],
            vec![(deposit, 1)],
        ));
        let withdraw = e("balance_manager", "withdraw");
        templates.push(PtbTemplate::exact_only(
            "exchange_escrow_withdraw".to_owned(),
            vec![withdraw.clone()],
            vec![withdraw.clone()],
            vec![(withdraw, 1)],
        ));

        // On-chain salt-watermark cancel (dead-man switch for makers whose
        // soft cancels the relayer might ignore).
        let cancel_up_to = e("settlement", "cancel_up_to");
        templates.push(PtbTemplate::exact_only(
            "exchange_cancel_up_to".to_owned(),
            vec![cancel_up_to.clone()],
            vec![cancel_up_to.clone()],
            vec![(cancel_up_to, 2)],
        ));

        // Permissionless option-market listing (SO-415): 12 type args — the
        // option coin's own instantiation (U, S + 10 byte markers). The
        // entry is permissionless, dedup'd and expiry-gated on-chain, and
        // mints nothing to anyone; the sponsor's exposure stays gas-only.
        if let Some(listing) = ex.listing {
            for (name, function) in [
                ("exchange_list_call_market", "create_call_market"),
                ("exchange_list_put_market", "create_put_market"),
            ] {
                let target = MoveTarget::new(listing, "exchange_listing", function);
                templates.push(PtbTemplate::exact_only(
                    name.to_owned(),
                    vec![target.clone()],
                    vec![target.clone()],
                    vec![(target, 12)],
                ));
            }
        }
    }


    // Curated trading vaults (SO-282): wallet-facing flows. Deposits ride
    // an appraisal whose legs vary with vault holdings — the template
    // anchors begin_appraisal → deposit and allows the oracle/adapter
    // appraisal calls in between. Withdrawal requests and closed-stake
    // distribution are single anchored calls. Curator/session ops are
    // NOT sponsored (curators run bots with their own gas).
    if let Some(tvp) = trading_vault {
        let tvt = |module: &str, function: &str| MoveTarget::new(tvp.trading_vault, module, function);
        let begin = tvt("vault", "begin_appraisal");
        let deposit = tvt("vault", "deposit");
        let mut appraisal_allowed = vec![
            TargetMatcher::Exact(begin.clone()),
            TargetMatcher::Exact(tvt("vault", "appraise_balance")),
            TargetMatcher::Exact(tvt("vault", "record_position_value")),
            TargetMatcher::Exact(tvt("vault_mm", "appraise_call_position")),
            TargetMatcher::Exact(tvt("vault_mm", "appraise_put_position")),
            TargetMatcher::Exact(tvt("vault_mm", "appraise_call_coin")),
            TargetMatcher::Exact(MoveTarget::new(tvp.oracle_pyth, "oracle_pyth", "attest")),
        ];
        if let Some(dba) = tvp.deepbook_adapter {
            for f in ["begin_custody_appraisal", "value_asset", "value_pool_locked", "finalize_custody_appraisal"] {
                appraisal_allowed.push(TargetMatcher::Exact(MoveTarget::new(dba, "deepbook_adapter", f)));
            }
        }
        if let Some(xa) = tvp.exchange_adapter {
            for f in ["begin_custody_appraisal", "value_asset", "finalize_custody_appraisal"] {
                appraisal_allowed.push(TargetMatcher::Exact(MoveTarget::new(xa, "exchange_adapter", f)));
            }
        }
        if let Some(oa) = tvp.options_adapter {
            for f in ["appraise_rfq_ticket", "appraise_call_position", "appraise_put_position"] {
                appraisal_allowed.push(TargetMatcher::Exact(MoveTarget::new(oa, "options_adapter", f)));
            }
        }
        // External-account vaults (SO-299) carry a mandatory
        // `equity_oracle::record` leg between begin_appraisal and the
        // consumer — allowed (not required: most vaults have no external
        // account), anchors unchanged.
        let equity_record = tvp
            .equity_oracle
            .map(|eo| MoveTarget::new(eo, "equity_oracle", "record"));
        if let Some(rec) = &equity_record {
            appraisal_allowed.push(TargetMatcher::Exact(rec.clone()));
        }
        // Attestation-bearing deposits prepend the Pyth price-update
        // legs (wormhole verify → authenticated infos → per-feed update
        // → potato destroy) and wrap attestations in `0x1::option`
        // calls. All value-neutral: the sponsor risks gas plus the
        // 1-MIST-per-feed update fee split from it.
        // Switchboard's equivalents, allowlisted at the same time
        // (SO-335). Its prefix is a single `run_N` producing the quote
        // bundle every `attest` reads from — no shared-object refresh and
        // no update fee, so the sponsor's exposure is strictly smaller
        // than the Pyth path's.
        if let Some(sb) = tvp.switchboard {
            appraisal_allowed.push(TargetMatcher::Exact(MoveTarget::new(
                sb.adapter,
                "oracle_switchboard",
                "attest",
            )));
            for n in 1..=SWITCHBOARD_MAX_ORACLES {
                appraisal_allowed.push(TargetMatcher::Exact(MoveTarget::new(
                    sb.switchboard,
                    "quote_submit_action",
                    &format!("run_{n}"),
                )));
            }
        }
        let mut pyth_legs = Vec::new();
        if let Some(pp) = tvp.pyth {
            pyth_legs.push(MoveTarget::new(pp.wormhole, "vaa", "parse_and_verify"));
            for f in [
                "create_authenticated_price_infos_using_accumulator",
                "update_single_price_feed",
            ] {
                pyth_legs.push(MoveTarget::new(pp.pyth, "pyth", f));
            }
            pyth_legs.push(MoveTarget::new(pp.pyth, "hot_potato_vector", "destroy"));
        }
        let option_wraps =
            [MoveTarget::new(stdlib(), "option", "some"), MoveTarget::new(stdlib(), "option", "none")];
        for t in pyth_legs.iter().chain(option_wraps.iter()) {
            appraisal_allowed.push(TargetMatcher::Exact(t.clone()));
        }
        let mut deposit_allowed = appraisal_allowed.clone();
        deposit_allowed.push(TargetMatcher::Exact(deposit.clone()));
        let mut deposit_arities = vec![(begin.clone(), 1), (deposit.clone(), 1)];
        if let Some(rec) = equity_record {
            deposit_arities.push((rec, 0));
        }
        // Pin the prefix-leg arities: the four Pyth calls take 0 type
        // args except the potato destroy (`<PriceInfo>`); the option
        // wrappers take exactly 1.
        for t in pyth_legs {
            let arity = usize::from(t.module == "hot_potato_vector");
            deposit_arities.push((t, arity));
        }
        for t in option_wraps {
            deposit_arities.push((t, 1));
        }
        // Switchboard: `attest<Asset, Quote>` takes 2 type args like the
        // Pyth one; `run_N` takes none.
        if let Some(sb) = tvp.switchboard {
            deposit_arities.push((
                MoveTarget::new(sb.adapter, "oracle_switchboard", "attest"),
                2,
            ));
            for n in 1..=SWITCHBOARD_MAX_ORACLES {
                deposit_arities.push((
                    MoveTarget::new(sb.switchboard, "quote_submit_action", &format!("run_{n}")),
                    0,
                ));
            }
        }
        templates.push(PtbTemplate {
            name: "trading_vault:deposit".to_owned(),
            required: vec![TargetMatcher::Exact(begin.clone()), TargetMatcher::Exact(deposit.clone())],
            allowed: deposit_allowed,
            arities: deposit_arities,
        });
        let create = tvt("vault", "create_vault");
        templates.push(PtbTemplate::exact_only(
            "trading_vault:create_vault".to_owned(),
            vec![create.clone()],
            vec![create.clone()],
            vec![(create, 1)],
        ));
        // request_withdraw<P> and amend_payout_asset<P> carry the payout
        // asset as their single type argument (SO-370);
        // enqueue_closed_stake stays type-free.
        for (name, function, arity) in [
            ("trading_vault:request_withdraw", "request_withdraw", 1),
            ("trading_vault:amend_payout_asset", "amend_payout_asset", 1),
            ("trading_vault:enqueue_closed_stake", "enqueue_closed_stake", 0),
        ] {
            let target = tvt("vault", function);
            templates.push(PtbTemplate::exact_only(
                name.to_owned(),
                vec![target.clone()],
                vec![target.clone()],
                vec![(target, arity)],
            ));
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

    fn vault_pkg() -> ObjectID {
        ObjectID::from_hex_literal("0xdef").unwrap()
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

    fn exchange_pkg() -> ObjectID {
        ObjectID::from_hex_literal("0x22be4c").unwrap()
    }

    fn exchange_adapter_pkg() -> ObjectID {
        ObjectID::from_hex_literal("0xea1").unwrap()
    }

    fn listing_pkg() -> ObjectID {
        ObjectID::from_hex_literal("0x115e").unwrap()
    }

    fn exchange_pkgs() -> ExchangeSponsorPkgs {
        ExchangeSponsorPkgs {
            exchange: exchange_pkg(),
            adapter: Some(exchange_adapter_pkg()),
            listing: Some(listing_pkg()),
        }
    }

    fn cctp_tmm_pkg() -> ObjectID {
        ObjectID::from_hex_literal("0xc12c1e").unwrap()
    }


    fn templates() -> Vec<PtbTemplate> {
        protocol_templates(
            pkg(),
            Some(vault_pkg()),
            &[(pkg(), "tbtc".to_owned())],
            true,
            Some(exchange_pkgs()),
            Some(cctp_tmm_pkg()),
            None,
        )
    }

    fn target(module: &str, function: &str) -> MoveTarget {
        MoveTarget::new(pkg(), module, function)
    }

    /// The MM's release implementation lives at some arbitrary package/module.
    fn mm_release() -> MoveTarget {
        let mm_pkg = ObjectID::from_hex_literal("0xfeed").unwrap();
        MoveTarget::new(mm_pkg, "mm_collateral", "release")
    }

    /// The 5-call quote→request→release→execute shape for one flow.
    fn flow_calls(module: &str, request_fn: &str, execute_fn: &str) -> Vec<(MoveTarget, usize)> {
        vec![
            (target("quote", "new_quote"), 2),
            (target("quote", "new_signed_quote"), 0),
            (target(module, request_fn), 3),
            (mm_release(), 1),
            (target(module, execute_fn), 3),
        ]
    }

    #[test]
    fn cctp_bridge_flow_matches() {
        // Mirrors frontend tx/bridge.ts: coinWithBalance plumbing, then a
        // direct call into Circle's deposit_for_burn (1 type arg).
        let pt = build(
            &[
                (MoveTarget::new(framework(), "coin", "zero"), 1),
                (
                    MoveTarget::new(cctp_tmm_pkg(), "deposit_for_burn", "deposit_for_burn"),
                    1,
                ),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &pt), Some("cctp_bridge"));
    }

    #[test]
    fn cctp_bridge_wrong_arity_is_rejected() {
        // A forged burn call with the wrong generics must not be sponsored.
        let pt = build(
            &[(
                MoveTarget::new(cctp_tmm_pkg(), "deposit_for_burn", "deposit_for_burn"),
                3,
            )],
            false,
        );
        assert_eq!(match_any(&templates(), &pt), None);
    }

    #[test]
    fn write_flow_matches() {
        let pt = build(
            &flow_calls("bucket", "request_writer_flow", "execute_writer_flow"),
            true,
        );
        assert_eq!(match_any(&templates(), &pt), Some("write"));
    }

    #[test]
    fn buy_flow_matches() {
        let pt = build(
            &flow_calls("bucket", "request_trader_flow", "execute_trader_flow"),
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
    fn put_wallet_flows_match() {
        // Same request → release → execute shape as calls, anchored on the
        // put_bucket module.
        let write = build(
            &flow_calls("put_bucket", "request_writer_flow", "execute_writer_flow"),
            true,
        );
        assert_eq!(match_any(&templates(), &write), Some("put_write"));
        let buy = build(
            &flow_calls("put_bucket", "request_trader_flow", "execute_trader_flow"),
            true,
        );
        assert_eq!(match_any(&templates(), &buy), Some("put_buy"));

        let ex = build(&[(target("put_bucket", "exercise"), 3)], true);
        assert_eq!(match_any(&templates(), &ex), Some("put_exercise"));
        let rd = build(&[(target("put_bucket", "redeem_position"), 3)], false);
        assert_eq!(match_any(&templates(), &rd), Some("put_redeem"));
    }

    #[test]
    fn create_and_write_flow_matches() {
        // Any-strike genesis write: create (12 targs) prepended, share (3)
        // appended around the standard write shape — still the `write`
        // template (creation legs are allowed, not required).
        let mut calls = vec![(target("bucket", "create_bucket_any_strike"), 12)];
        calls.extend(flow_calls("bucket", "request_writer_flow", "execute_writer_flow"));
        calls.push((target("bucket", "share_bucket"), 3));
        let pt = build(&calls, true);
        assert_eq!(match_any(&templates(), &pt), Some("write"));
    }

    #[test]
    fn create_only_flow_matches() {
        let pt = build(
            &[
                (target("bucket", "create_bucket_any_strike"), 12),
                (target("bucket", "share_bucket"), 3),
            ],
            true,
        );
        assert_eq!(match_any(&templates(), &pt), Some("create_bucket"));
        let put = build(
            &[
                (target("put_bucket", "create_put_bucket_any_strike"), 12),
                (target("put_bucket", "share_put_bucket"), 3),
            ],
            true,
        );
        assert_eq!(match_any(&templates(), &put), Some("put_create_bucket"));
    }

    #[test]
    fn create_with_wrong_arity_rejected() {
        // A creation leg with the wrong type-arg count fails the pinned
        // arity check in every template.
        let pt = build(
            &[
                (target("bucket", "create_bucket_any_strike"), 3),
                (target("bucket", "share_bucket"), 3),
            ],
            true,
        );
        assert_eq!(match_any(&templates(), &pt), None);
    }

    #[test]
    fn two_wildcard_release_calls_rejected() {
        // A second release-shaped call cannot ride along — even though each
        // one individually satisfies the AnyRelease matcher.
        let mut calls = flow_calls("bucket", "request_writer_flow", "execute_writer_flow");
        let evil = MoveTarget::new(ObjectID::from_hex_literal("0xdead").unwrap(), "x", "release");
        calls.insert(4, (evil, 1));
        let pt = build(&calls, false);
        assert_eq!(match_any(&templates(), &pt), None);
    }

    #[test]
    fn wildcard_wrong_function_name_or_arity_rejected() {
        // Wrong function name in the release slot.
        let mut calls = flow_calls("bucket", "request_writer_flow", "execute_writer_flow");
        calls[3] = (
            MoveTarget::new(ObjectID::from_hex_literal("0xfeed").unwrap(), "mm_collateral", "steal"),
            1,
        );
        assert_eq!(match_any(&templates(), &build(&calls, false)), None);

        // Right name, wrong type-arg arity (2 instead of 1).
        let mut calls = flow_calls("bucket", "request_writer_flow", "execute_writer_flow");
        calls[3] = (mm_release(), 2);
        assert_eq!(match_any(&templates(), &build(&calls, false)), None);
    }

    #[test]
    fn wildcard_cannot_substitute_for_a_pinned_call() {
        // Drop the pinned request_writer_flow and put an extra foreign
        // `release` in its place: the wildcard slot must not stand in for the
        // pinned call (and two release calls are refused anyway).
        let pt = build(
            &[
                (target("quote", "new_quote"), 2),
                (target("quote", "new_signed_quote"), 0),
                (mm_release(), 1),
                (target("bucket", "execute_writer_flow"), 3),
            ],
            false,
        );
        // Missing required request_writer_flow → no match.
        assert_eq!(match_any(&templates(), &pt), None);

        // A protocol call named like a wildcard (hypothetical
        // `bucket::release<1>`) cannot satisfy the AnyRelease slot either:
        // exact matchers win first, so the required wildcard slot stays
        // unsatisfied. (There is no pinned `bucket::release` in any template,
        // so this call fails the closed allowed set outright.)
        let mut calls = flow_calls("bucket", "request_writer_flow", "execute_writer_flow");
        calls[3] = (target("bucket", "release"), 1);
        let pt = build(&calls, false);
        // `bucket::release` is not a pinned target; it matches AnyRelease and
        // the template still matches — the wildcard is package-agnostic by
        // design. What must NOT happen is a pinned target doubling as the
        // wildcard: replace the release call with a second execute call.
        assert_eq!(match_any(&templates(), &pt), Some("write"));
        let mut calls = flow_calls("bucket", "request_writer_flow", "execute_writer_flow");
        calls[3] = (target("bucket", "execute_writer_flow"), 3);
        let pt = build(&calls, false);
        assert_eq!(
            match_any(&templates(), &pt),
            None,
            "a pinned call must not satisfy the AnyRelease slot"
        );
    }

    #[test]
    fn faucet_mint_matches() {
        let pt = build(&[(target("tbtc", "mint_to_sender"), 0)], false);
        assert_eq!(match_any(&templates(), &pt), Some("faucet_mint:tbtc"));
    }

    #[test]
    fn faucet_rejected_when_disabled() {
        let no_faucet =
            protocol_templates(pkg(), Some(vault_pkg()), &[(pkg(), "tbtc".to_owned())], false, None, None, None);
        let pt = build(&[(target("tbtc", "mint_to_sender"), 0)], false);
        assert_eq!(match_any(&no_faucet, &pt), None);
    }

    #[test]
    fn exchange_route_fill_matches_and_rejects_riders() {
        let e = |module: &str, function: &str| MoveTarget::new(exchange_pkg(), module, function);
        // Single settlement leg + slippage guard, coin prep prelude.
        let single = build(
            &[(e("settlement", "fill_limit_order"), 2), (e("settlement", "assert_coin_min"), 1)],
            true,
        );
        assert_eq!(match_any(&templates(), &single), Some("exchange_route_fill"));
        // Multi-leg with a direct-escrow vault-maker leg.
        let multi = build(
            &[
                (e("settlement", "fill_limit_order_reverse"), 2),
                (MoveTarget::new(exchange_adapter_pkg(), "exchange_adapter", "fill_vault_order"), 2),
                (e("settlement", "assert_coin_min"), 1),
            ],
            true,
        );
        assert_eq!(match_any(&templates(), &multi), Some("exchange_route_fill"));
        // No slippage guard => no match (the anchor is required).
        let unguarded = build(&[(e("settlement", "fill_limit_order"), 2)], true);
        assert_eq!(match_any(&templates(), &unguarded), None);
        // An escrow withdraw can never ride along with a fill.
        let rider = build(
            &[
                (e("settlement", "fill_limit_order"), 2),
                (e("settlement", "assert_coin_min"), 1),
                (e("balance_manager", "withdraw"), 1),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &rider), None);
    }

    #[test]
    fn exchange_escrow_templates_match_their_frontend_shapes() {
        let e = |module: &str, function: &str| MoveTarget::new(exchange_pkg(), module, function);
        let create = build(&[(e("balance_manager", "new"), 0)], false);
        assert_eq!(match_any(&templates(), &create), Some("exchange_enable_escrow"));
        let deposit = build(&[(e("balance_manager", "deposit"), 1)], true);
        assert_eq!(match_any(&templates(), &deposit), Some("exchange_escrow_deposit"));
        let withdraw = build(&[(e("balance_manager", "withdraw"), 1)], false);
        assert_eq!(match_any(&templates(), &withdraw), Some("exchange_escrow_withdraw"));
        let cancel = build(&[(e("settlement", "cancel_up_to"), 2)], false);
        assert_eq!(match_any(&templates(), &cancel), Some("exchange_cancel_up_to"));
        // Deposit arity is pinned: a 2-type-arg deposit is refused.
        let bad = build(&[(e("balance_manager", "deposit"), 2)], false);
        assert_eq!(match_any(&templates(), &bad), None);
    }

    #[test]
    fn exchange_list_market_matches_arity_and_unconfigured() {
        let call = MoveTarget::new(listing_pkg(), "exchange_listing", "create_call_market");
        let put = MoveTarget::new(listing_pkg(), "exchange_listing", "create_put_market");
        let list_call = build(&[(call.clone(), 12)], false);
        assert_eq!(match_any(&templates(), &list_call), Some("exchange_list_call_market"));
        let list_put = build(&[(put, 12)], false);
        assert_eq!(match_any(&templates(), &list_put), Some("exchange_list_put_market"));
        // 12 type args or nothing — an under-instantiated call is refused.
        let bad_arity = build(&[(call.clone(), 2)], false);
        assert_eq!(match_any(&templates(), &bad_arity), None);
        // No exchange configured => nothing exchange-shaped sponsors.
        let none = protocol_templates(pkg(), Some(vault_pkg()), &[], false, None, None, None);
        assert_eq!(match_any(&none, &list_call), None);
        let fill = build(
            &[
                (MoveTarget::new(exchange_pkg(), "settlement", "fill_limit_order"), 2),
                (MoveTarget::new(exchange_pkg(), "settlement", "assert_coin_min"), 1),
            ],
            true,
        );
        assert_eq!(match_any(&none, &fill), None);
    }

    #[test]
    fn foreign_call_injection_rejected() {
        let evil = ObjectID::from_hex_literal("0xdead").unwrap();
        let mut calls = flow_calls("bucket", "request_writer_flow", "execute_writer_flow");
        calls.insert(4, (MoveTarget::new(evil, "drain", "all"), 0));
        let pt = build(&calls, false);
        assert_eq!(match_any(&templates(), &pt), None);
    }

    #[test]
    fn framework_call_other_than_coin_zero_rejected() {
        // Proves we whitelist `0x2::coin::zero`, not all of `0x2`.
        let mut calls = flow_calls("bucket", "request_writer_flow", "execute_writer_flow");
        calls.insert(
            4,
            (MoveTarget::new(framework(), "transfer", "public_transfer"), 1),
        );
        let pt = build(&calls, false);
        assert_eq!(match_any(&templates(), &pt), None);
    }

    #[test]
    fn wrong_type_arg_arity_rejected() {
        let pt = build(&[(target("bucket", "exercise"), 2)], false);
        assert_eq!(match_any(&templates(), &pt), None);
    }

    #[test]
    fn exercise_with_coin_with_balance_cleanup_matches() {
        // The exact shape prod refused: the `coinWithBalance` resolver wraps
        // `bucket::exercise` in a split/merge prelude and a trailing
        // `0x2::coin::destroy_zero<1>` cleanup.
        // [SplitCoins; MergeCoins; SplitCoins; bucket::exercise<3>;
        //  TransferObjects; 0x2::coin::destroy_zero<1>]
        use sui_types::transaction::Argument;
        let mut b = ProgrammableTransactionBuilder::new();
        let amt = b.pure(1u64).unwrap();
        b.command(Command::SplitCoins(Argument::GasCoin, vec![amt]));
        b.command(Command::MergeCoins(Argument::GasCoin, vec![Argument::GasCoin]));
        let amt2 = b.pure(1u64).unwrap();
        b.command(Command::SplitCoins(Argument::GasCoin, vec![amt2]));
        b.programmable_move_call(
            pkg(),
            Identifier::new("bucket").unwrap(),
            Identifier::new("exercise").unwrap(),
            vec![TypeTag::U64, TypeTag::U64, TypeTag::U64],
            vec![],
        );
        b.command(Command::TransferObjects(
            vec![Argument::GasCoin],
            Argument::GasCoin,
        ));
        b.programmable_move_call(
            framework(),
            Identifier::new("coin").unwrap(),
            Identifier::new("destroy_zero").unwrap(),
            vec![TypeTag::U64],
            vec![],
        );
        let pt = b.finish();
        assert_eq!(match_any(&templates(), &pt), Some("exercise"));
    }

    #[test]
    fn benign_coin_primitives_skipped_across_templates() {
        let coin = |function: &str| MoveTarget::new(framework(), "coin", function);
        let e = |module: &str, function: &str| MoveTarget::new(exchange_pkg(), module, function);

        // `destroy_zero<1>` trailing an exchange route fill — a different
        // template than `exercise`, proving the skip is universal, not
        // exercise-only.
        let fill = build(
            &[
                (e("settlement", "fill_limit_order"), 2),
                (e("settlement", "assert_coin_min"), 1),
                (coin("destroy_zero"), 1),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &fill), Some("exchange_route_fill"));

        // `coin::zero<1>` is now skipped everywhere, not just in write/buy: a
        // vault deposit carrying it still matches. Vault flows target the
        // options_vault package, not core.
        let vault_deposit = build(
            &[
                (MoveTarget::new(vault_pkg(), "vault", "deposit"), 3),
                (coin("zero"), 1),
            ],
            false,
        );
        assert_eq!(match_any(&templates(), &vault_deposit), Some("vault:deposit"));
        // The same call against the core package no longer matches.
        let wrong_pkg = build(&[(target("vault", "deposit"), 3)], false);
        assert_eq!(match_any(&templates(), &wrong_pkg), None);
    }

    /// SO-332: with the covered-call vault package undeployed, none of the
    /// `vault:*` templates are registered and its PTBs stop being sponsorable.
    /// The rest of the template set is untouched.
    #[test]
    fn deprecated_vault_templates_absent_without_package() {
        let without =
            protocol_templates(pkg(), None, &[(pkg(), "tbtc".to_owned())], true, Some(exchange_pkgs()), Some(cctp_tmm_pkg()), None);
        assert!(
            !without.iter().any(|t| t.name.starts_with("vault:")),
            "vault templates must not be registered without options_vault"
        );
        let vault_deposit = build(
            &[(MoveTarget::new(vault_pkg(), "vault", "deposit"), 3)],
            false,
        );
        assert_eq!(match_any(&without, &vault_deposit), None);

        // Everything else still sponsors: only the 5 vault flows drop out.
        let with = templates();
        assert_eq!(with.len() - without.len(), 5);
        let write = build(
            &[
                (target("quote", "new_quote"), 2),
                (target("quote", "new_signed_quote"), 0),
                (target("bucket", "request_writer_flow"), 3),
                (MoveTarget::new(ObjectID::random(), "mm", "release"), 1),
                (target("bucket", "execute_writer_flow"), 3),
            ],
            false,
        );
        assert_eq!(match_any(&without, &write), Some("write"));
    }

    #[test]
    fn forged_coin_primitive_not_skipped() {
        let coin = |function: &str| MoveTarget::new(framework(), "coin", function);
        // Wrong generics (arity != 1) are NOT treated as the benign primitive;
        // they fall through to the closed-target-set check and are refused.
        let bad_arity = build(
            &[(target("bucket", "exercise"), 3), (coin("destroy_zero"), 2)],
            false,
        );
        assert_eq!(match_any(&templates(), &bad_arity), None);
        // A non-cleanup `0x2::coin` function is not skipped either.
        let other_coin_fn = build(
            &[(target("bucket", "exercise"), 3), (coin("into_balance"), 1)],
            false,
        );
        assert_eq!(match_any(&templates(), &other_coin_fn), None);
    }

    #[test]
    fn trading_vault_deposit_with_equity_record_leg() {
        let tv_pkg = ObjectID::from_hex_literal("0x71ad").unwrap();
        let op_pkg = ObjectID::from_hex_literal("0x0217").unwrap();
        let eo_pkg = ObjectID::from_hex_literal("0xe071").unwrap();
        let with_eo = |equity_oracle: Option<ObjectID>| {
            protocol_templates(
                pkg(),
                Some(vault_pkg()),
                &[],
                false,
                None,
                None,
                Some(TradingVaultPkgs {
                    trading_vault: tv_pkg,
                    oracle_pyth: op_pkg,
                    deepbook_adapter: None,
                    options_adapter: None,
                    exchange_adapter: None,
                    equity_oracle,
                    pyth: None,
                    switchboard: None,
                }),
            )
        };
        // A deposit on an external-configured vault: attest → begin_appraisal
        // → equity_oracle::record → deposit (+ coinWithBalance prelude).
        let calls = [
            (MoveTarget::new(op_pkg, "oracle_pyth", "attest"), 2),
            (MoveTarget::new(tv_pkg, "vault", "begin_appraisal"), 1),
            (MoveTarget::new(eo_pkg, "equity_oracle", "record"), 0),
            (MoveTarget::new(tv_pkg, "vault", "deposit"), 1),
        ];
        let pt = build(&calls, true);
        assert_eq!(match_any(&with_eo(Some(eo_pkg)), &pt), Some("trading_vault:deposit"));

        // Without the equity-oracle package configured, the record leg is
        // a foreign call and the PTB is refused.
        assert_eq!(match_any(&with_eo(None), &pt), None);

        // A forged record call with type args fails the pinned arity.
        let mut forged = calls.clone();
        forged[2].1 = 1;
        assert_eq!(match_any(&with_eo(Some(eo_pkg)), &build(&forged, true)), None);

        // A plain deposit (no external account) still matches unchanged.
        let plain = build(
            &[
                (MoveTarget::new(tv_pkg, "vault", "begin_appraisal"), 1),
                (MoveTarget::new(tv_pkg, "vault", "deposit"), 1),
            ],
            true,
        );
        assert_eq!(match_any(&with_eo(Some(eo_pkg)), &plain), Some("trading_vault:deposit"));
    }

    /// SO-335: both providers' deposit shapes sponsor from ONE template
    /// set. This is what makes the oracle switch a config change rather
    /// than a gas-station redeploy — if it regresses, deposits silently
    /// stop being sponsored the moment the provider flips.
    #[test]
    fn both_providers_deposit_shapes_sponsor_simultaneously() {
        let tv_pkg = ObjectID::from_hex_literal("0x7").unwrap();
        let op_pkg = ObjectID::from_hex_literal("0x8").unwrap();
        let sb_adapter = ObjectID::from_hex_literal("0x9").unwrap();
        let sb_pkg = ObjectID::from_hex_literal("0xa").unwrap();
        let pyth_pkg = ObjectID::from_hex_literal("0x5b1f").unwrap();
        let wh_pkg = ObjectID::from_hex_literal("0xf473").unwrap();
        let tvt = |module: &str, function: &str| MoveTarget::new(tv_pkg, module, function);

        let templates = protocol_templates(
            pkg(),
            Some(vault_pkg()),
            &[],
            false,
            None,
            None,
            Some(TradingVaultPkgs {
                trading_vault: tv_pkg,
                oracle_pyth: op_pkg,
                deepbook_adapter: None,
                options_adapter: None,
                exchange_adapter: None,
                equity_oracle: None,
                pyth: Some(PythPkgs { pyth: pyth_pkg, wormhole: wh_pkg }),
                switchboard: Some(SwitchboardPkgs {
                    adapter: sb_adapter,
                    switchboard: sb_pkg,
                }),
            }),
        );

        // Pyth path: 4-call refresh prefix, then attest.
        let pyth_deposit = build(
            &[
                (MoveTarget::new(wh_pkg, "vaa", "parse_and_verify"), 0),
                (
                    MoveTarget::new(pyth_pkg, "pyth", "create_authenticated_price_infos_using_accumulator"),
                    0,
                ),
                (MoveTarget::new(pyth_pkg, "pyth", "update_single_price_feed"), 0),
                (MoveTarget::new(pyth_pkg, "hot_potato_vector", "destroy"), 1),
                (MoveTarget::new(op_pkg, "oracle_pyth", "attest"), 2),
                (tvt("vault", "begin_appraisal"), 1),
                (tvt("vault", "deposit"), 1),
            ],
            false,
        );
        assert_eq!(
            match_any(&templates, &pyth_deposit),
            Some("trading_vault:deposit")
        );

        // Switchboard path: ONE run_N producing the bundle, then attest.
        let sb_deposit = build(
            &[
                (MoveTarget::new(sb_pkg, "quote_submit_action", "run_3"), 0),
                (MoveTarget::new(sb_adapter, "oracle_switchboard", "attest"), 2),
                (tvt("vault", "begin_appraisal"), 1),
                (tvt("vault", "deposit"), 1),
            ],
            false,
        );
        assert_eq!(
            match_any(&templates, &sb_deposit),
            Some("trading_vault:deposit"),
            "switchboard deposits must sponsor from the same template set"
        );

        // A forged arity on the switchboard attest is still refused —
        // covering both providers must not loosen either.
        let forged = build(
            &[
                (MoveTarget::new(sb_pkg, "quote_submit_action", "run_3"), 0),
                (MoveTarget::new(sb_adapter, "oracle_switchboard", "attest"), 3),
                (tvt("vault", "begin_appraisal"), 1),
                (tvt("vault", "deposit"), 1),
            ],
            false,
        );
        assert_eq!(match_any(&templates, &forged), None);
    }

    #[test]
    fn trading_vault_deposit_with_pyth_prefix() {
        let tv_pkg = ObjectID::from_hex_literal("0x71ad").unwrap();
        let op_pkg = ObjectID::from_hex_literal("0x0217").unwrap();
        let pyth_pkg = ObjectID::from_hex_literal("0xabf8").unwrap();
        let wh_pkg = ObjectID::from_hex_literal("0xf473").unwrap();
        let with_pyth = |pyth: Option<PythPkgs>| {
            protocol_templates(
                pkg(),
                Some(vault_pkg()),
                &[],
                false,
                None,
                None,
                Some(TradingVaultPkgs {
                    trading_vault: tv_pkg,
                    oracle_pyth: op_pkg,
                    deepbook_adapter: None,
                    options_adapter: None,
                    exchange_adapter: None,
                    equity_oracle: None,
                    pyth,
                    switchboard: None,
                }),
            )
        };
        let handles = PythPkgs { pyth: pyth_pkg, wormhole: wh_pkg };
        // Attestation-bearing deposit: pyth update prefix → attest →
        // begin_appraisal → option-wrapped appraisal legs → deposit.
        let calls = [
            (MoveTarget::new(wh_pkg, "vaa", "parse_and_verify"), 0),
            (
                MoveTarget::new(pyth_pkg, "pyth", "create_authenticated_price_infos_using_accumulator"),
                0,
            ),
            (MoveTarget::new(pyth_pkg, "pyth", "update_single_price_feed"), 0),
            (MoveTarget::new(pyth_pkg, "pyth", "update_single_price_feed"), 0),
            (MoveTarget::new(pyth_pkg, "hot_potato_vector", "destroy"), 1),
            (MoveTarget::new(op_pkg, "oracle_pyth", "attest"), 2),
            (MoveTarget::new(tv_pkg, "vault", "begin_appraisal"), 1),
            (MoveTarget::new(stdlib(), "option", "some"), 1),
            (MoveTarget::new(stdlib(), "option", "none"), 1),
            (MoveTarget::new(tv_pkg, "vault", "appraise_balance"), 2),
            (MoveTarget::new(tv_pkg, "vault", "deposit"), 1),
        ];
        let pt = build(&calls, true);
        assert_eq!(match_any(&with_pyth(Some(handles)), &pt), Some("trading_vault:deposit"));

        // Without the Pyth packages configured the prefix legs are
        // foreign calls and the PTB is refused.
        assert_eq!(match_any(&with_pyth(None), &pt), None);

        // A forged potato destroy with the wrong type arity is refused.
        let mut forged = calls.clone();
        forged[4].1 = 2;
        assert_eq!(match_any(&with_pyth(Some(handles)), &build(&forged, true)), None);
    }

    /// SO-370 shapes: request_withdraw/amend carry the payout type arg,
    /// non-accounting deposits wrap their attestation in `option::some`,
    /// and exchange-custody appraisal legs sponsor inside a deposit.
    #[test]
    fn trading_vault_so370_shapes() {
        let tv_pkg = ObjectID::from_hex_literal("0x71ad").unwrap();
        let op_pkg = ObjectID::from_hex_literal("0x0217").unwrap();
        let xa_pkg = ObjectID::from_hex_literal("0xe8c1").unwrap();
        let templates = protocol_templates(
            pkg(),
            None,
            &[],
            false,
            None,
            None,
            Some(TradingVaultPkgs {
                trading_vault: tv_pkg,
                oracle_pyth: op_pkg,
                deepbook_adapter: None,
                options_adapter: None,
                exchange_adapter: Some(xa_pkg),
                equity_oracle: None,
                pyth: None,
                switchboard: None,
            }),
        );
        // request_withdraw<P> — one type arg; the pre-SO-370 type-free
        // shape is no longer sponsored.
        let rw = build(&[(MoveTarget::new(tv_pkg, "vault", "request_withdraw"), 1)], false);
        assert_eq!(match_any(&templates, &rw), Some("trading_vault:request_withdraw"));
        let rw_old = build(&[(MoveTarget::new(tv_pkg, "vault", "request_withdraw"), 0)], false);
        assert_eq!(match_any(&templates, &rw_old), None);
        // amend_payout_asset<P>: single anchored call.
        let amend = build(&[(MoveTarget::new(tv_pkg, "vault", "amend_payout_asset"), 1)], false);
        assert_eq!(match_any(&templates, &amend), Some("trading_vault:amend_payout_asset"));
        // Non-accounting deposit: attest → begin_appraisal → option::some
        // wrapping the SAME attest result → deposit.
        let dep = build(
            &[
                (MoveTarget::new(op_pkg, "oracle_pyth", "attest"), 2),
                (MoveTarget::new(tv_pkg, "vault", "begin_appraisal"), 1),
                (MoveTarget::new(stdlib(), "option", "some"), 1),
                (MoveTarget::new(tv_pkg, "vault", "deposit"), 1),
            ],
            true,
        );
        assert_eq!(match_any(&templates, &dep), Some("trading_vault:deposit"));
        // Exchange-custody appraisal legs ride inside the deposit.
        let dep_x = build(
            &[
                (MoveTarget::new(op_pkg, "oracle_pyth", "attest"), 2),
                (MoveTarget::new(tv_pkg, "vault", "begin_appraisal"), 1),
                (MoveTarget::new(xa_pkg, "exchange_adapter", "begin_custody_appraisal"), 0),
                (MoveTarget::new(xa_pkg, "exchange_adapter", "value_asset"), 1),
                (MoveTarget::new(xa_pkg, "exchange_adapter", "finalize_custody_appraisal"), 0),
                (MoveTarget::new(stdlib(), "option", "none"), 1),
                (MoveTarget::new(tv_pkg, "vault", "deposit"), 1),
            ],
            true,
        );
        assert_eq!(match_any(&templates, &dep_x), Some("trading_vault:deposit"));
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
