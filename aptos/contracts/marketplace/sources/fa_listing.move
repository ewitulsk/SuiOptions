/// Fixed-price listings priced in any allowlisted fungible asset.
///
/// This replaces the reference `coin_listing` module: the payment leg is
/// `primary_fungible_store` instead of `Coin<CoinType>`, so the quote token
/// is a data field (`Listing::quote`), not a type parameter. Auctions are
/// out of scope at P0.
module marketplace::fa_listing {
    use std::error;
    use std::option::{Self, Option};
    use std::signer;
    use std::string::{Self, String};
    use std::vector;

    use aptos_framework::fungible_asset::{Self, Metadata};
    use aptos_framework::object::{Self, ConstructorRef, Object, ObjectCore};
    use aptos_framework::primary_fungible_store;

    use marketplace::events;
    use marketplace::fee_schedule::{Self, FeeSchedule};
    use marketplace::listing::{Self, Listing, TokenV1Container};

    #[test_only]
    friend marketplace::venue_tests;



    /// There exists no listing.
    const ENO_LISTING: u64 = 1;
    /// The entity is not the seller.
    const ENOT_SELLER: u64 = 6;
    /// Royalty plus commission exceed the price; the fill cannot pay everybody.
    const EFEE_EXCEEDS_PRICE: u64 = 7;
    /// Vector length mismatch in a batch call.
    const ELENGTH_MISMATCH: u64 = 8;

    const FIXED_PRICE_TYPE: vector<u8> = b"fixed price";

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    /// Fixed-price listing priced in `quote`.
    struct FixedPriceListing has key {
        /// The price to purchase the listed item, in quote-token units.
        price: u64,
        /// The FA metadata object the listing is priced in.
        quote: Object<Metadata>,
    }

    // Init functions

    public entry fun init_fixed_price(
        seller: &signer,
        object: Object<ObjectCore>,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        start_time: u64,
        price: u64,
    ) {
        init_fixed_price_internal(seller, object, fee_schedule, quote, start_time, price);
    }

    public(friend) fun init_fixed_price_internal(
        seller: &signer,
        object: Object<ObjectCore>,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        start_time: u64,
        price: u64,
    ): Object<Listing> {
        fee_schedule::assert_quote_enabled(&fee_schedule, object::object_address(&quote));
        let (listing_signer, constructor_ref) = init(
            seller,
            object,
            fee_schedule,
            start_time,
            price,
            quote,
        );

        move_to(&listing_signer, FixedPriceListing { price, quote });

        let listing = object::object_from_constructor_ref(&constructor_ref);

        events::emit_listing_placed(
            fee_schedule,
            string::utf8(FIXED_PRICE_TYPE),
            object::object_address(&listing),
            signer::address_of(seller),
            price,
            object::object_address(&quote),
            fee_schedule::listing_fee(fee_schedule, price),
            listing::token_metadata(listing),
        );

        listing
    }

    public entry fun init_fixed_price_for_tokenv1(
        seller: &signer,
        token_creator: address,
        token_collection: String,
        token_name: String,
        token_property_version: u64,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        start_time: u64,
        price: u64,
    ) {
        init_fixed_price_for_tokenv1_internal(
            seller,
            token_creator,
            token_collection,
            token_name,
            token_property_version,
            fee_schedule,
            quote,
            start_time,
            price,
        );
    }

    public(friend) fun init_fixed_price_for_tokenv1_internal(
        seller: &signer,
        token_creator: address,
        token_collection: String,
        token_name: String,
        token_property_version: u64,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        start_time: u64,
        price: u64,
    ): Object<Listing> {
        let object = listing::create_tokenv1_container(
            seller,
            token_creator,
            token_collection,
            token_name,
            token_property_version,
        );
        init_fixed_price_internal(
            seller,
            object::convert(object),
            fee_schedule,
            quote,
            start_time,
            price,
        )
    }

    /// List many v2 tokens in one transaction. `objects[i]` lists at `prices[i]`.
    public entry fun init_fixed_price_many(
        seller: &signer,
        objects: vector<Object<ObjectCore>>,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        start_time: u64,
        prices: vector<u64>,
    ) {
        assert!(vector::length(&objects) == vector::length(&prices), error::invalid_argument(ELENGTH_MISMATCH));
        let i = 0;
        let n = vector::length(&objects);
        while (i < n) {
            init_fixed_price_internal(
                seller,
                *vector::borrow(&objects, i),
                fee_schedule,
                quote,
                start_time,
                *vector::borrow(&prices, i),
            );
            i = i + 1;
        };
    }

    inline fun init(
        seller: &signer,
        object: Object<ObjectCore>,
        fee_schedule: Object<FeeSchedule>,
        start_time: u64,
        initial_price: u64,
        quote: Object<Metadata>,
    ): (signer, ConstructorRef) {
        let listing_fee = fee_schedule::listing_fee(fee_schedule, initial_price);
        if (listing_fee != 0) {
            let fee = primary_fungible_store::withdraw(seller, quote, listing_fee);
            primary_fungible_store::deposit(fee_schedule::fee_address(fee_schedule), fee);
        };

        listing::init(seller, object, fee_schedule, start_time, object::object_address(&quote))
    }

    // Mutators

