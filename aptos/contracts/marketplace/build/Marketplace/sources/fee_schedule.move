/// Defines the charges associated with using a marketplace, namely:
/// * Listing rate, the units charged for creating a listing (zero at launch).
/// * Bidding rate, the units per bid made by a potential buyer (zero at launch).
/// * Commission, the units transferred to the marketplace upon sale.
///
/// Every mutation requires the transferable [`AdminCap`](marketplace::admin)
/// object, so admin can move to a multisig without a contract upgrade.
/// Listings reference the shared schedule object, so mutations apply to
/// existing listings.
module marketplace::fee_schedule {
    use std::error;
    use std::signer;
    use std::string::{Self, String};
    use aptos_std::math64;
    use aptos_std::smart_table::{Self, SmartTable};

    use aptos_std::type_info;

    use aptos_framework::event;
    use aptos_framework::fungible_asset::Metadata;
    use aptos_framework::object::{Self, ConstructorRef, ExtendRef, Object};

    use marketplace::admin::{Self, AdminCap};

    /// FeeSchedule does not exist.
    const ENO_FEE_SCHEDULE: u64 = 1;
    /// The denominator in a fraction cannot be zero.
    const EDENOMINATOR_IS_ZERO: u64 = 2;
    /// The value represented by a fraction cannot be greater than 1.
    const EEXCEEDS_MAXIMUM: u64 = 3;
    /// The quote token is not allowlisted or is disabled.
    const EQUOTE_NOT_ALLOWED: u64 = 5;
    /// Discount exceeds the commission rate.
    const EDISCOUNT_TOO_LARGE: u64 = 6;

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    /// Defines marketplace fees
    struct FeeSchedule has key {
        /// Address to send fees to
        fee_address: address,
        /// Ref for changing the configuration of the marketplace
        extend_ref: ExtendRef,
    }

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    /// Per-marketplace trading configuration: allowlisted quote tokens,
    /// per-collection commission overrides and per-wallet discounts.
    struct MarketplaceConfig has key {
        /// Quote token (FA metadata address) -> config. Only enabled tokens
        /// can be listed, offered or filled in.
        quotes: SmartTable<address, QuoteConfig>,
        /// Collection object address -> commission override. The override
        /// replaces the schedule numerator; the schedule denominator applies.
        collection_commission: SmartTable<address, u64>,
        /// Wallet address -> discount numerator subtracted from the
        /// commission numerator (floored at zero); schedule denominator applies.
        wallet_discount: SmartTable<address, u64>,
    }

    struct QuoteConfig has copy, drop, store {
        enabled: bool,
        /// Floor for the commission on fills priced in this token, in token units.
        min_fee: u64,
    }

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    /// Fixed rate for bidding
    struct FixedRateBiddingFee has drop, key {
        /// Fixed rate for bidding
        bidding_fee: u64,
    }

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    /// Fixed rate for listing
    struct FixedRateListingFee has drop, key {
        /// Fixed rate for listing
        listing_fee: u64,
    }

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    /// Fixed rate for commission
    struct FixedRateCommission has drop, key {
        /// Fixed rate for commission
        commission: u64,
    }

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    /// Percentage-based rate for commission
    struct PercentageRateCommission has drop, key {
        /// Denominator for the commission rate
        denominator: u64,
        /// Numerator for the commission rate
        numerator: u64,
    }

    #[event]
    /// Event representing a change to the marketplace configuration
    struct Mutation has drop, store {
        marketplace: address,
        /// The type info of the struct that was updated.
        updated_resource: String,
    }

    #[event]
    /// Emitted once at venue creation so deploy tooling (and the status
    /// page) can discover the schedule and admin-cap object addresses.
    struct MarketplaceBorn has drop, store {
        creator: address,
        fee_schedule: address,
        admin_cap: address,
    }

    // Initializers

    /// Create a marketplace with a fixed bidding and listing rate and a percentage commission.
    /// Returns the transferable admin cap alongside the schedule object.
    public entry fun init_entry(
        creator: &signer,
        fee_address: address,
        bidding_fee: u64,
        listing_fee: u64,
        commission_denominator: u64,
        commission_numerator: u64,
    ) {
        let (cap, schedule) = init(
            creator,
            fee_address,
            bidding_fee,
            listing_fee,
            commission_denominator,
            commission_numerator,
        );
        event::emit(MarketplaceBorn {
            creator: signer::address_of(creator),
            fee_schedule: object::object_address(&schedule),
            admin_cap: object::object_address(&cap),
        });
    }


