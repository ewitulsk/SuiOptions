/// Hub-side multichain message handling (docs/multichain-vault-plan.md
/// §3–§5): spoke binding, the ordered-lane message handlers, the
/// appraisal spoke legs, and ConfigSync emission.
///
/// Failure discipline: RELAYER-side problems (wrong attestation, stale
/// price, bad lane wiring) ABORT — the transaction reverts atomically,
/// the lane does not advance, and the relayer retries. SPOKE-side
/// conditions that a retry can never cure (paused vault, unknown asset,
/// lockup, wiped generation…) REJECT — the lane advances and the spoke
/// receives an explicit negative ack (`accepted: false` / `pay_amount:
/// 0`), so a bad request can never wedge the lane.
module vault_v2::multichain;

use std::type_name;
use sui::clock::Clock;

use options_core::admin::AdminCap;

use vault_v2::capital;
use vault_v2::endpoint::{Self, EndpointRegistry, OutboundMessage, VerifiedInbound};
use vault_v2::errors;
use vault_v2::events;
use vault_v2::fees;
use vault_v2::price::{Self, PriceAttestation};
use vault_v2::registry::{Self, VaultProtocolConfig};
use vault_v2::spoke;
use vault_v2::vault::{Self, Appraisal, CuratorCap, TradingVault};
use vault_v2::wire::{Self, Inbound};

const BPS_DENOM: u128 = 10_000;
const U64_MAX: u128 = 18_446_744_073_709_551_615;

// Reject codes (see the event-schema comment in `events.move`).
const REJECT_NONE: u8 = 0;
const REJECT_PAUSED: u8 = 1;
const REJECT_NOT_OPEN: u8 = 2;
const REJECT_UNKNOWN_ASSET: u8 = 3;
const REJECT_AMOUNT_INVALID: u8 = 4;
const REJECT_TRANCHE_INVALID: u8 = 5;
const REJECT_ACK_DEADLINE: u8 = 6;
const REJECT_RISK_STATE: u8 = 7;
const REJECT_SENIOR_BUFFER: u8 = 8;
const REJECT_ZERO_OR_DEAD: u8 = 9;
const REJECT_NO_HOLDING: u8 = 10;
const REJECT_LOCKED: u8 = 11;
const REJECT_WIPED: u8 = 12;
const REJECT_JUNIOR_BLOCKED: u8 = 13;
const REJECT_SHARES_EXCEED: u8 = 14;

fun mul_div(a: u128, b: u128, c: u128): u128 {
    (((a as u256) * (b as u256) / (c as u256)) as u128)
}

// ══════════════════════════ spoke binding ══════════════════════════

/// Bind a spoke to this vault (admin + curator co-signed, multichain
/// plan §3). `E` is the transport witness type (one active endpoint per
/// spoke); `M` is the pricing marker for the spoke's payout asset.
public fun bind_spoke<E, M>(
    _: &AdminCap,
    cap: &CuratorCap,
    vault: &mut TradingVault,
    reg: &EndpointRegistry,
    spoke_id: u64,
    chain_id: u64,
    spoke_vault: address,
    endpoint_code: u8,
    payout_asset_code: u8,
    max_sync_age_ms: u64,
    ack_deadline_ms: u64,
    curator_address: address,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    vault::mc_assert_cap(vault, cap);
    let ep = type_name::with_defining_ids<E>();
    assert!(endpoint::is_endpoint_allowed(reg, &ep), errors::endpoint_not_allowed());
    assert!(
        chain_id != 0 && chain_id != endpoint::hub_chain_id(reg),
        errors::config_invalid(),
    );
    assert!(max_sync_age_ms > 0 && ack_deadline_ms > 0, errors::config_invalid());
    let marker = type_name::with_defining_ids<M>();
    let s = spoke::new(
        chain_id,
        spoke_vault,
        ep,
        payout_asset_code,
        marker,
        max_sync_age_ms,
        ack_deadline_ms,
        clock.timestamp_ms(),
        curator_address,
        endpoint_code,
        ctx,
    );
    vault::bind_spoke_internal(vault, spoke_id, s);
    events::emit_spoke_bound(
        object::id(vault),
        spoke_id,
        chain_id,
        spoke_vault,
        ep,
        endpoint_code,
        payout_asset_code,
        marker,
        max_sync_age_ms,
        ack_deadline_ms,
    );
}

