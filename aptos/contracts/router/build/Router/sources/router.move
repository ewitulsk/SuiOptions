/// Cross-venue sweep router: fill listings on any live Aptos marketplace
/// in one atomic transaction, with a buyer-side add-on fee.
///
/// Every venue buy path is `public entry`, hence callable from this module.
/// Venue interfaces are linked against source shims in `deps/` whose
/// signatures were verified against on-chain ABIs on 2026-09-03; the shims
/// are never published. Sweeps stay atomic at P0: any stale listing aborts
/// the whole transaction and the API re-simulates before signing.
///
/// Fees start at zero and are raised by admin mutation after shakeout.
module router::router {
    use std::error;
    use std::option;
    use std::signer;
    use std::string::String;
    use std::vector;

    use aptos_framework::aptos_coin::AptosCoin;
    use aptos_framework::event;
    use aptos_framework::fungible_asset::Metadata;
    use aptos_framework::object::{Self, Object};
    use aptos_framework::primary_fungible_store;

    use router::admin::{Self, AdminCap};

    use bluemove_v2::coin_listing as bluemove_listing;
    use bluemove_v2::listing as bluemove_list;
    use okx::okx_fixed_price;
    use rarible::coin_listing as rarible_listing;
    use rarible::listing as rarible_list;
    use topaz_v2::coin_listing as topaz_listing;
    use topaz_v2::listing as topaz_list;
    use tradeport_v2::listings as tp_listings_v1;
    use tradeport_v2::listings_v2 as tp_listings;
    use wapal::coin_listing as wapal_listing;
    use wapal::listing as wapal_list;

    /// Venue ids.
    const VENUE_WAPAL: u8 = 1;
    const VENUE_RARIBLE: u8 = 2;
    const VENUE_TOPAZ_V2: u8 = 3;
    const VENUE_BLUEMOVE_V2: u8 = 4;
    const VENUE_TRADEPORT_V2: u8 = 5;
    const VENUE_TRADEPORT_V1: u8 = 6;
    const VENUE_OKX: u8 = 7;

    const FEE_DENOMINATOR: u64 = 10_000;

    /// APT fungible-asset metadata object.
    const APT_METADATA: address = @0xa;

    /// Unknown venue id.
    const EUNKNOWN_VENUE: u64 = 1;
    /// Venue disabled by admin.
    const EVenue_DISABLED: u64 = 2;
    /// Vector length mismatch.
    const ELENGTH_MISMATCH: u64 = 3;
    /// Listing price moved since the payload was built.
    const EPRICE_MISMATCH: u64 = 4;

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    struct RouterConfig has key {
        treasury: address,
        /// Buyer-side add-on, in basis points.
        fee_bps: u64,
        /// Floor per sweep, in octas.
        min_fee: u64,
        /// Venue id -> enabled. Absent counts as disabled.
        enabled: vector<bool>,
    }

    #[event]
    /// Emitted once at router creation for deploy-tooling discovery.
    struct RouterBorn has drop, store {
        creator: address,
        config: address,
        admin_cap: address,
    }

    #[event]
    /// Attribution for exactly one fill. The indexer records this as
    /// attribution only, never as a second sale.
    struct RouterFill has drop, store {
        venue: u8,
        listing: address,
        price: u64,
        fee: u64,
        buyer: address,
    }

    /// Publish the router config and mint the transferable admin cap.
    /// `enabled_venues` lists venue ids live at deploy time.
    public entry fun init(
        creator: &signer,
        treasury: address,
        fee_bps: u64,
        min_fee: u64,
        enabled_venues: vector<u8>,
    ) {
        let (_, cap) = init_internal(creator, treasury, fee_bps, min_fee, enabled_venues);
        // The cap object is owned by the creator at birth; the explicit
        // self-transfer only satisfies the move checker. Handing admin to a
        // multisig later is `transfer_admin_cap` to its address.
        let creator_addr = signer::address_of(creator);
        object::transfer(creator, cap, creator_addr);
    }