    public fun init(
        creator: &signer,
        fee_address: address,
        bidding_fee: u64,
        listing_fee: u64,
        commission_denominator: u64,
        commission_numerator: u64,
    ): (Object<AdminCap>, Object<FeeSchedule>) {
        assert!(
            commission_numerator <= commission_denominator,
            error::invalid_argument(EEXCEEDS_MAXIMUM),
        );
        assert!(
            commission_denominator != 0,
            error::out_of_range(EDENOMINATOR_IS_ZERO),
        );

        let (constructor_ref, fee_schedule_signer) = empty_init(creator, fee_address);
        move_to(&fee_schedule_signer, FixedRateBiddingFee { bidding_fee });
        move_to(&fee_schedule_signer, FixedRateListingFee { listing_fee });
        let commission_rate = PercentageRateCommission {
            denominator: commission_denominator,
            numerator: commission_numerator,
        };
        move_to(&fee_schedule_signer, commission_rate);
        move_to(&fee_schedule_signer, MarketplaceConfig {
            quotes: smart_table::new(),
            collection_commission: smart_table::new(),
            wallet_discount: smart_table::new(),
        });
        let cap = admin::create_admin_cap(creator);
        (cap, object::object_from_constructor_ref(&constructor_ref))
    }

    /// Create a marketplace with no fees.
    public entry fun empty(creator: &signer, fee_address: address) {
        let (_cap, _schedule) = empty_with_cap(creator, fee_address);
    }

    /// Create a marketplace with no fees, returning the admin cap.
    public fun empty_with_cap(
        creator: &signer,
        fee_address: address,
    ): (Object<AdminCap>, Object<FeeSchedule>) {
        let (constructor_ref, fee_schedule_signer) = empty_init(creator, fee_address);
        move_to(&fee_schedule_signer, MarketplaceConfig {
            quotes: smart_table::new(),
            collection_commission: smart_table::new(),
            wallet_discount: smart_table::new(),
        });
        let cap = admin::create_admin_cap(creator);
        (cap, object::object_from_constructor_ref(&constructor_ref))
    }

    inline fun empty_init(creator: &signer, fee_address: address): (ConstructorRef, signer) {
        let constructor_ref = object::create_object_from_account(creator);
        let extend_ref = object::generate_extend_ref(&constructor_ref);
        let fee_schedule_signer = object::generate_signer(&constructor_ref);

        let marketplace = FeeSchedule {
            fee_address,
            extend_ref,
        };
        move_to(&fee_schedule_signer, marketplace);

        (constructor_ref, fee_schedule_signer)
    }

    // Mutators: every mutation requires the admin capability.

    /// Set the fee address
    public entry fun set_fee_address(
        creator: &signer,
        cap: Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
        fee_address: address,
    ) acquires FeeSchedule {
        let (_, fee_schedule_addr) = assert_access(creator, &cap, marketplace);
        let fee_schedule_obj = borrow_global_mut<FeeSchedule>(fee_schedule_addr);
        fee_schedule_obj.fee_address = fee_address;
        let updated_resource = string::utf8(b"fee_address");
        event::emit(Mutation { marketplace: fee_schedule_addr, updated_resource });
    }

    /// Remove any existing listing fees and set a fixed rate listing fee.
    public entry fun set_fixed_rate_listing_fee(
        creator: &signer,
        cap: Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
        fee: u64,
    ) acquires FeeSchedule, FixedRateListingFee {
        let fee_schedule_signer = remove_listing_fee(creator, &cap, marketplace);
        move_to(&fee_schedule_signer, FixedRateListingFee { listing_fee: fee });
        let updated_resource = type_info::type_name<FixedRateListingFee>();
        event::emit(Mutation { marketplace: signer::address_of(&fee_schedule_signer), updated_resource });
    }