/// Register an additional spoke asset under `code`, priced via `M`.
public fun add_spoke_asset<M>(
    _: &AdminCap,
    cap: &CuratorCap,
    vault: &mut TradingVault,
    spoke_id: u64,
    code: u8,
) {
    vault::mc_assert_cap(vault, cap);
    let marker = type_name::with_defining_ids<M>();
    spoke::add_asset(vault::spoke_mut(vault, spoke_id), code, marker);
    events::emit_spoke_asset_added(object::id(vault), spoke_id, code, marker);
}

/// Update the curator's spoke-chain address (propagates on the next
/// ConfigSync). Curator-gated — rotation runs through the hub cap.
public fun set_spoke_curator(
    cap: &CuratorCap,
    vault: &mut TradingVault,
    spoke_id: u64,
    curator_address: address,
) {
    vault::mc_assert_cap(vault, cap);
    spoke::set_curator_address(vault::spoke_mut(vault, spoke_id), curator_address);
    events::emit_spoke_curator_set(object::id(vault), spoke_id, curator_address);
}

/// Commit the spoke's integration set (admin + curator co-signed, §6).
public fun set_spoke_integrations_root(
    _: &AdminCap,
    cap: &CuratorCap,
    vault: &mut TradingVault,
    spoke_id: u64,
    root: address,
) {
    vault::mc_assert_cap(vault, cap);
    spoke::set_integrations_root(vault::spoke_mut(vault, spoke_id), root);
    events::emit_spoke_integrations_root_set(object::id(vault), spoke_id, root);
}

/// Unbind a fully drained spoke (no holdings, no assets, no payables).
public fun unbind_spoke(
    _: &AdminCap,
    cap: &CuratorCap,
    vault: &mut TradingVault,
    spoke_id: u64,
) {
    vault::mc_assert_cap(vault, cap);
    let s = vault::unbind_spoke_internal(vault, spoke_id);
    assert!(spoke::is_drained(&s), errors::spoke_not_drained());
    spoke::destroy_drained(s);
    events::emit_spoke_unbound(object::id(vault), spoke_id);
}

// ══════════════════════ inbound admission ══════════════════════

/// Shared admission for every inbound message: transport identity, lane
/// wiring, and the strictly-ordered sequence — aborts on any mismatch
/// (relayer-side problems). Advances the lane.
fun admit(
    vault: &mut TradingVault,
    reg: &EndpointRegistry,
    msg: VerifiedInbound,
    expected_type: u8,
): (u64, Inbound) {
    let (ep, env, inbound) = endpoint::open(msg);
    assert!(wire::msg_type(&env) == expected_type, errors::wire_malformed());
    let spoke_id = wire::inbound_spoke_id(&inbound);
    let hub_chain = endpoint::hub_chain_id(reg);
    let vault_addr = object::id(vault).to_address();
    let s = vault::spoke_mut(vault, spoke_id);
    assert!(ep == spoke::endpoint(s), errors::wrong_endpoint());
    assert!(
        wire::src_chain_id(&env) == spoke::chain_id(s)
            && wire::src_app(&env) == spoke::vault_address(s)
            && wire::dst_chain_id(&env) == hub_chain
            && wire::dst_app(&env) == vault_addr,
        errors::wrong_lane(),
    );
    spoke::apply_inbound_seq(s, wire::seq(&env));
    (spoke_id, inbound)
}

// ══════════════════════ deposit notice (§4) ══════════════════════