    public fun init_internal(
        creator: &signer,
        treasury: address,
        fee_bps: u64,
        min_fee: u64,
        enabled_venues: vector<u8>,
    ): (Object<RouterConfig>, Object<AdminCap>) {
        let constructor_ref = object::create_object_from_account(creator);
        let config_signer = object::generate_signer(&constructor_ref);
        let enabled = vector::empty<bool>();
        let i = 0;
        while (i < VENUE_OKX) {
            vector::push_back(&mut enabled, false);
            i = i + 1;
        };
        let j = 0;
        let m = vector::length(&enabled_venues);
        while (j < m) {
            let v = *vector::borrow(&enabled_venues, j);
            if (v >= 1 && v <= VENUE_OKX) {
                *vector::borrow_mut(&mut enabled, ((v - 1) as u64)) = true;
            };
            j = j + 1;
        };
        move_to(&config_signer, RouterConfig { treasury, fee_bps, min_fee, enabled });
        let cap = admin::create_admin_cap(creator);
        let config = object::object_from_constructor_ref(&constructor_ref);
        event::emit(RouterBorn {
            creator: signer::address_of(creator),
            config: object::object_address(&config),
            admin_cap: object::object_address(&cap),
        });
        (config, cap)
    }

    /// Flip a venue on or off without a redeploy.
    public entry fun set_venue_enabled(
        admin_signer: &signer,
        cap: Object<AdminCap>,
        config: Object<RouterConfig>,
        venue: u8,
        enabled_flag: bool,
    ) acquires RouterConfig {
        let config_addr = object::object_address(&config);
        assert!(exists<RouterConfig>(config_addr), error::not_found(EUNKNOWN_VENUE));
        admin::assert_admin(&cap, signer::address_of(admin_signer));
        assert!(venue >= 1 && venue <= VENUE_OKX, error::invalid_argument(EUNKNOWN_VENUE));
        let cfg = borrow_global_mut<RouterConfig>(config_addr);
        *vector::borrow_mut(&mut cfg.enabled, ((venue - 1) as u64)) = enabled_flag;
    }

    /// Retune the buyer-side fee without a redeploy.
    public entry fun set_fee(
        admin_signer: &signer,
        cap: Object<AdminCap>,
        config: Object<RouterConfig>,
        fee_bps: u64,
        min_fee: u64,
    ) acquires RouterConfig {
        let config_addr = object::object_address(&config);
        assert!(exists<RouterConfig>(config_addr), error::not_found(EUNKNOWN_VENUE));
        admin::assert_admin(&cap, signer::address_of(admin_signer));
        let cfg = borrow_global_mut<RouterConfig>(config_addr);
        cfg.fee_bps = fee_bps;
        cfg.min_fee = min_fee;
    }

    /// Pure fee quote: `sum(prices) * fee_bps / 10_000`, floored at `min_fee`.
    public fun quote_fee(config: Object<RouterConfig>, total: u64): u64 acquires RouterConfig {
        let config_addr = object::object_address(&config);
        assert!(exists<RouterConfig>(config_addr), error::not_found(EUNKNOWN_VENUE));
        let cfg = borrow_global<RouterConfig>(config_addr);
        let fee = total * cfg.fee_bps / FEE_DENOMINATOR;
        if (fee < cfg.min_fee) {
            cfg.min_fee
        } else {
            fee
        }
    }