    inline fun remove_listing_fee(
        creator: &signer,
        cap: &Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
    ): signer  {
        let (fee_schedule_signer, fee_schedule_addr) = assert_access(creator, cap, marketplace);
        if (exists<FixedRateListingFee>(fee_schedule_addr)) {
            move_from<FixedRateListingFee>(fee_schedule_addr);
        };
        fee_schedule_signer
    }

    /// Remove any existing bidding fees and set a fixed rate bidding fee.
    public entry fun set_fixed_rate_bidding_fee(
        creator: &signer,
        cap: Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
        fee: u64,
    ) acquires FeeSchedule, FixedRateBiddingFee {
        let fee_schedule_signer = remove_bidding_fee(creator, &cap, marketplace);
        move_to(&fee_schedule_signer, FixedRateBiddingFee { bidding_fee: fee });
        let updated_resource = type_info::type_name<FixedRateBiddingFee>();
        event::emit(Mutation { marketplace: signer::address_of(&fee_schedule_signer), updated_resource });
    }

    inline fun remove_bidding_fee(
        creator: &signer,
        cap: &Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
    ): signer  {
        let (fee_schedule_signer, fee_schedule_addr) = assert_access(creator, cap, marketplace);
        if (exists<FixedRateBiddingFee>(fee_schedule_addr)) {
            move_from<FixedRateBiddingFee>(fee_schedule_addr);
        };
        fee_schedule_signer
    }

    /// Remove any existing commission and set a fixed rate commission.
    public entry fun set_fixed_rate_commission(
        creator: &signer,
        cap: Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
        commission: u64,
    ) acquires FeeSchedule, FixedRateCommission, PercentageRateCommission {
        let fee_schedule_signer = remove_commission(creator, &cap, marketplace);
        move_to(&fee_schedule_signer, FixedRateCommission { commission });
        let updated_resource = type_info::type_name<FixedRateCommission>();
        event::emit(Mutation { marketplace: signer::address_of(&fee_schedule_signer), updated_resource });
    }

    /// Remove any existing commission and set a percentage rate commission.
    public entry fun set_percentage_rate_commission(
        creator: &signer,
        cap: Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
        denominator: u64,
        numerator: u64,
    ) acquires FeeSchedule, FixedRateCommission, PercentageRateCommission {
        assert!(
            numerator <= denominator,
            error::invalid_argument(EEXCEEDS_MAXIMUM),
        );
        assert!(
            denominator != 0,
            error::out_of_range(EDENOMINATOR_IS_ZERO),
        );

        let fee_schedule_signer = remove_commission(creator, &cap, marketplace);
        move_to(&fee_schedule_signer, PercentageRateCommission { denominator, numerator });
        let updated_resource = type_info::type_name<PercentageRateCommission>();
        event::emit(Mutation { marketplace: signer::address_of(&fee_schedule_signer), updated_resource });
    }

    inline fun remove_commission(
        creator: &signer,
        cap: &Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
    ): signer  {
        let (fee_schedule_signer, fee_schedule_addr) = assert_access(creator, cap, marketplace);
        if (exists<FixedRateCommission>(fee_schedule_addr)) {
            move_from<FixedRateCommission>(fee_schedule_addr);
        } else if (exists<PercentageRateCommission>(fee_schedule_addr)) {
            move_from<PercentageRateCommission>(fee_schedule_addr);
        };
        fee_schedule_signer
    }

    /// Allowlist a quote token with its per-fill minimum fee. Disabled tokens
    /// keep `end_fixed_price` / offer cancel working but block new listings,
    /// offers and fills.
    public entry fun allow_quote_token(
        creator: &signer,
        cap: Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
        quote: Object<Metadata>,
        min_fee: u64,
    ) acquires MarketplaceConfig {
        let fee_schedule_addr = assert_access_mut(creator, &cap, marketplace);
        let config = borrow_global_mut<MarketplaceConfig>(fee_schedule_addr);
        let quote_addr = object::object_address(&quote);
        smart_table::upsert(&mut config.quotes, quote_addr, QuoteConfig { enabled: true, min_fee });
        event::emit(Mutation {
            marketplace: fee_schedule_addr,
            updated_resource: string::utf8(b"quote_allowlist"),
        });
    }