/// Apply one `DepositNotice`: value the raw amount at the attested
/// marker price, mint shares into the requested tranche at current NAV
/// (entry haircut applied), credit the holdings ledger and the spoke's
/// recognized funds — or reject with an explicit code. Returns the
/// `DepositAck` for the spoke's bound transport to ship this PTB.
public fun handle_deposit_notice(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    reg: &EndpointRegistry,
    msg: VerifiedInbound,
    appraisal: Appraisal,
    att: PriceAttestation,
    clock: &Clock,
    _ctx: &mut TxContext,
): OutboundMessage {
    let (spoke_id, inbound) = admit(vault, reg, msg, wire::deposit_notice_type());
    let (_, deposit_seq, depositor, asset_code, amount_raw, tranche_code, ts_ms) =
        wire::as_deposit_notice(&inbound);

    // Capital state is synced on every applied message, accepted or not.
    let now = clock.timestamp_ms();
    let nav = vault::mc_consume_appraisal(vault, appraisal);
    let (senior_nav, junior_nav) = vault::mc_sync_capital(vault, cfg, nav, now);

    let mut reject = REJECT_NONE;
    let mut value = 0u64;
    let mut shares = 0u128;
    let mut generation = 0u64;
    let mut locked_until_ms = 0u64;
    let amount64 = if (amount_raw > U64_MAX) { 0 } else { amount_raw as u64 };

    let tranched = capital::is_tranched(vault::capital_structure(vault));
    let tranche_valid = if (tranched) {
        tranche_code == 1 || tranche_code == 2
    } else { tranche_code == 0 };
    let asset_known = spoke::has_asset(vault::spoke_ref(vault, spoke_id), asset_code);

    if (registry::is_paused(cfg) || vault::deposits_paused(vault)) {
        reject = REJECT_PAUSED;
    } else if (!vault::is_open(vault)) {
        reject = REJECT_NOT_OPEN;
    } else if (!asset_known) {
        reject = REJECT_UNKNOWN_ASSET;
    } else if (amount_raw == 0 || amount_raw > U64_MAX) {
        reject = REJECT_AMOUNT_INVALID;
    } else if (!tranche_valid) {
        reject = REJECT_TRANCHE_INVALID;
    } else if (ts_ms < now && now - ts_ms > spoke::ack_deadline_ms(vault::spoke_ref(vault, spoke_id))) {
        // The spoke-side reclaim window opens after a strictly larger
        // timeout, so refusing here can never race a refund.
        reject = REJECT_ACK_DEADLINE;
    };

    if (reject == REJECT_NONE) {
        // Relayer-side attestation checks: abort (retry with the right
        // attestation), never reject.
        let marker = spoke::asset_marker(vault::spoke_ref(vault, spoke_id), asset_code);
        assert!(price::asset(&att) == marker, errors::price_asset_mismatch());
        vault::check_attestation(vault, cfg, &att, clock);

        let state = capital::risk_state_code(&capital::risk_state(vault::book(vault)));
        let tranche = capital::tranche_from_code(tranche_code);
        let (entry_haircut_bps, _) = vault::haircuts(vault);
        let gross = mul_div(amount64 as u128, price::price(&att), price::price_scale());
        let net = gross * (BPS_DENOM - (entry_haircut_bps as u128)) / BPS_DENOM;

        if (state == 2 || state == 3) {
            reject = REJECT_RISK_STATE;
        } else if (capital::is_senior(&tranche) && state != 0) {
            reject = REJECT_RISK_STATE;
        } else if (net == 0 || net > U64_MAX) {
            reject = REJECT_AMOUNT_INVALID;
        } else if (
            capital::is_senior(&tranche)
                && (junior_nav as u256) * (BPS_DENOM as u256)
                    < (capital::target_junior_bps(vault::capital_structure(vault)) as u256)
                        * ((nav + net) as u256)
        ) {
            reject = REJECT_SENIOR_BUFFER;
        } else {
            let tranche_nav = if (capital::is_senior(&tranche)) { senior_nav } else if (
                capital::is_junior(&tranche)
            ) { junior_nav } else { nav };
            let supply = capital::supply_of(vault::book(vault), &tranche);
            if (supply > 0 && tranche_nav == 0) {
                reject = REJECT_ZERO_OR_DEAD;
            } else {
                value = net as u64;
                shares = fees::shares_for_value(value, tranche_nav, supply, vault::share_offset());
                if (shares == 0) {
                    reject = REJECT_ZERO_OR_DEAD;
                    value = 0;
                } else {
                    generation = if (capital::is_junior(&tranche)) {
                        capital::active_junior_generation(vault::book(vault))
                    } else { 0 };
                    locked_until_ms = now + vault::lockup_ms(vault);
                    capital::on_deposit(vault::mc_book_mut(vault), &tranche, value, shares);
                    vault::mc_bump_capital_seq(vault);
                    let s = vault::spoke_mut(vault, spoke_id);
                    spoke::credit_free(s, asset_code, amount64);
                    spoke::credit_holding(
                        s,
                        depositor,
                        tranche_code,
                        shares,
                        value,
                        locked_until_ms,
                        generation,
                    );
                    // Post-deposit commitment re-test at the grown NAV
                    // (mirrors `deposit_internal`).
                    let post_nav = nav + (value as u128);
                    let post_junior = if (capital::is_senior(&tranche)) { junior_nav } else {
                        junior_nav + (value as u128)
                    };
                    vault::mc_retest_commitment(vault, cfg, post_nav, post_junior);
                }
            }
        }
    };

    let accepted = reject == REJECT_NONE;
    events::emit_spoke_deposit_processed(
        object::id(vault),
        spoke_id,
        deposit_seq,
        depositor,
        asset_code,
        amount64,
        tranche_code,
        reject,
        value,
        shares,
        generation,
        locked_until_ms,
    );

    let hub_chain = endpoint::hub_chain_id(reg);
    let vault_addr = object::id(vault).to_address();
    let (out_seq, dst_chain, dst_app) = {
        let s = vault::spoke_mut(vault, spoke_id);
        (spoke::next_outbound_seq(s), spoke::chain_id(s), spoke::vault_address(s))
    };
    let bytes = wire::encode_deposit_ack(
        hub_chain,
        dst_chain,
        vault_addr,
        dst_app,
        out_seq,
        deposit_seq,
        accepted,
        shares,
    );
    let ep = spoke::endpoint(vault::spoke_ref(vault, spoke_id));
    endpoint::make_outbound(ep, dst_chain, dst_app, out_seq, wire::deposit_ack_type(), bytes)
}