    /// Fill one listing per vector slot across venues, atomically. The fee is
    /// taken in APT up front; each fill emits `RouterFill`.
    ///
    /// Parallel vectors: `venues[i]` selects the adapter, `listings[i]` is
    /// the listing object (or, for OKX, the listing address; for Tradeport
    /// v1, the creator address), `expected_prices[i]` is the exact price the
    /// payload builder simulated. The `v1_*` vectors carry Tradeport v1
    /// `TokenId` fields; other venues ignore them.
    public entry fun buy_many(
        buyer: &signer,
        config: Object<RouterConfig>,
        venues: vector<u8>,
        listings: vector<address>,
        expected_prices: vector<u64>,
        v1_creators: vector<address>,
        v1_collections: vector<String>,
        v1_names: vector<String>,
        v1_property_versions: vector<u64>,
    ) acquires RouterConfig {
        let n = vector::length(&venues);
        assert!(vector::length(&listings) == n, error::invalid_argument(ELENGTH_MISMATCH));
        assert!(vector::length(&expected_prices) == n, error::invalid_argument(ELENGTH_MISMATCH));
        assert!(vector::length(&v1_creators) == n, error::invalid_argument(ELENGTH_MISMATCH));
        assert!(vector::length(&v1_collections) == n, error::invalid_argument(ELENGTH_MISMATCH));
        assert!(vector::length(&v1_names) == n, error::invalid_argument(ELENGTH_MISMATCH));
        assert!(vector::length(&v1_property_versions) == n, error::invalid_argument(ELENGTH_MISMATCH));

        let config_addr = object::object_address(&config);
        assert!(exists<RouterConfig>(config_addr), error::not_found(EUNKNOWN_VENUE));
        let total = 0;
        let k = 0;
        while (k < n) {
            total = total + *vector::borrow(&expected_prices, k);
            k = k + 1;
        };
        // Validate venues and copy the treasury, then end the borrow before
        // calling `quote_fee` (same-module re-borrow is rejected).
        let treasury = {
            let cfg = borrow_global<RouterConfig>(config_addr);
            let i = 0;
            while (i < n) {
                assert_venue_enabled(&cfg.enabled, *vector::borrow(&venues, i));
                i = i + 1;
            };
            cfg.treasury
        };
        let fee = quote_fee(config, total);
        if (fee != 0) {
            let apt = object::address_to_object<Metadata>(APT_METADATA);
            let fee_fa = primary_fungible_store::withdraw(buyer, apt, fee);
            primary_fungible_store::deposit(treasury, fee_fa);
        };

        let buyer_addr = signer::address_of(buyer);
        // Pop from the back so String args move (no copy). Lengths were
        // checked equal above, so pops stay aligned; fill order is reversed.
        while (!vector::is_empty(&venues)) {
            let venue = vector::pop_back(&mut venues);
            let listing = vector::pop_back(&mut listings);
            let expected = vector::pop_back(&mut expected_prices);
            let v1_creator = vector::pop_back(&mut v1_creators);
            let v1_collection = vector::pop_back(&mut v1_collections);
            let v1_name = vector::pop_back(&mut v1_names);
            let v1_pv = vector::pop_back(&mut v1_property_versions);
            if (venue == VENUE_WAPAL) {
                let l = object::address_to_object<wapal_list::Listing>(listing);
                assert_price(wapal_listing::price<AptosCoin>(l), expected);
                wapal_listing::purchase<AptosCoin>(buyer, l);
            } else if (venue == VENUE_RARIBLE) {
                let l = object::address_to_object<rarible_list::Listing>(listing);
                assert_price(rarible_listing::price<AptosCoin>(l), expected);
                rarible_listing::purchase<AptosCoin>(buyer, l);
            } else if (venue == VENUE_TOPAZ_V2) {
                let l = object::address_to_object<topaz_list::Listing>(listing);
                assert_price(topaz_listing::price<AptosCoin>(l), expected);
                topaz_listing::purchase<AptosCoin>(buyer, l);
            } else if (venue == VENUE_BLUEMOVE_V2) {
                let l = object::address_to_object<bluemove_list::Listing>(listing);
                assert_price(bluemove_listing::price<AptosCoin>(l), expected);
                bluemove_listing::purchase<AptosCoin>(buyer, l, expected);
            } else if (venue == VENUE_TRADEPORT_V2) {
                let l = object::address_to_object<tp_listings::Listing>(listing);
                tp_listings::buy_token(buyer, l);
            } else if (venue == VENUE_TRADEPORT_V1) {
                tp_listings_v1::buy_token(
                    buyer,
                    v1_creator,
                    v1_collection,
                    v1_name,
                    v1_pv,
                );
            } else if (venue == VENUE_OKX) {
                okx_fixed_price::buy_direct_listing<AptosCoin>(buyer, listing, expected);
            } else {
                abort error::invalid_argument(EUNKNOWN_VENUE)
            };
            event::emit(RouterFill { venue, listing, price: expected, fee, buyer: buyer_addr });
        };
    }

    inline fun assert_venue_enabled(enabled: &vector<bool>, venue: u8) {
        assert!(venue >= 1 && venue <= VENUE_OKX, error::invalid_argument(EUNKNOWN_VENUE));
        assert!(*vector::borrow(enabled, ((venue - 1) as u64)), error::permission_denied(EVenue_DISABLED));
    }

    inline fun assert_price(actual: option::Option<u64>, expected: u64) {
        assert!(
            option::is_some(&actual) && *option::borrow(&actual) == expected,
            error::invalid_argument(EPRICE_MISMATCH),
        );
    }

    #[view]
    public fun venue_enabled(config: Object<RouterConfig>, venue: u8): bool acquires RouterConfig {
        let config_addr = object::object_address(&config);
        assert!(exists<RouterConfig>(config_addr), error::not_found(EUNKNOWN_VENUE));
        assert!(venue >= 1 && venue <= VENUE_OKX, error::invalid_argument(EUNKNOWN_VENUE));
        *vector::borrow(&borrow_global<RouterConfig>(config_addr).enabled, ((venue - 1) as u64))
    }
}