    /// Enable or disable an allowlisted quote token.
    public entry fun set_quote_enabled(
        creator: &signer,
        cap: Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
        quote: address,
        enabled: bool,
    ) acquires MarketplaceConfig {
        let fee_schedule_addr = assert_access_mut(creator, &cap, marketplace);
        let config = borrow_global_mut<MarketplaceConfig>(fee_schedule_addr);
        assert!(smart_table::contains(&config.quotes, quote), error::not_found(EQUOTE_NOT_ALLOWED));
        smart_table::borrow_mut(&mut config.quotes, quote).enabled = enabled;
        event::emit(Mutation {
            marketplace: fee_schedule_addr,
            updated_resource: string::utf8(b"quote_allowlist"),
        });
    }

    /// Change the per-fill minimum fee for an allowlisted quote token.
    public entry fun set_quote_min_fee(
        creator: &signer,
        cap: Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
        quote: address,
        min_fee: u64,
    ) acquires MarketplaceConfig {
        let fee_schedule_addr = assert_access_mut(creator, &cap, marketplace);
        let config = borrow_global_mut<MarketplaceConfig>(fee_schedule_addr);
        assert!(smart_table::contains(&config.quotes, quote), error::not_found(EQUOTE_NOT_ALLOWED));
        smart_table::borrow_mut(&mut config.quotes, quote).min_fee = min_fee;
        event::emit(Mutation {
            marketplace: fee_schedule_addr,
            updated_resource: string::utf8(b"quote_allowlist"),
        });
    }

    /// Per-collection commission-numerator override (Tradeport shape: the
    /// schedule denominator still applies). Applies to existing listings.
    public entry fun upsert_collection_numerator(
        creator: &signer,
        cap: Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
        collection: address,
        numerator: u64,
    ) acquires MarketplaceConfig, PercentageRateCommission {
        let fee_schedule_addr = assert_access_mut(creator, &cap, marketplace);
        let denominator = schedule_denominator(fee_schedule_addr);
        assert!(numerator <= denominator, error::invalid_argument(EEXCEEDS_MAXIMUM));
        let config = borrow_global_mut<MarketplaceConfig>(fee_schedule_addr);
        smart_table::upsert(&mut config.collection_commission, collection, numerator);
        event::emit(Mutation {
            marketplace: fee_schedule_addr,
            updated_resource: string::utf8(b"collection_commission"),
        });
    }

    /// Remove a per-collection override, falling back to the schedule rate.
    public entry fun remove_collection_override(
        creator: &signer,
        cap: Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
        collection: address,
    ) acquires MarketplaceConfig {
        let fee_schedule_addr = assert_access_mut(creator, &cap, marketplace);
        let config = borrow_global_mut<MarketplaceConfig>(fee_schedule_addr);
        if (smart_table::contains(&config.collection_commission, collection)) {
            smart_table::remove(&mut config.collection_commission, collection);
        };
        event::emit(Mutation {
            marketplace: fee_schedule_addr,
            updated_resource: string::utf8(b"collection_commission"),
        });
    }

    /// Per-wallet discount (`loyalty`): subtracted from the commission
    /// numerator, floored at zero.
    public entry fun set_wallet_discount(
        creator: &signer,
        cap: Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
        wallet: address,
        discount_numerator: u64,
    ) acquires MarketplaceConfig, PercentageRateCommission {
        let fee_schedule_addr = assert_access_mut(creator, &cap, marketplace);
        let denominator = schedule_denominator(fee_schedule_addr);
        assert!(discount_numerator <= denominator, error::invalid_argument(EDISCOUNT_TOO_LARGE));
        let config = borrow_global_mut<MarketplaceConfig>(fee_schedule_addr);
        smart_table::upsert(&mut config.wallet_discount, wallet, discount_numerator);
        event::emit(Mutation {
            marketplace: fee_schedule_addr,
            updated_resource: string::utf8(b"wallet_discount"),
        });
    }

    /// Remove a per-wallet discount.
    public entry fun remove_wallet_discount(
        creator: &signer,
        cap: Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
        wallet: address,
    ) acquires MarketplaceConfig {
        let fee_schedule_addr = assert_access_mut(creator, &cap, marketplace);
        let config = borrow_global_mut<MarketplaceConfig>(fee_schedule_addr);
        if (smart_table::contains(&config.wallet_discount, wallet)) {
            smart_table::remove(&mut config.wallet_discount, wallet);
        };
        event::emit(Mutation {
            marketplace: fee_schedule_addr,
            updated_resource: string::utf8(b"wallet_discount"),
        });
    }