// ══════════════════════ withdraw request (§5) ══════════════════════

/// Apply one `WithdrawRequest`: burn the shares in full at current NAV
/// (exit haircut + fee crystallization mirroring `fulfill_next`), book
/// the payout as a spoke payable (§5.1), and instruct the spoke to pay —
/// or reject with `pay_amount = 0`. Fee value stays in the vault: the
/// curator's net mints into the commitment escrow, the protocol's cut
/// into the protocol-fee escrow.
public fun handle_withdraw_request(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    reg: &EndpointRegistry,
    msg: VerifiedInbound,
    appraisal: Appraisal,
    att: PriceAttestation,
    clock: &Clock,
    ctx: &mut TxContext,
): OutboundMessage {
    let (spoke_id, inbound) = admit(vault, reg, msg, wire::withdraw_request_type());
    let (_, request_seq, user, tranche_code, shares_req, all) =
        wire::as_withdraw_request(&inbound);

    let now = clock.timestamp_ms();
    let nav = vault::mc_consume_appraisal(vault, appraisal);
    let (senior_nav, junior_nav) = vault::mc_sync_capital(vault, cfg, nav, now);

    // Batch-locked figures, post-sync pre-burn (mirrors `Fulfillment`).
    let locked_claim = capital::senior_claim(vault::book(vault));
    let senior_supply = capital::senior_shares(vault::book(vault));
    let tranched = capital::is_tranched(vault::capital_structure(vault));

    let mut reject = REJECT_NONE;
    let mut shares = 0u128;
    let mut value = 0u64;
    let mut basis = 0u64;
    let mut gross_fee = 0u64;
    let mut protocol_cut = 0u64;
    let mut curator_net = 0u64;
    let mut pay_units = 0u64;

    let tranche_valid = if (tranched) {
        tranche_code == 1 || tranche_code == 2
    } else { tranche_code == 0 };

    if (!tranche_valid) {
        reject = REJECT_TRANCHE_INVALID;
    } else if (!spoke::has_holding(vault::spoke_ref(vault, spoke_id), user, tranche_code)) {
        reject = REJECT_NO_HOLDING;
    } else {
        let (h_shares, _h_basis, locked_until, h_generation) =
            spoke::holding_fields(vault::spoke_ref(vault, spoke_id), user, tranche_code);
        let tranche = capital::tranche_from_code(tranche_code);
        let active_gen = capital::active_junior_generation(vault::book(vault));
        if (capital::is_junior(&tranche) && h_generation < active_gen) {
            // Worthless wiped-generation claim: burn it so the ledger
            // stays clean (mirrors `burn_wiped_position`).
            let burned =
                spoke::burn_wiped_holding(vault::spoke_mut(vault, spoke_id), user, tranche_code);
            let _ = burned;
            reject = REJECT_WIPED;
        } else if (now < locked_until) {
            reject = REJECT_LOCKED;
        } else if (
            tranched
                && capital::is_junior(&tranche)
                && capital::is_junior_blocked(vault::book(vault))
        ) {
            // §3.6 class block: the hub-side junior lane queues in this
            // state; the spoke path answers "not now" — re-request after
            // recovery.
            reject = REJECT_JUNIOR_BLOCKED;
        } else {
            shares = if (all) { h_shares } else { shares_req };
            if (shares == 0 || shares > h_shares) {
                reject = REJECT_SHARES_EXCEED;
                shares = 0;
            }
        }
    };

    if (reject == REJECT_NONE) {
        let tranche = capital::tranche_from_code(tranche_code);
        let (t_nav, t_supply) = if (capital::is_senior(&tranche)) {
            (senior_nav, senior_supply)
        } else {
            (junior_nav, capital::junior_shares(vault::book(vault)))
        };
        value = fees::claim_value(shares, t_nav, t_supply, vault::share_offset());

        // Payout conversion: attested marker price + exit haircut
        // (relayer-side checks abort).
        let payout_code = spoke::payout_asset(vault::spoke_ref(vault, spoke_id));
        let marker = spoke::asset_marker(vault::spoke_ref(vault, spoke_id), payout_code);
        assert!(price::asset(&att) == marker, errors::price_asset_mismatch());
        vault::check_attestation(vault, cfg, &att, clock);
        let (_, exit_haircut_bps) = vault::haircuts(vault);
        let eff =
            price::price(&att) * (BPS_DENOM + (exit_haircut_bps as u128)) / BPS_DENOM;

        // Peek the basis slice for crystallization before mutating.
        let (h_shares_total, h_basis_total, _, _) =
            spoke::holding_fields(vault::spoke_ref(vault, spoke_id), user, tranche_code);
        let basis_slice = if (shares == h_shares_total) { h_basis_total } else {
            (((h_basis_total as u256) * (shares as u256) / (h_shares_total as u256)) as u64)
        };
        let (_, gf, pc, cn) = fees::crystallize(
            value,
            basis_slice,
            vault::curator_fee_bps(vault),
            registry::protocol_fee_bps(cfg),
        );
        gross_fee = gf;
        protocol_cut = pc;
        curator_net = cn;
        let payout_n = value - gross_fee;
        pay_units = mul_div(payout_n as u128, price::price_scale(), eff) as u64;

        if (pay_units == 0) {
            // Dust exit would burn shares for nothing — refuse instead.
            reject = REJECT_ZERO_OR_DEAD;
            value = 0;
            gross_fee = 0;
            protocol_cut = 0;
            curator_net = 0;
            shares = 0;
        } else {
            basis = spoke::debit_holding(vault::spoke_mut(vault, spoke_id), user, tranche_code, shares);
            capital::on_fulfill(
                vault::mc_book_mut(vault),
                &tranche,
                shares,
                locked_claim,
                senior_supply,
            );
            // Fee mints at the same pre-burn batch ratio (§3.5).
            if (curator_net > 0) {
                let m = fees::shares_for_value(curator_net, t_nav, t_supply, vault::share_offset());
                if (m > 0) {
                    capital::on_fee_mint(vault::mc_book_mut(vault), &tranche, m, curator_net);
                    vault::mc_credit_commitment(vault, &tranche, m, curator_net, ctx);
                };
            };
            if (protocol_cut > 0) {
                let m =
                    fees::shares_for_value(protocol_cut, t_nav, t_supply, vault::share_offset());
                if (m > 0) {
                    capital::on_fee_mint(vault::mc_book_mut(vault), &tranche, m, protocol_cut);
                    vault::mc_credit_protocol_escrow(vault, &tranche, m, protocol_cut, ctx);
                };
            };
            spoke::book_payable(vault::spoke_mut(vault, spoke_id), payout_code, pay_units);
            vault::mc_bump_capital_seq(vault);
        }
    };

    let payable_after = {
        let s = vault::spoke_ref(vault, spoke_id);
        spoke::payable(s, spoke::payout_asset(s))
    };
    events::emit_spoke_withdraw_processed(
        object::id(vault),
        spoke_id,
        request_seq,
        user,
        tranche_code,
        reject,
        shares,
        value,
        basis,
        gross_fee,
        protocol_cut,
        curator_net,
        pay_units,
        payable_after,
    );

    let hub_chain = endpoint::hub_chain_id(reg);
    let vault_addr = object::id(vault).to_address();
    let (out_seq, dst_chain, dst_app) = {
        let s = vault::spoke_mut(vault, spoke_id);
        (spoke::next_outbound_seq(s), spoke::chain_id(s), spoke::vault_address(s))
    };
    let bytes = wire::encode_withdraw_ack(
        hub_chain,
        dst_chain,
        vault_addr,
        dst_app,
        out_seq,
        request_seq,
        user,
        pay_units as u128,
    );
    let ep = spoke::endpoint(vault::spoke_ref(vault, spoke_id));
    endpoint::make_outbound(ep, dst_chain, dst_app, out_seq, wire::withdraw_ack_type(), bytes)
}