    /// Purchase a fixed-price listing. Pays exactly `price` in the listing's
    /// quote token: royalty first, then commission (floored at the token's
    /// `min_fee`), remainder to the seller.
    public entry fun purchase(
        purchaser: &signer,
        object: Object<Listing>,
    ) acquires FixedPriceListing {
        let listing_addr = listing::assert_started(&object);
        assert!(exists<FixedPriceListing>(listing_addr), error::not_found(ENO_LISTING));
        let FixedPriceListing { price, quote } = move_from<FixedPriceListing>(listing_addr);

        let quote_addr = object::object_address(&quote);
        let purchaser_addr = signer::address_of(purchaser);
        let fee_schedule = listing::fee_schedule(object);
        fee_schedule::assert_quote_enabled(&fee_schedule, quote_addr);

        let payment = primary_fungible_store::withdraw(purchaser, quote, price);
        complete_purchase(purchaser, purchaser_addr, object, &mut payment, quote_addr);

        fungible_asset::destroy_zero(payment);
    }

    /// Buy many listings atomically: any stale listing aborts the whole sweep.
    public entry fun purchase_many(
        purchaser: &signer,
        objects: vector<Object<Listing>>,
    ) {
        let i = 0;
        let n = vector::length(&objects);
        while (i < n) {
            purchase(purchaser, *vector::borrow(&objects, i));
            i = i + 1;
        };
    }

    /// Seller-only repricing. Emits a fresh `ListingPlaced` for the same
    /// listing address so the indexer upserts the row.
    public entry fun update_fixed_price(
        seller: &signer,
        object: Object<Listing>,
        new_price: u64,
    ) acquires FixedPriceListing {
        let listing_addr = object::object_address(&object);
        assert!(exists<FixedPriceListing>(listing_addr), error::not_found(ENO_LISTING));
        assert!(
            listing::seller(object) == signer::address_of(seller),
            error::permission_denied(ENOT_SELLER),
        );
        let fixed = borrow_global_mut<FixedPriceListing>(listing_addr);
        fixed.price = new_price;
        let fee_schedule = listing::fee_schedule(object);
        events::emit_listing_placed(
            fee_schedule,
            string::utf8(FIXED_PRICE_TYPE),
            listing_addr,
            signer::address_of(seller),
            new_price,
            object::object_address(&fixed.quote),
            fee_schedule::listing_fee(fee_schedule, new_price),
            listing::token_metadata(object),
        );
    }

    /// End a fixed price listing early. Disabling the quote token never
    /// blocks this: the seller can always get the NFT back.
    public entry fun end_fixed_price(
        seller: &signer,
        object: Object<Listing>,
    ) acquires FixedPriceListing {
        let token_metadata = listing::token_metadata(object);

        let expected_seller_addr = signer::address_of(seller);
        let (actual_seller_addr, fee_schedule) = listing::close(seller, object, expected_seller_addr);
        assert!(expected_seller_addr == actual_seller_addr, error::permission_denied(ENOT_SELLER));

        let listing_addr = object::object_address(&object);
        assert!(exists<FixedPriceListing>(listing_addr), error::not_found(ENO_LISTING));
        let FixedPriceListing { price, quote } = move_from<FixedPriceListing>(listing_addr);

        events::emit_listing_canceled(
            fee_schedule,
            string::utf8(FIXED_PRICE_TYPE),
            listing_addr,
            actual_seller_addr,
            price,
            object::object_address(&quote),
            0,
            token_metadata,
        );
    }

    inline fun complete_purchase(
        completer: &signer,
        purchaser_addr: address,
        object: Object<Listing>,
        payment: &mut fungible_asset::FungibleAsset,
        quote_addr: address,
    ) {
        let token_metadata = listing::token_metadata(object);

        let price = fungible_asset::amount(payment);
        let (royalty_addr, royalty_charge) = listing::compute_royalty(object, price);
        let (seller, fee_schedule) = listing::close(completer, object, purchaser_addr);

        // Royalty first so creators are always honoured in full.
        if (royalty_charge != 0) {
            let royalty = fungible_asset::extract(payment, royalty_charge);
            primary_fungible_store::deposit(royalty_addr, royalty);
        };

        // Commission of what's left, floored at the quote token's min_fee.
        // The seller's wallet discount (if any) already shaped this number.
        let commission_charge = fee_schedule::commission_for(
            fee_schedule,
            quote_addr,
            seller,
            events::token_collection_address(&token_metadata),
            price,
        );
        assert!(
            royalty_charge + commission_charge <= price,
            error::invalid_state(EFEE_EXCEEDS_PRICE),
        );
        if (commission_charge != 0) {
            let commission = fungible_asset::extract(payment, commission_charge);
            primary_fungible_store::deposit(fee_schedule::fee_address(fee_schedule), commission);
        };

        // Seller gets what is left.
        let remainder = fungible_asset::amount(payment);
        let seller_funds = fungible_asset::extract(payment, remainder);
        primary_fungible_store::deposit(seller, seller_funds);

        events::emit_listing_filled(
            fee_schedule,
            string::utf8(FIXED_PRICE_TYPE),
            object::object_address(&object),
            seller,
            purchaser_addr,
            price,
            commission_charge,
            royalty_charge,
            quote_addr,
            commission_charge,
            token_metadata,
        );
    }

    // View

    #[view]
    public fun price(object: Object<Listing>): Option<u64> acquires FixedPriceListing {
        let listing_addr = object::object_address(&object);
        if (exists<FixedPriceListing>(listing_addr)) {
            option::some(borrow_global<FixedPriceListing>(listing_addr).price)
        } else {
            option::none()
        }
    }

    #[view]
    public fun quote(object: Object<Listing>): Option<Object<Metadata>> acquires FixedPriceListing {
        let listing_addr = object::object_address(&object);
        if (exists<FixedPriceListing>(listing_addr)) {
            option::some(borrow_global<FixedPriceListing>(listing_addr).quote)
        } else {
            option::none()
        }
    }
}