    inline fun assert_access(
        creator: &signer,
        cap: &Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
    ): (signer, address)  {
        let fee_schedule_addr = assert_exists_internal(&marketplace);
        admin::assert_admin(cap, signer::address_of(creator));
        let fee_schedule_obj = borrow_global<FeeSchedule>(fee_schedule_addr);
        let fee_schedule_signer = object::generate_signer_for_extending(&fee_schedule_obj.extend_ref);
        (fee_schedule_signer, fee_schedule_addr)
    }

    inline fun assert_access_mut(
        creator: &signer,
        cap: &Object<AdminCap>,
        marketplace: Object<FeeSchedule>,
    ): address  {
        let fee_schedule_addr = assert_exists_internal(&marketplace);
        admin::assert_admin(cap, signer::address_of(creator));
        fee_schedule_addr
    }

    inline fun schedule_denominator(fee_schedule_addr: address): u64  {
        assert!(
            exists<PercentageRateCommission>(fee_schedule_addr),
            error::invalid_state(EDENOMINATOR_IS_ZERO),
        );
        borrow_global<PercentageRateCommission>(fee_schedule_addr).denominator
    }

    // View functions
    #[view]
    public fun fee_address(marketplace: Object<FeeSchedule>): address acquires FeeSchedule {
        let fee_schedule_addr = assert_exists_internal(&marketplace);
        borrow_global<FeeSchedule>(fee_schedule_addr).fee_address
    }

    #[view]
    public fun listing_fee(
        marketplace: Object<FeeSchedule>,
        _base: u64,
    ): u64 acquires FixedRateListingFee {
        let fee_schedule_addr = assert_exists_internal(&marketplace);
        if (exists<FixedRateListingFee>(fee_schedule_addr)) {
            borrow_global<FixedRateListingFee>(fee_schedule_addr).listing_fee
        } else {
            0
        }
    }

    #[view]
    public fun bidding_fee(
        marketplace: Object<FeeSchedule>,
        _bid: u64,
    ): u64 acquires FixedRateBiddingFee {
        let fee_schedule_addr = assert_exists_internal(&marketplace);
        if (exists<FixedRateBiddingFee>(fee_schedule_addr)) {
            borrow_global<FixedRateBiddingFee>(fee_schedule_addr).bidding_fee
        } else {
            0
        }
    }

    #[view]
    public fun commission(
        marketplace: Object<FeeSchedule>,
        price: u64,
    ): u64 acquires FixedRateCommission, PercentageRateCommission {
        let fee_schedule_addr = assert_exists_internal(&marketplace);
        if (exists<FixedRateCommission>(fee_schedule_addr)) {
            borrow_global<FixedRateCommission>(fee_schedule_addr).commission
        } else if (exists<PercentageRateCommission>(fee_schedule_addr)) {
            let fees = borrow_global<PercentageRateCommission>(fee_schedule_addr);
            math64::mul_div(price, fees.numerator, fees.denominator)
        } else {
            0
        }
    }

    #[view]
    /// Full commission for a fill: percentage rate (collection override wins,
    /// wallet discount subtracted, floored at zero), floored at the quote
    /// token's `min_fee`. Fixed-rate commission, when set, bypasses
    /// overrides and discounts but still respects `min_fee`.
    public fun commission_for(
        marketplace: Object<FeeSchedule>,
        quote: address,
        buyer: address,
        collection: address,
        price: u64,
    ): u64 acquires FixedRateCommission, MarketplaceConfig, PercentageRateCommission {
        let fee_schedule_addr = assert_exists_internal(&marketplace);
        let raw = if (exists<FixedRateCommission>(fee_schedule_addr)) {
            borrow_global<FixedRateCommission>(fee_schedule_addr).commission
        } else if (exists<PercentageRateCommission>(fee_schedule_addr)) {
            let fees = borrow_global<PercentageRateCommission>(fee_schedule_addr);
            let numerator = fees.numerator;
            let config = borrow_global<MarketplaceConfig>(fee_schedule_addr);
            if (smart_table::contains(&config.collection_commission, collection)) {
                numerator = *smart_table::borrow(&config.collection_commission, collection);
            };
            if (smart_table::contains(&config.wallet_discount, buyer)) {
                let discount = *smart_table::borrow(&config.wallet_discount, buyer);
                numerator = if (discount >= numerator) { 0 } else { numerator - discount };
            };
            math64::mul_div(price, numerator, fees.denominator)
        } else {
            0
        };
        let floor = quote_min_fee_internal(fee_schedule_addr, quote);
        if (raw < floor) { floor } else { raw }
    }