// ══════════════════════ payout receipt (§5) ══════════════════════

/// The spoke physically paid a queued withdrawal: extinguish the
/// payable and the recognized funds backing it. Books clamp rather than
/// abort (the lane must keep moving); a non-zero `unmatched` in the
/// event is a reconciliation alarm.
public fun handle_payout_receipt(
    vault: &mut TradingVault,
    reg: &EndpointRegistry,
    msg: VerifiedInbound,
) {
    let (spoke_id, inbound) = admit(vault, reg, msg, wire::payout_receipt_type());
    let (_, request_seq, amount_raw) = wire::as_payout_receipt(&inbound);
    let amount = if (amount_raw > U64_MAX) { U64_MAX as u64 } else { amount_raw as u64 };
    let payout_code = spoke::payout_asset(vault::spoke_ref(vault, spoke_id));
    let unmatched = spoke::settle_payout(vault::spoke_mut(vault, spoke_id), payout_code, amount);
    // Assets and liability shrink together, so NAV is unchanged and no
    // appraisal seq needs bumping.
    events::emit_spoke_payout_settled(object::id(vault), spoke_id, request_seq, amount, unmatched);
}

// ══════════════════════ state sync (§2.1, §3) ══════════════════════

/// Freshness heartbeat + reconciliation cross-check. The hub's books
/// stay authoritative (event-sourced); a report that disagrees with
/// them raises `divergent` for off-chain alarming but changes nothing.
public fun handle_state_sync(
    vault: &mut TradingVault,
    reg: &EndpointRegistry,
    msg: VerifiedInbound,
) {
    let (spoke_id, inbound) = admit(vault, reg, msg, wire::state_sync_type());
    let (_, asset_codes, frees, reserveds, fee_pot, integration_raw, ts_ms) =
        wire::as_state_sync(&inbound);

    // Reported < book is normal (an ACK or receipt still in flight the
    // other way keeps funds `pending` spoke-side); reported > book means
    // the spoke recognizes funds the hub never minted against — alarm.
    let mut divergent = false;
    {
        let s = vault::spoke_ref(vault, spoke_id);
        let mut i = 0;
        while (i < asset_codes.length()) {
            let code = asset_codes[i];
            if (spoke::has_asset(s, code)) {
                let reported = frees[i] + reserveds[i];
                if (reported > (spoke::free_total(s, code) as u128)) { divergent = true };
            } else {
                divergent = true;
            };
            i = i + 1;
        };
    };
    spoke::record_sync(vault::spoke_mut(vault, spoke_id), ts_ms, fee_pot);
    events::emit_spoke_state_synced(
        object::id(vault),
        spoke_id,
        ts_ms,
        fee_pot,
        divergent,
        integration_raw.length(),
    );
}

