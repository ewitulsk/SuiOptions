/// Plan §3.1 invariant suite: exact payment splits, allowlist gating,
/// both token standards round-tripping, offer expiry/cancel semantics,
/// collection-offer quantity, and fee-schedule-mutation propagation.
#[test_only]
module marketplace::venue_tests {
    use std::option;
    use std::signer;
    use std::string;

    use aptos_framework::object;
    use aptos_framework::timestamp;
    use aptos_token::token as tokenv1;

    use marketplace::collection_offer;
    use marketplace::fa_listing;
    use marketplace::fee_schedule;
    use marketplace::listing;
    use marketplace::test_utils;
    use marketplace::token_offer;

    fun setup_v2(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
        royalty_num: u64,
        royalty_denom: u64,
    ): (address, address, address, object::Object<aptos_framework::fungible_asset::Metadata>, object::Object<marketplace::fee_schedule::FeeSchedule>, object::Object<aptos_token_objects::token::Token>) {
        let (marketplace_addr, seller_addr, purchaser_addr, quote) =
            test_utils::setup(aptos_framework, marketplace, seller, purchaser);
        let (_cap, schedule) = test_utils::fee_schedule(marketplace, quote);
        let (_collection, token) =
            test_utils::mint_tokenv2_with_collection_royalty(seller, royalty_num, royalty_denom);
        (marketplace_addr, seller_addr, purchaser_addr, quote, schedule, token)
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    fun test_fixed_price_v2_exact_split(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (marketplace_addr, seller_addr, purchaser_addr, quote, schedule, token) =
            setup_v2(aptos_framework, marketplace, seller, purchaser, 1, 100);
        // price 500, royalty 1% = 5, commission 1% = 5, seller 490.
        let listing = fa_listing::init_fixed_price_internal(
            seller,
            object::convert(token),
            schedule,
            quote,
            0,
            500,
        );
        assert!(fa_listing::price(listing) == option::some(500), 0);
        assert!(listing::seller(listing) == seller_addr, 0);

        fa_listing::purchase(purchaser, listing);

        assert!(object::owner(token) == purchaser_addr, 0);
        assert!(test_utils::balance(marketplace_addr, quote) == 5, 0);
        assert!(test_utils::balance(seller_addr, quote) == 10495, 0);
        assert!(test_utils::balance(purchaser_addr, quote) == 9500, 0);
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    fun test_fixed_price_v1_roundtrip(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (marketplace_addr, seller_addr, purchaser_addr, quote) =
            test_utils::setup(aptos_framework, marketplace, seller, purchaser);
        let (_cap, schedule) = test_utils::fee_schedule(marketplace, quote);
        // 1% royalty v1 token.
        let token_id = test_utils::mint_tokenv1(seller);
        let (creator_addr, collection_name, token_name, pv) =
            tokenv1::get_token_id_fields(&token_id);

        let listing = fa_listing::init_fixed_price_for_tokenv1_internal(
            seller,
            creator_addr,
            collection_name,
            token_name,
            pv,
            schedule,
            quote,
            0,
            500,
        );
        fa_listing::purchase(purchaser, listing);

        assert!(tokenv1::balance_of(purchaser_addr, token_id) == 1, 0);
        assert!(test_utils::balance(marketplace_addr, quote) == 5, 0);
        assert!(test_utils::balance(seller_addr, quote) == 10495, 0);
        assert!(test_utils::balance(purchaser_addr, quote) == 9500, 0);
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    fun test_end_fixed_price_returns_nft(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (_marketplace_addr, seller_addr, purchaser_addr, quote, schedule, token) =
            setup_v2(aptos_framework, marketplace, seller, purchaser, 1, 100);
        let listing = fa_listing::init_fixed_price_internal(
            seller,
            object::convert(token),
            schedule,
            quote,
            0,
            500,
        );
        fa_listing::end_fixed_price(seller, listing);
        assert!(object::owner(token) == seller_addr, 0);
        assert!(test_utils::balance(seller_addr, quote) == 10000, 0);
        assert!(test_utils::balance(purchaser_addr, quote) == 10000, 0);
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    fun test_update_fixed_price_and_sweep(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (_marketplace_addr, _seller_addr, purchaser_addr, quote, schedule, token) =
            setup_v2(aptos_framework, marketplace, seller, purchaser, 0, 1);
        let token2 = test_utils::mint_tokenv2_additional(seller);
        let l1 = fa_listing::init_fixed_price_internal(
            seller, object::convert(token), schedule, quote, 0, 500,
        );
        let l2 = fa_listing::init_fixed_price_internal(
            seller, object::convert(token2), schedule, quote, 0, 500,
        );
        fa_listing::update_fixed_price(seller, l1, 600);
        assert!(fa_listing::price(l1) == option::some(600), 0);
        // royalty 0, commission 1%: l1 -> 6 fee / 594 seller; l2 -> 5 / 495.
        fa_listing::purchase_many(purchaser, vector[l1, l2]);
        assert!(object::owner(token) == purchaser_addr, 0);
        assert!(object::owner(token2) == purchaser_addr, 0);
        assert!(test_utils::balance(purchaser_addr, quote) == 10000 - 1100, 0);
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    fun test_min_fee_floor_applies(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (marketplace_addr, seller_addr, purchaser_addr, quote) =
            test_utils::setup(aptos_framework, marketplace, seller, purchaser);
        let (cap, schedule) = fee_schedule::init(marketplace, signer::address_of(marketplace), 0, 0, 100, 1);
        fee_schedule::allow_quote_token(marketplace, cap, schedule, quote, 50);
        let (_collection, token) = test_utils::mint_tokenv2_with_collection_royalty(seller, 0, 1);
        // price 100, royalty 0, computed commission 1 -> floored to min_fee 50.
        let listing = fa_listing::init_fixed_price_internal(
            seller, object::convert(token), schedule, quote, 0, 100,
        );
        fa_listing::purchase(purchaser, listing);
        assert!(test_utils::balance(marketplace_addr, quote) == 50, 0);
        assert!(test_utils::balance(seller_addr, quote) == 10050, 0);
        assert!(test_utils::balance(purchaser_addr, quote) == 9900, 0);
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    #[expected_failure(abort_code = 0x30007, location = marketplace::fa_listing)]
    fun test_full_royalty_plus_fee_aborts(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (_marketplace_addr, _seller_addr, _purchaser_addr, quote, schedule, token) =
            setup_v2(aptos_framework, marketplace, seller, purchaser, 100, 100);
        let listing = fa_listing::init_fixed_price_internal(
            seller, object::convert(token), schedule, quote, 0, 500,
        );
        // royalty 500 + commission 5 > price 500.
        fa_listing::purchase(purchaser, listing);
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    #[expected_failure(abort_code = 0x10005, location = marketplace::fee_schedule)]
    fun test_disabled_quote_blocks_fill_but_not_cancel(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (_marketplace_addr, _seller_addr, _purchaser_addr, quote, schedule, token) =
            setup_v2(aptos_framework, marketplace, seller, purchaser, 1, 100);
        let listing = fa_listing::init_fixed_price_internal(
            seller, object::convert(token), schedule, quote, 0, 500,
        );
        // Disabling after listing exists blocks purchase ...
        let (cap, _) = test_utils::fee_schedule(marketplace, quote);
        fee_schedule::set_quote_enabled(marketplace, cap, schedule, object::object_address(&quote), false);
        fa_listing::purchase(purchaser, listing);
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    fun test_disabled_quote_still_cancels(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (_marketplace_addr, seller_addr, _purchaser_addr, quote, schedule, token) =
            setup_v2(aptos_framework, marketplace, seller, purchaser, 1, 100);
        let listing = fa_listing::init_fixed_price_internal(
            seller, object::convert(token), schedule, quote, 0, 500,
        );
        let (cap, _) = test_utils::fee_schedule(marketplace, quote);
        fee_schedule::set_quote_enabled(marketplace, cap, schedule, object::object_address(&quote), false);
        // ... but the seller can always get the NFT back.
        fa_listing::end_fixed_price(seller, listing);
        assert!(object::owner(token) == seller_addr, 0);
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    fun test_token_offer_v2_fill(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (marketplace_addr, seller_addr, purchaser_addr, quote, schedule, token) =
            setup_v2(aptos_framework, marketplace, seller, purchaser, 1, 100);
        let offer = token_offer::init_for_tokenv2(
            purchaser, token, schedule, quote, 500, timestamp::now_seconds() + 1000,
        );
        assert!(token_offer::price(offer) == 500, 0);
        assert!(!token_offer::expired(offer), 0);
        assert!(test_utils::balance(purchaser_addr, quote) == 9500, 0);

        token_offer::sell_tokenv2(seller, offer);

        assert!(object::owner(token) == purchaser_addr, 0);
        assert!(!token_offer::exists_at(offer), 0);
        assert!(test_utils::balance(marketplace_addr, quote) == 5, 0);
        assert!(test_utils::balance(seller_addr, quote) == 10495, 0);
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    fun test_token_offer_expired_cancelled_by_anyone(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (_marketplace_addr, _seller_addr, purchaser_addr, quote, schedule, token) =
            setup_v2(aptos_framework, marketplace, seller, purchaser, 1, 100);
        let offer = token_offer::init_for_tokenv2(
            purchaser, token, schedule, quote, 500, timestamp::now_seconds() + 100,
        );
        test_utils::increment_timestamp(100);
        assert!(token_offer::expired(offer), 0);
        // Anyone (here: the seller) can cancel; the bidder is refunded.
        token_offer::cancel_expired(seller, offer);
        assert!(!token_offer::exists_at(offer), 0);
        assert!(test_utils::balance(purchaser_addr, quote) == 10000, 0);
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    #[expected_failure(abort_code = 0x30006, location = marketplace::token_offer)]
    fun test_token_offer_expired_cannot_fill(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (_marketplace_addr, _seller_addr, _purchaser_addr, quote, schedule, token) =
            setup_v2(aptos_framework, marketplace, seller, purchaser, 1, 100);
        let offer = token_offer::init_for_tokenv2(
            purchaser, token, schedule, quote, 500, timestamp::now_seconds() + 100,
        );
        test_utils::increment_timestamp(100);
        token_offer::sell_tokenv2(seller, offer);
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    fun test_collection_offer_fills_quantity_then_closes(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (marketplace_addr, seller_addr, purchaser_addr, quote, schedule, token) =
            setup_v2(aptos_framework, marketplace, seller, purchaser, 1, 100);
        let token2 = test_utils::mint_tokenv2_additional(seller);
        let collection = aptos_token_objects::token::collection_object(token);
        let offer = collection_offer::init_for_collection_v2(
            purchaser, collection, schedule, quote, 500, 2, timestamp::now_seconds() + 1000,
        );
        assert!(collection_offer::remaining(offer) == 2, 0);
        assert!(test_utils::balance(purchaser_addr, quote) == 9000, 0);

        collection_offer::fill_v2(seller, offer, token);
        assert!(collection_offer::remaining(offer) == 1, 0);
        assert!(object::owner(token) == purchaser_addr, 0);

        collection_offer::fill_v2(seller, offer, token2);
        assert!(!collection_offer::exists_at(offer), 0);
        assert!(object::owner(token2) == purchaser_addr, 0);

        // Two fills: 2 x (royalty 5 + commission 5), seller +980.
        assert!(test_utils::balance(marketplace_addr, quote) == 10, 0);
        assert!(test_utils::balance(seller_addr, quote) == 10990, 0);
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    fun test_fee_mutation_applies_to_existing_listing(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (marketplace_addr, _seller_addr, _purchaser_addr, quote, schedule, token) =
            setup_v2(aptos_framework, marketplace, seller, purchaser, 0, 1);
        let listing = fa_listing::init_fixed_price_internal(
            seller, object::convert(token), schedule, quote, 0, 1000,
        );
        // Raise commission to 5% after listing; the fill pays the new rate.
        let (cap, _) = test_utils::fee_schedule(marketplace, quote);
        fee_schedule::set_percentage_rate_commission(marketplace, cap, schedule, 100, 5);
        fa_listing::purchase(purchaser, listing);
        assert!(test_utils::balance(marketplace_addr, quote) == 50, 0);
    }

    #[test(aptos_framework = @0x1, marketplace = @0x111, seller = @0x222, purchaser = @0x333)]
    #[expected_failure(abort_code = 0x30002, location = marketplace::listing)]
    fun test_not_started_listing(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ) {
        let (_marketplace_addr, _seller_addr, _purchaser_addr, quote, schedule, token) =
            setup_v2(aptos_framework, marketplace, seller, purchaser, 1, 100);
        let listing = fa_listing::init_fixed_price_internal(
            seller, object::convert(token), schedule, quote, timestamp::now_seconds() + 1, 500,
        );
        fa_listing::purchase(purchaser, listing);
    }
}
