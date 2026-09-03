/// Vector-of-address convenience entries: bulk list / unlist / purchase.
module marketplace::marketplace_scripts {
    use std::string::String;
    use std::vector;

    use aptos_framework::fungible_asset::Metadata;
    use aptos_framework::object::{Object, ObjectCore};

    use marketplace::fa_listing;
    use marketplace::listing::Listing;
    use marketplace::fee_schedule::FeeSchedule;

    /// List many v2 tokens in one transaction.
    public entry fun bulk_list_v2(
        seller: &signer,
        objects: vector<Object<ObjectCore>>,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        start_time: u64,
        prices: vector<u64>,
    ) {
        fa_listing::init_fixed_price_many(seller, objects, fee_schedule, quote, start_time, prices);
    }

    /// List many TokenV1 tokens in one transaction.
    public entry fun bulk_list_v1(
        seller: &signer,
        token_creator: address,
        token_collection: String,
        token_names: vector<String>,
        token_property_versions: vector<u64>,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        start_time: u64,
        prices: vector<u64>,
    ) {
        let n = vector::length(&token_names);
        assert!(n == vector::length(&token_property_versions), 0);
        assert!(n == vector::length(&prices), 0);
        let i = 0;
        while (i < n) {
            fa_listing::init_fixed_price_for_tokenv1(
                seller,
                token_creator,
                token_collection,
                *vector::borrow(&token_names, i),
                *vector::borrow(&token_property_versions, i),
                fee_schedule,
                quote,
                start_time,
                *vector::borrow(&prices, i),
            );
            i = i + 1;
        };
    }

    /// End many listings in one transaction (bulk rescue / delist).
    public entry fun bulk_unlist(
        seller: &signer,
        listings: vector<Object<Listing>>,
    ) {
        let i = 0;
        let n = vector::length(&listings);
        while (i < n) {
            fa_listing::end_fixed_price(seller, *vector::borrow(&listings, i));
            i = i + 1;
        };
    }

    /// Buy many listings in one transaction (sweep).
    public entry fun bulk_purchase(
        purchaser: &signer,
        listings: vector<Object<Listing>>,
    ) {
        fa_listing::purchase_many(purchaser, listings);
    }
}