// ══════════════════════ appraisal spoke leg (§5.1) ══════════════════════

/// Record one spoke's NAV leg: recognized funds valued at attested
/// marker prices plus hub-valued integration equity, and outstanding
/// payables as a liability. Aborts `spoke_stale` when the last applied
/// StateSync is older than the spoke's `max_sync_age_ms` — a dark spoke
/// blocks NAV completion (the safe failure mode).
public fun record_spoke_state(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    a: &mut Appraisal,
    spoke_id: u64,
    atts: vector<PriceAttestation>,
    clock: &Clock,
) {
    let s = vault::spoke_ref(vault, spoke_id);
    let now = clock.timestamp_ms();
    let last = spoke::last_sync_ms(s);
    if (last < now) {
        assert!(now - last <= spoke::max_sync_age_ms(s), errors::spoke_stale());
    };

    let codes = spoke::asset_codes(s);
    let mut contribution = 0u128;
    let mut liability = 0u128;
    let mut i = 0;
    while (i < codes.length()) {
        let code = codes[i];
        let marker = spoke::asset_marker(s, code);
        let px = find_price(vault, cfg, &atts, marker, clock);
        contribution =
            contribution + mul_div(spoke::free_total(s, code) as u128, px, price::price_scale());
        liability = liability + mul_div(spoke::payable(s, code) as u128, px, price::price_scale());
        i = i + 1;
    };
    contribution = contribution + (spoke::integration_value(s) as u128);
    vault::appraisal_record_spoke(vault, a, spoke_id, contribution, liability);
    events::emit_spoke_state_recorded(object::id(vault), spoke_id, contribution, liability);
}