    #[view]
    public fun quote_enabled(marketplace: Object<FeeSchedule>, quote: address): bool
    acquires MarketplaceConfig {
        let fee_schedule_addr = assert_exists_internal(&marketplace);
        let config = borrow_global<MarketplaceConfig>(fee_schedule_addr);
        smart_table::contains(&config.quotes, quote)
            && smart_table::borrow(&config.quotes, quote).enabled
    }

    #[view]
    public fun quote_min_fee(marketplace: Object<FeeSchedule>, quote: address): u64
    acquires MarketplaceConfig {
        let fee_schedule_addr = assert_exists_internal(&marketplace);
        quote_min_fee_internal(fee_schedule_addr, quote)
    }

    /// Abort unless `quote` is allowlisted and enabled.
    public fun assert_quote_enabled(marketplace: &Object<FeeSchedule>, quote: address)
    acquires MarketplaceConfig {
        let fee_schedule_addr = assert_exists_internal(marketplace);
        let config = borrow_global<MarketplaceConfig>(fee_schedule_addr);
        assert!(
            smart_table::contains(&config.quotes, quote)
                && smart_table::borrow(&config.quotes, quote).enabled,
            error::invalid_argument(EQUOTE_NOT_ALLOWED),
        );
    }

    inline fun quote_min_fee_internal(fee_schedule_addr: address, quote: address): u64
    {
        let config = borrow_global<MarketplaceConfig>(fee_schedule_addr);
        if (smart_table::contains(&config.quotes, quote)) {
            smart_table::borrow(&config.quotes, quote).min_fee
        } else {
            0
        }
    }

    public fun assert_exists(marketplace: &Object<FeeSchedule>) {
        assert_exists_internal(marketplace);
    }

    inline fun assert_exists_internal(marketplace: &Object<FeeSchedule>): address {
        let fee_schedule_addr = object::object_address(marketplace);
        assert!(
            exists<FeeSchedule>(fee_schedule_addr),
            error::not_found(ENO_FEE_SCHEDULE),
        );
        fee_schedule_addr
    }

    // Tests

    #[test_only]
    use aptos_framework::account;
    #[test_only]
    use aptos_framework::fungible_asset;

    #[test(creator = @0x123)]
    fun test_init(
        creator: &signer,
    ) acquires FeeSchedule, FixedRateBiddingFee, FixedRateCommission, FixedRateListingFee, PercentageRateCommission {
        let creator_addr = signer::address_of(creator);
        account::create_account_for_test(creator_addr);
        let (_cap, obj) = init(creator, creator_addr, 0, 0, 1, 0);

        assert!(fee_address(obj) == creator_addr, 0);
        assert!(listing_fee(obj, 5) == 0, 0);
        assert!(bidding_fee(obj, 5) == 0, 0);
        assert!(commission(obj, 5) == 0, 0);
    }

    #[test(creator = @0x123)]
    fun test_admin_cap_gating(
        creator: &signer,
    ) acquires FeeSchedule, FixedRateBiddingFee, FixedRateCommission, FixedRateListingFee, MarketplaceConfig, PercentageRateCommission {
        let creator_addr = signer::address_of(creator);
        account::create_account_for_test(creator_addr);
        let (cap, obj) = init(creator, creator_addr, 0, 0, 10, 1);

        set_fee_address(creator, cap, obj, @0x0);
        set_fixed_rate_listing_fee(creator, cap, obj, 5);
        set_fixed_rate_bidding_fee(creator, cap, obj, 6);
        set_percentage_rate_commission(creator, cap, obj, 10, 1);

        assert!(fee_address(obj) == @0x0, 0);
        assert!(listing_fee(obj, 5) == 5, 0);
        assert!(bidding_fee(obj, 5) == 6, 0);
        assert!(commission(obj, 20) == 2, 0);

        set_fixed_rate_commission(creator, cap, obj, 8);
        assert!(commission(obj, 20) == 8, 0);

        // Quote allowlist: unknown token is disabled with zero floor.
        let quote = @0xA;
        assert!(!quote_enabled(obj, quote), 0);
        assert!(quote_min_fee(obj, quote) == 0, 0);
    }

