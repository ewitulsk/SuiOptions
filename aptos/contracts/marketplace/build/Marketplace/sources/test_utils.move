#[test_only]
module marketplace::test_utils {
    use std::signer;
    use std::string;
    use std::vector;

    use std::option;

    use aptos_framework::account;
    use aptos_framework::fungible_asset::{Self, Metadata};
    use aptos_framework::object::{Self, Object};
    use aptos_framework::primary_fungible_store;
    use aptos_framework::timestamp;

    use aptos_token::token as tokenv1;
    use aptos_token_objects::token::Token;
    use aptos_token_objects::aptos_token;
    use aptos_token_objects::collection::Collection;

    use marketplace::admin::AdminCap;
    use marketplace::fee_schedule::{Self, FeeSchedule};

    /// Test setup funded in a managed FA quote token: creates accounts,
    /// mints 10_000 units to seller and purchaser, and returns the metadata.
    public inline fun setup(
        aptos_framework: &signer,
        marketplace: &signer,
        seller: &signer,
        purchaser: &signer,
    ): (address, address, address, Object<Metadata>) {
        timestamp::set_time_has_started_for_testing(aptos_framework);

        let marketplace_addr = signer::address_of(marketplace);
        account::create_account_for_test(marketplace_addr);

        let seller_addr = signer::address_of(seller);
        account::create_account_for_test(seller_addr);

        let purchaser_addr = signer::address_of(purchaser);
        account::create_account_for_test(purchaser_addr);

        let (creator_ref, _token_object) = fungible_asset::create_test_token(seller);
        primary_fungible_store::create_primary_store_enabled_fungible_asset(
            &creator_ref,
            option::some(1000000),
            string::utf8(b"Test Quote"),
            string::utf8(b"TQ"),
            8,
            string::utf8(b""),
            string::utf8(b""),
        );
        let mint_ref = fungible_asset::generate_mint_ref(&creator_ref);
        let quote = fungible_asset::mint_ref_metadata(&mint_ref);
        primary_fungible_store::mint(&mint_ref, seller_addr, 10000);
        primary_fungible_store::mint(&mint_ref, purchaser_addr, 10000);

        (marketplace_addr, seller_addr, purchaser_addr, quote)
    }

    public fun balance(addr: address, quote: Object<Metadata>): u64 {
        primary_fungible_store::balance(addr, quote)
    }

    /// 1% commission schedule with the test quote allowlisted at zero min fee.
    /// Returns the transferable admin cap alongside the schedule.
    public fun fee_schedule(
        admin: &signer,
        quote: Object<Metadata>,
    ): (Object<AdminCap>, Object<FeeSchedule>) {
        let (cap, schedule) = fee_schedule::init(
            admin,
            signer::address_of(admin),
            0,
            0,
            100,
            1,
        );
        fee_schedule::allow_quote_token(admin, cap, schedule, quote, 0);
        (cap, schedule)
    }

    public inline fun increment_timestamp(seconds: u64) {
        timestamp::update_global_time_for_test(timestamp::now_microseconds() + (seconds * 1000000));
    }

    public fun mint_tokenv2_with_collection(seller: &signer): (Object<Collection>, Object<Token>) {
        let collection_name = string::utf8(b"collection_name");

        let collection_object = aptos_token::create_collection_object(
            seller,
            string::utf8(b"collection description"),
            2,
            collection_name,
            string::utf8(b"collection uri"),
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            1,
            100,
        );

        let aptos_token = aptos_token::mint_token_object(
            seller,
            collection_name,
            string::utf8(b"description"),
            string::utf8(b"token_name"),
            string::utf8(b"uri"),
            vector::empty(),
            vector::empty(),
            vector::empty(),
        );
        (object::convert(collection_object), object::convert(aptos_token))
    }

    public fun mint_tokenv2_with_collection_royalty(
        seller: &signer,
        royalty_numerator: u64,
        royalty_denominator: u64
    ): (Object<Collection>, Object<Token>) {
        let collection_name = string::utf8(b"collection_name");

        let collection_object = aptos_token::create_collection_object(
            seller,
            string::utf8(b"collection description"),
            2,
            collection_name,
            string::utf8(b"collection uri"),
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            royalty_numerator,
            royalty_denominator,
        );

        let aptos_token = aptos_token::mint_token_object(
            seller,
            collection_name,
            string::utf8(b"description"),
            string::utf8(b"token_name"),
            string::utf8(b"uri"),
            vector::empty(),
            vector::empty(),
            vector::empty(),
        );
        (object::convert(collection_object), object::convert(aptos_token))
    }

    public fun mint_tokenv2(seller: &signer): Object<Token> {
        let (_collection, token) = mint_tokenv2_with_collection(seller);
        token
    }

    public fun mint_tokenv2_additional(seller: &signer): Object<Token> {
        let collection_name = string::utf8(b"collection_name");

        let aptos_token = aptos_token::mint_token_object(
            seller,
            collection_name,
            string::utf8(b"description"),
            string::utf8(b"token_name_2"),
            string::utf8(b"uri"),
            vector::empty(),
            vector::empty(),
            vector::empty(),
        );
        object::convert(aptos_token)
    }

    public fun mint_tokenv1(seller: &signer): tokenv1::TokenId {
        let collection_name = string::utf8(b"collection_name");
        let token_name = string::utf8(b"token_name");

        tokenv1::create_collection(
            seller,
            collection_name,
            string::utf8(b"Collection: Hello, World"),
            string::utf8(b"https://aptos.dev"),
            2,
            vector[true, true, true],
        );

        tokenv1::create_token_script(
            seller,
            collection_name,
            token_name,
            string::utf8(b"Hello, Token"),
            1,
            1,
            string::utf8(b"https://aptos.dev"),
            signer::address_of(seller),
            100,
            1,
            vector[true, true, true, true, true],
            vector::empty(),
            vector::empty(),
            vector::empty(),
        );

        tokenv1::create_token_id_raw(
            signer::address_of(seller),
            collection_name,
            token_name,
            0,
        )
    }

    public fun mint_tokenv1_additional(seller: &signer): tokenv1::TokenId {
        let collection_name = string::utf8(b"collection_name");
        let token_name = string::utf8(b"token_name_2");
        tokenv1::create_token_script(
            seller,
            collection_name,
            token_name,
            string::utf8(b"Hello, Token"),
            1,
            1,
            string::utf8(b"https://aptos.dev"),
            signer::address_of(seller),
            100,
            1,
            vector[true, true, true, true, true],
            vector::empty(),
            vector::empty(),
            vector::empty(),
        );

        tokenv1::create_token_id_raw(
            signer::address_of(seller),
            collection_name,
            token_name,
            0,
        )
    }

    public fun mint_tokenv1_additional_royalty(
        seller: &signer,
        royalty_numerator: u64,
        royalty_denominator: u64
    ): tokenv1::TokenId {
        let collection_name = string::utf8(b"collection_name");
        let token_name = string::utf8(b"token_name_2");
        tokenv1::create_token_script(
            seller,
            collection_name,
            token_name,
            string::utf8(b"Hello, Token"),
            1,
            1,
            string::utf8(b"https://aptos.dev"),
            signer::address_of(seller),
            royalty_denominator,
            royalty_numerator,
            vector[true, true, true, true, true],
            vector::empty(),
            vector::empty(),
            vector::empty(),
        );

        tokenv1::create_token_id_raw(
            signer::address_of(seller),
            collection_name,
            token_name,
            0,
        )
    }
}