fun find_price(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    atts: &vector<PriceAttestation>,
    marker: type_name::TypeName,
    clock: &Clock,
): u128 {
    let mut i = 0;
    while (i < atts.length()) {
        let att = &atts[i];
        if (price::asset(att) == marker) {
            vault::check_attestation(vault, cfg, att, clock);
            return price::price(att)
        };
        i = i + 1;
    };
    abort errors::attestation_missing()
}

// ══════════════════════ config sync (§3) ══════════════════════

/// Build the current gate/identity snapshot for a spoke. Permissionless
/// — the contents are pure vault state; the messenger cranks it on every
/// relevant hub event and on a heartbeat cadence.
public fun build_config_sync(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    reg: &EndpointRegistry,
    spoke_id: u64,
): OutboundMessage {
    let hub_chain = endpoint::hub_chain_id(reg);
    let vault_addr = object::id(vault).to_address();
    let paused =
        registry::is_paused(cfg) || vault::deposits_paused(vault) || !vault::is_open(vault);
    let risk_off = vault::is_risk_off(vault);
    let s = vault::spoke_mut(vault, spoke_id);
    let out_seq = spoke::next_outbound_seq(s);
    let bytes = wire::encode_config_sync(
        hub_chain,
        spoke::chain_id(s),
        vault_addr,
        spoke::vault_address(s),
        out_seq,
        paused,
        risk_off,
        spoke::curator_address(s),
        spoke::endpoint_code(s),
        spoke::integrations_root(s),
    );
    endpoint::make_outbound(
        spoke::endpoint(s),
        spoke::chain_id(s),
        spoke::vault_address(s),
        out_seq,
        wire::config_sync_type(),
        bytes,
    )
}
