address marketplace {
/// Collection offers priced in any allowlisted fungible asset.
///
/// A collection offer escrows `quantity * item_price` up front and fills up
/// to `quantity` times; each fill moves one matching token to the bidder and
/// pays royalty + commission + seller from one unit price. Expired offers
/// cannot be filled but can be cancelled by anyone (remainder refunded to the
/// bidder); the bidder can cancel at any time.
module collection_offer {
    use std::error;
    use std::option::{Self, Option};
    use std::signer;
    use std::string::String;

    use aptos_framework::fungible_asset::{Self, FungibleAsset, Metadata};
    use aptos_framework::object::{Self, DeleteRef, ExtendRef, Object};
    use aptos_framework::primary_fungible_store;
    use aptos_framework::timestamp;

    use aptos_token::token as tokenv1;

    use aptos_token_objects::royalty;
    use aptos_token_objects::token::{Self as tokenv2, Token as TokenV2};
    use aptos_token_objects::collection::Collection;

    use marketplace::events;
    use marketplace::fee_schedule::{Self, FeeSchedule};
    use marketplace::listing::{Self, TokenV1Container};

    /// No collection offer defined.
    const ENO_COLLECTION_OFFER: u64 = 1;
    /// This is not the owner of the collection offer.
    const ENOT_OWNER: u64 = 2;
    /// This is not the owner of the token.
    const ENOT_TOKEN_OWNER: u64 = 3;
    /// The token is not in the offered collection.
    const EWRONG_COLLECTION: u64 = 4;
    /// Royalty plus commission exceed the price.
    const EFEE_EXCEEDS_PRICE: u64 = 5;
    /// The collection offer has expired.
    const EEXPIRED: u64 = 6;
    /// The offer is not expired yet.
    const ENOT_EXPIRED: u64 = 7;
    /// Quantity must be at least one.
    const EZERO_QUANTITY: u64 = 8;

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    /// A standing offer to buy up to `remaining` tokens from a collection.
    struct CollectionOffer has key {
        fee_schedule: Object<FeeSchedule>,
        quote: address,
        item_price: u64,
        remaining: u64,
        expiration_time: u64,
        delete_ref: DeleteRef,
    }

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    /// Escrowed payment backing the unfilled quantity. Funds live in an
    /// object-owned fungible store; `transfer_ref` authorises payouts and
    /// `amount` tracks the escrowed total.
    struct EscrowedFunds has key {
        extend_ref: ExtendRef,
        amount: u64,
    }

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    struct CollectionOfferV1 has copy, drop, key {
        creator_address: address,
        collection_name: String,
    }

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    struct CollectionOfferV2 has copy, drop, key {
        collection: Object<Collection>,
    }

    // Initializers

    /// Offer on every token of a TokenV1 collection.
    public entry fun init_for_collection_v1_entry(
        purchaser: &signer,
        creator_address: address,
        collection_name: String,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        item_price: u64,
        quantity: u64,
        expiration_time: u64,
    ) {
        init_for_collection_v1(
            purchaser,
            creator_address,
            collection_name,
            fee_schedule,
            quote,
            item_price,
            quantity,
            expiration_time,
        );
    }

    public fun init_for_collection_v1(
        purchaser: &signer,
        creator_address: address,
        collection_name: String,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        item_price: u64,
        quantity: u64,
        expiration_time: u64,
    ): Object<CollectionOffer> {
        let offer_signer = init_offer(purchaser, fee_schedule, quote, item_price, quantity, expiration_time);
        move_to(&offer_signer, CollectionOfferV1 { creator_address, collection_name });

        let offer_addr = signer::address_of(&offer_signer);
        events::emit_collection_offer_placed(
            fee_schedule,
            offer_addr,
            signer::address_of(purchaser),
            item_price,
            quantity,
            object::object_address(&quote),
            fee_schedule::bidding_fee(fee_schedule, item_price),
            events::collection_metadata_for_tokenv1(creator_address, collection_name),
        );
        object::address_to_object(offer_addr)
    }

    /// Offer on every token of a TokenV2 collection.
    public entry fun init_for_collection_v2_entry(
        purchaser: &signer,
        collection: Object<Collection>,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        item_price: u64,
        quantity: u64,
        expiration_time: u64,
    ) {
        init_for_collection_v2(
            purchaser,
            collection,
            fee_schedule,
            quote,
            item_price,
            quantity,
            expiration_time,
        );
    }

    public fun init_for_collection_v2(
        purchaser: &signer,
        collection: Object<Collection>,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        item_price: u64,
        quantity: u64,
        expiration_time: u64,
    ): Object<CollectionOffer> {
        let offer_signer = init_offer(purchaser, fee_schedule, quote, item_price, quantity, expiration_time);
        move_to(&offer_signer, CollectionOfferV2 { collection });

        let offer_addr = signer::address_of(&offer_signer);
        events::emit_collection_offer_placed(
            fee_schedule,
            offer_addr,
            signer::address_of(purchaser),
            item_price,
            quantity,
            object::object_address(&quote),
            fee_schedule::bidding_fee(fee_schedule, item_price),
            events::collection_metadata_for_tokenv2(collection),
        );
        object::address_to_object(offer_addr)
    }

    inline fun init_offer(
        purchaser: &signer,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        item_price: u64,
        quantity: u64,
        expiration_time: u64,
    ): signer {
        assert!(quantity > 0, error::invalid_argument(EZERO_QUANTITY));
        fee_schedule::assert_quote_enabled(&fee_schedule, object::object_address(&quote));
        let constructor_ref = object::create_object_from_account(purchaser);
        let transfer_ref = object::generate_transfer_ref(&constructor_ref);
        object::disable_ungated_transfer(&transfer_ref);

        let offer_signer = object::generate_signer(&constructor_ref);
        move_to(&offer_signer, CollectionOffer {
            fee_schedule,
            quote: object::object_address(&quote),
            item_price,
            remaining: quantity,
            expiration_time,
            delete_ref: object::generate_delete_ref(&constructor_ref),
        });

        let bidding_fee = fee_schedule::bidding_fee(fee_schedule, item_price);
        if (bidding_fee != 0) {
            let fee = primary_fungible_store::withdraw(purchaser, quote, bidding_fee);
            primary_fungible_store::deposit(fee_schedule::fee_address(fee_schedule), fee);
        };
        let offer_addr = object::address_from_constructor_ref(&constructor_ref);
        let payment = primary_fungible_store::withdraw(purchaser, quote, item_price * quantity);
        primary_fungible_store::deposit(offer_addr, payment);
        move_to(&offer_signer, EscrowedFunds {
            extend_ref: object::generate_extend_ref(&constructor_ref),
            amount: item_price * quantity,
        });

        offer_signer
    }

    // Mutators

    /// Cancel and refund the unfilled remainder to the bidder. Bidder-only.
    public entry fun cancel(
        purchaser: &signer,
        offer: Object<CollectionOffer>,
    ) acquires CollectionOffer, CollectionOfferV1, CollectionOfferV2, EscrowedFunds {
        let offer_addr = object::object_address(&offer);
        assert!(exists<CollectionOffer>(offer_addr), error::not_found(ENO_COLLECTION_OFFER));
        assert!(
            object::is_owner(offer, signer::address_of(purchaser)),
            error::permission_denied(ENOT_OWNER),
        );
        let bidder = signer::address_of(purchaser);
        emit_canceled(offer_addr, bidder);
        refund_and_cleanup(bidder, offer);
    }

    /// Cancel an expired offer. Anyone can call; the remainder goes to the bidder.
    public entry fun cancel_expired(
        anyone: &signer,
        offer: Object<CollectionOffer>,
    ) acquires CollectionOffer, CollectionOfferV1, CollectionOfferV2, EscrowedFunds {
        let _ = anyone;
        let offer_addr = object::object_address(&offer);
        assert!(exists<CollectionOffer>(offer_addr), error::not_found(ENO_COLLECTION_OFFER));
        assert!(
            timestamp::now_seconds() >= borrow_global<CollectionOffer>(offer_addr).expiration_time,
            error::invalid_state(ENOT_EXPIRED),
        );
        let bidder = object::owner(offer);
        emit_canceled(offer_addr, bidder);
        refund_and_cleanup(bidder, offer);
    }

    /// Sell a TokenV1 token into a v1 collection offer.
    public entry fun fill_v1(
        seller: &signer,
        offer: Object<CollectionOffer>,
        token_name: String,
        property_version: u64,
    ) acquires CollectionOffer, CollectionOfferV1, CollectionOfferV2, EscrowedFunds {
        let offer_addr = object::object_address(&offer);
        assert!(exists<CollectionOfferV1>(offer_addr), error::not_found(ENO_COLLECTION_OFFER));
        let offer_info = borrow_global<CollectionOfferV1>(offer_addr);

        let token_id = tokenv1::create_token_id_raw(
            offer_info.creator_address,
            offer_info.collection_name,
            token_name,
            property_version,
        );
        let token = tokenv1::withdraw_token(seller, token_id, 1);

        let recipient = object::owner(offer);
        let container = if (tokenv1::get_direct_transfer(recipient)) {
            tokenv1::direct_deposit_with_opt_in(recipient, token);
            option::none()
        } else {
            let container = listing::create_tokenv1_container_with_token(seller, token);
            object::transfer(seller, container, recipient);
            option::some(container)
        };

        let royalty = tokenv1::get_royalty(token_id);
        settle_one_fill(
            recipient,
            signer::address_of(seller),
            offer_addr,
            tokenv1::get_royalty_payee(&royalty),
            tokenv1::get_royalty_denominator(&royalty),
            tokenv1::get_royalty_numerator(&royalty),
            @0x0,
            events::token_metadata_for_tokenv1(token_id),
        );
        if (option::is_some(&container)) {
            option::destroy_some(container);
        } else {
            option::destroy_none(container);
        };
    }

    /// Sell a TokenV2 token into a v2 collection offer.
    public entry fun fill_v2(
        seller: &signer,
        offer: Object<CollectionOffer>,
        token: Object<TokenV2>,
    ) acquires CollectionOffer, CollectionOfferV1, CollectionOfferV2, EscrowedFunds {
        let offer_addr = object::object_address(&offer);
        assert!(exists<CollectionOfferV2>(offer_addr), error::not_found(ENO_COLLECTION_OFFER));
        let seller_address = signer::address_of(seller);
        assert!(seller_address == object::owner(token), error::permission_denied(ENOT_TOKEN_OWNER));

        let want = borrow_global<CollectionOfferV2>(offer_addr).collection;
        let got = tokenv2::collection_object(token);
        assert!(object::object_address(&want) == object::object_address(&got), error::invalid_argument(EWRONG_COLLECTION));

        let recipient = object::owner(offer);
        object::transfer(seller, token, recipient);

        let royalty = tokenv2::royalty(token);
        let (royalty_payee, royalty_denominator, royalty_numerator) = if (option::is_some(&royalty)) {
            let royalty = option::destroy_some(royalty);
            (
                royalty::payee_address(&royalty),
                royalty::denominator(&royalty),
                royalty::numerator(&royalty),
            )
        } else {
            (seller_address, 1, 0)
        };

        settle_one_fill(
            recipient,
            seller_address,
            offer_addr,
            royalty_payee,
            royalty_denominator,
            royalty_numerator,
            object::object_address(&want),
            events::token_metadata_for_tokenv2(token),
        );
    }

    /// Pay out one unit price from escrow, decrement `remaining`, and destroy
    /// the offer once it hits zero.
    inline fun settle_one_fill(
        buyer: address,
        seller: address,
        offer_addr: address,
        royalty_payee: address,
        royalty_denominator: u64,
        royalty_numerator: u64,
        collection: address,
        token_metadata: events::TokenMetadata,
    ) {
        assert!(exists<CollectionOffer>(offer_addr), error::not_found(ENO_COLLECTION_OFFER));
        let offer = borrow_global_mut<CollectionOffer>(offer_addr);
        assert!(
            timestamp::now_seconds() < offer.expiration_time,
            error::invalid_state(EEXPIRED),
        );
        let price = offer.item_price;
        let quote = offer.quote;
        let fee_schedule = offer.fee_schedule;
        offer.remaining = offer.remaining - 1;
        let remaining = offer.remaining;

        assert!(exists<EscrowedFunds>(offer_addr), error::not_found(ENO_COLLECTION_OFFER));
        let escrow = borrow_global_mut<EscrowedFunds>(offer_addr);
        let payment = escrow_withdraw(escrow, object::address_to_object(quote), price);
        escrow.amount = escrow.amount - price;

        let royalty_charge = price * royalty_numerator / royalty_denominator;
        if (royalty_charge != 0) {
            let royalties = fungible_asset::extract(&mut payment, royalty_charge);
            primary_fungible_store::deposit(royalty_payee, royalties);
        };

        let commission_charge = fee_schedule::commission_for(
            fee_schedule,
            quote,
            seller,
            collection,
            price,
        );
        assert!(
            royalty_charge + commission_charge <= price,
            error::invalid_state(EFEE_EXCEEDS_PRICE),
        );
        if (commission_charge != 0) {
            let commission = fungible_asset::extract(&mut payment, commission_charge);
            primary_fungible_store::deposit(fee_schedule::fee_address(fee_schedule), commission);
        };

        let remainder = fungible_asset::amount(&payment);
        let seller_funds = fungible_asset::extract(&mut payment, remainder);
        primary_fungible_store::deposit(seller, seller_funds);
        fungible_asset::destroy_zero(payment);

        events::emit_collection_offer_filled(
            fee_schedule,
            offer_addr,
            buyer,
            seller,
            price,
            royalty_charge,
            commission_charge,
            quote,
            commission_charge,
            token_metadata,
        );

        if (remaining == 0) {
            destroy_offer(object::address_to_object(offer_addr));
        };
    }

    inline fun emit_canceled(offer_addr: address, bidder: address)
    {
        let offer = borrow_global<CollectionOffer>(offer_addr);
        if (exists<CollectionOfferV2>(offer_addr)) {
            let info = borrow_global<CollectionOfferV2>(offer_addr);
            events::emit_collection_offer_canceled(
                offer.fee_schedule,
                offer_addr,
                bidder,
                offer.item_price,
                offer.remaining,
                offer.quote,
                0,
                events::collection_metadata_for_tokenv2(info.collection),
            );
        } else {
            let info = borrow_global<CollectionOfferV1>(offer_addr);
            events::emit_collection_offer_canceled(
                offer.fee_schedule,
                offer_addr,
                bidder,
                offer.item_price,
                offer.remaining,
                offer.quote,
                0,
                events::collection_metadata_for_tokenv1(info.creator_address, info.collection_name),
            );
        };
    }

    inline fun refund_and_cleanup(
        bidder: address,
        offer: Object<CollectionOffer>,
    ) {
        let offer_addr = object::object_address(&offer);
        let quote = borrow_global<CollectionOffer>(offer_addr).quote;
        let escrow = borrow_global<EscrowedFunds>(offer_addr);
        let refund = escrow_withdraw(escrow, object::address_to_object(quote), escrow.amount);
        primary_fungible_store::deposit(bidder, refund);
        destroy_offer(offer);
    }

    /// Withdraw escrowed funds using the offer object's signer.
    inline fun escrow_withdraw(
        escrow: &EscrowedFunds,
        quote: Object<Metadata>,
        amount: u64,
    ): FungibleAsset {
        let offer_signer = object::generate_signer_for_extending(&escrow.extend_ref);
        primary_fungible_store::withdraw(&offer_signer, quote, amount)
    }

    inline fun destroy_offer(
        offer: Object<CollectionOffer>,
    ) {
        let offer_addr = object::object_address(&offer);
        let EscrowedFunds { extend_ref: _, amount: _ } = move_from(offer_addr);
        let CollectionOffer {
            fee_schedule: _,
            quote: _,
            item_price: _,
            remaining: _,
            expiration_time: _,
            delete_ref,
        } = move_from(offer_addr);
        // Escrow was fully withdrawn; the empty primary store needs no cleanup.
        object::delete(delete_ref);
        if (exists<CollectionOfferV2>(offer_addr)) {
            move_from<CollectionOfferV2>(offer_addr);
        } else if (exists<CollectionOfferV1>(offer_addr)) {
            move_from<CollectionOfferV1>(offer_addr);
        };
    }

    // View

    #[view]
    public fun exists_at(offer: Object<CollectionOffer>): bool {
        exists<CollectionOffer>(object::object_address(&offer))
    }

    #[view]
    public fun expired(offer: Object<CollectionOffer>): bool acquires CollectionOffer {
        borrow_offer(offer).expiration_time <= timestamp::now_seconds()
    }

    #[view]
    public fun expiration_time(offer: Object<CollectionOffer>): u64 acquires CollectionOffer {
        borrow_offer(offer).expiration_time
    }

    #[view]
    public fun fee_schedule(offer: Object<CollectionOffer>): Object<FeeSchedule> acquires CollectionOffer {
        borrow_offer(offer).fee_schedule
    }

    #[view]
    public fun price(offer: Object<CollectionOffer>): u64 acquires CollectionOffer {
        borrow_offer(offer).item_price
    }

    #[view]
    public fun quote(offer: Object<CollectionOffer>): address acquires CollectionOffer {
        borrow_offer(offer).quote
    }

    #[view]
    public fun remaining(offer: Object<CollectionOffer>): u64 acquires CollectionOffer {
        borrow_offer(offer).remaining
    }

    inline fun borrow_offer(offer: Object<CollectionOffer>): &CollectionOffer  {
        let offer_addr = object::object_address(&offer);
        assert!(exists<CollectionOffer>(offer_addr), error::not_found(ENO_COLLECTION_OFFER));
        borrow_global(offer_addr)
    }
}
}