    #[test(creator = @0x123, non_creator = @0x223)]
    #[expected_failure(abort_code = 0x50001, location = marketplace::admin)]
    fun test_non_creator_fee_address(creator: &signer, non_creator: &signer)
    acquires FeeSchedule {
        let creator_addr = signer::address_of(creator);
        account::create_account_for_test(creator_addr);
        account::create_account_for_test(signer::address_of(non_creator));
        let (cap, obj) = init(creator, creator_addr, 0, 0, 1, 0);
        // Hand the cap to nobody: non_creator never owns it.
        admin::transfer_admin_cap(creator, cap, creator_addr);
        set_fee_address(non_creator, cap, obj, @0x0);
    }

    #[test(creator = @0x123)]
    fun test_commission_for_overrides_and_floor(
        creator: &signer,
    ) acquires FixedRateCommission, MarketplaceConfig, PercentageRateCommission {
        let creator_addr = signer::address_of(creator);
        account::create_account_for_test(creator_addr);
        // 10% base commission.
        let (cap, obj) = init(creator, creator_addr, 0, 0, 100, 10);
        let (creator_ref, _) = fungible_asset::create_test_token(creator);
        let (mint_ref, _, _, _) = fungible_asset::init_test_metadata(&creator_ref);
        let quote_obj = fungible_asset::mint_ref_metadata(&mint_ref);
        let quote = object::object_address(&quote_obj);
        allow_quote_token(creator, cap, obj, quote_obj, 7);

        // Base: 10% of 100 = 10, above the floor of 7.
        assert!(commission_for(obj, quote, @0x777, @0x0, 100) == 10, 0);
        // Tiny price: 10% of 10 = 1, floored at min_fee 7.
        assert!(commission_for(obj, quote, @0x777, @0x0, 10) == 7, 0);
        // Collection override to 50%: 50 of 100.
        upsert_collection_numerator(creator, cap, obj, @0xC011, 50);
        assert!(commission_for(obj, quote, @0x777, @0xC011, 100) == 50, 0);
        // Wallet discount of 5pp: (10 - 5)% of 100 = 5, floored at 7.
        set_wallet_discount(creator, cap, obj, @0x777, 5);
        assert!(commission_for(obj, quote, @0x777, @0x0, 100) == 7, 0);
        assert!(commission_for(obj, quote, @0x777, @0xC011, 100) == 45, 0);
        // Full discount zeroes the rate; the floor still applies.
        set_wallet_discount(creator, cap, obj, @0x777, 100);
        assert!(commission_for(obj, quote, @0x777, @0x0, 100) == 7, 0);
        remove_wallet_discount(creator, cap, obj, @0x777);
        remove_collection_override(creator, cap, obj, @0xC011);
        assert!(commission_for(obj, quote, @0x777, @0x0, 100) == 10, 0);
    }

    #[test(creator = @0x123)]
    #[expected_failure(abort_code = 0x10005, location = Self)]
    fun test_assert_quote_enabled_rejects_unknown(
        creator: &signer,
    ) acquires MarketplaceConfig {
        let creator_addr = signer::address_of(creator);
        account::create_account_for_test(creator_addr);
        let (_cap, obj) = init(creator, creator_addr, 0, 0, 1, 0);
        assert_quote_enabled(&obj, @0xA);
    }

    #[test(creator = @0x123)]
    #[expected_failure(abort_code = 0x10003, location = Self)]
    fun test_init_too_big_percentage_commission(creator: &signer) {
        let creator_addr = signer::address_of(creator);
        account::create_account_for_test(creator_addr);
        init(creator, creator_addr, 0, 0, 1, 2);
    }
}
