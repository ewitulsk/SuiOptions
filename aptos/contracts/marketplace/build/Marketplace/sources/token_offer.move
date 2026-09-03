address marketplace {
/// Token offers priced in any allowlisted fungible asset.
///
/// A token offer lets an entity bid on a specific token at any time. The
/// full price is withdrawn from the bidder up front into an object-owned
/// escrow (`EscrowedFunds`, a plain `FungibleAsset` — no type parameter).
/// A seller then exchanges the token for the escrowed payment. Expired
/// offers cannot be filled but can be cancelled by anyone (refund goes to
/// the bidder); the bidder can cancel at any time.
module token_offer {
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

    use marketplace::events;
    use marketplace::fee_schedule::{Self, FeeSchedule};
    use marketplace::listing::{Self, TokenV1Container};
    use aptos_token::token::TokenId;

    /// No token offer defined.
    const ENO_TOKEN_OFFER: u64 = 1;
    /// Royalty plus commission exceed the price.
    const EFEE_EXCEEDS_PRICE: u64 = 5;
    /// The token offer has expired.
    const EEXPIRED: u64 = 6;
    /// This is not the owner of the token offer.
    const ENOT_OWNER: u64 = 4;
    /// This is not the owner of the token.
    const ENOT_TOKEN_OWNER: u64 = 3;
    /// The offer is not expired yet.
    const ENOT_EXPIRED: u64 = 7;

    // Core data structures

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    /// A timed offer to buy a token, with payment held in escrow.
    struct TokenOffer has key {
        fee_schedule: Object<FeeSchedule>,
        quote: address,
        item_price: u64,
        expiration_time: u64,
        delete_ref: DeleteRef,
    }

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    /// Escrowed payment for a token offer, in the offer's quote token. Funds
    /// live in the offer object's primary store; `extend_ref` authorises
    /// payouts and `amount` tracks the escrowed total.
    struct EscrowedFunds has key {
        extend_ref: ExtendRef,
        amount: u64,
    }

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    /// Stores the metadata associated with a tokenv1 token offer.
    struct TokenOfferTokenV1 has copy, drop, key {
        creator_address: address,
        collection_name: String,
        token_name: String,
        property_version: u64,
    }

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    /// Stores the metadata associated with a tokenv2 token offer.
    struct TokenOfferTokenV2 has copy, drop, key {
        token: Object<TokenV2>,
    }

    // Initializers

    /// Create a tokenv1 token offer.
    public entry fun init_for_tokenv1_entry(
        purchaser: &signer,
        creator_address: address,
        collection_name: String,
        token_name: String,
        property_version: u64,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        item_price: u64,
        expiration_time: u64,
    ) {
        init_for_tokenv1(
            purchaser,
            creator_address,
            collection_name,
            token_name,
            property_version,
            fee_schedule,
            quote,
            item_price,
            expiration_time
        );
    }

    public fun init_for_tokenv1(
        purchaser: &signer,
        creator_address: address,
        collection_name: String,
        token_name: String,
        property_version: u64,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        item_price: u64,
        expiration_time: u64,
    ): Object<TokenOffer> {
        let offer_signer = init_offer(purchaser, fee_schedule, quote, item_price, expiration_time);
        move_to(&offer_signer, TokenOfferTokenV1 { creator_address, collection_name, token_name, property_version });

        let token_id = tokenv1::create_token_id(
            tokenv1::create_token_data_id(creator_address, collection_name, token_name),
            property_version
        );
        let token_offer_addr = signer::address_of(&offer_signer);
        events::emit_token_offer_placed(
            fee_schedule,
            token_offer_addr,
            signer::address_of(purchaser),
            item_price,
            object::object_address(&quote),
            fee_schedule::bidding_fee(fee_schedule, item_price),
            events::token_metadata_for_tokenv1(token_id),
        );

        object::address_to_object(token_offer_addr)
    }

    /// Create a tokenv2 token offer.
    public entry fun init_for_tokenv2_entry(
        purchaser: &signer,
        token: Object<TokenV2>,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        item_price: u64,
        expiration_time: u64,
    ) {
        init_for_tokenv2(
            purchaser,
            token,
            fee_schedule,
            quote,
            item_price,
            expiration_time
        );
    }

    public fun init_for_tokenv2(
        purchaser: &signer,
        token: Object<TokenV2>,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        item_price: u64,
        expiration_time: u64,
    ): Object<TokenOffer> {
        let offer_signer = init_offer(purchaser, fee_schedule, quote, item_price, expiration_time);
        move_to(&offer_signer, TokenOfferTokenV2 { token });

        let token_offer_addr = signer::address_of(&offer_signer);
        events::emit_token_offer_placed(
            fee_schedule,
            token_offer_addr,
            signer::address_of(purchaser),
            item_price,
            object::object_address(&quote),
            fee_schedule::bidding_fee(fee_schedule, item_price),
            events::token_metadata_for_tokenv2(token),
        );

        object::address_to_object(token_offer_addr)
    }

    inline fun init_offer(
        purchaser: &signer,
        fee_schedule: Object<FeeSchedule>,
        quote: Object<Metadata>,
        item_price: u64,
        expiration_time: u64,
    ): signer {
        fee_schedule::assert_quote_enabled(&fee_schedule, object::object_address(&quote));
        let constructor_ref = object::create_object_from_account(purchaser);
        // Once we construct this, both the listing and its contents are soulbound until the conclusion.
        let object_transfer_ref = object::generate_transfer_ref(&constructor_ref);
        object::disable_ungated_transfer(&object_transfer_ref);

        let offer_signer = object::generate_signer(&constructor_ref);
        move_to(&offer_signer, TokenOffer {
            fee_schedule,
            quote: object::object_address(&quote),
            item_price,
            expiration_time,
            delete_ref: object::generate_delete_ref(&constructor_ref),
        });

        let bidding_fee = fee_schedule::bidding_fee(fee_schedule, item_price);
        if (bidding_fee != 0) {
            let fee = primary_fungible_store::withdraw(purchaser, quote, bidding_fee);
            primary_fungible_store::deposit(fee_schedule::fee_address(fee_schedule), fee);
        };
        let offer_addr = object::address_from_constructor_ref(&constructor_ref);
        let payment = primary_fungible_store::withdraw(purchaser, quote, item_price);
        primary_fungible_store::deposit(offer_addr, payment);
        move_to(&offer_signer, EscrowedFunds {
            extend_ref: object::generate_extend_ref(&constructor_ref),
            amount: item_price,
        });

        offer_signer
    }

    // Mutators

    /// Cancel an offer and refund the escrow to the bidder. Bidder-only.
    public entry fun cancel(
        purchaser: &signer,
        token_offer: Object<TokenOffer>,
    ) acquires EscrowedFunds, TokenOffer, TokenOfferTokenV1, TokenOfferTokenV2 {
        let token_offer_addr = object::object_address(&token_offer);
        assert!(
            exists<TokenOffer>(token_offer_addr),
            error::not_found(ENO_TOKEN_OFFER),
        );
        assert!(
            object::is_owner(token_offer, signer::address_of(purchaser)),
            error::permission_denied(ENOT_OWNER),
        );
        let token_offer_obj = borrow_global_mut<TokenOffer>(token_offer_addr);
        let token_metadata = offer_token_metadata(token_offer_addr);
        let purchaser_addr = signer::address_of(purchaser);

        events::emit_token_offer_canceled(
            token_offer_obj.fee_schedule,
            token_offer_addr,
            purchaser_addr,
            token_offer_obj.item_price,
            token_offer_obj.quote,
            0,
            token_metadata,
        );

        refund_and_cleanup(purchaser_addr, token_offer);
    }

    /// Cancel an expired offer. Anyone can call; the refund goes to the bidder.
    public entry fun cancel_expired(
        anyone: &signer,
        token_offer: Object<TokenOffer>,
    ) acquires EscrowedFunds, TokenOffer, TokenOfferTokenV1, TokenOfferTokenV2 {
        let _ = anyone;
        let token_offer_addr = object::object_address(&token_offer);
        assert!(
            exists<TokenOffer>(token_offer_addr),
            error::not_found(ENO_TOKEN_OFFER),
        );
        assert!(
            timestamp::now_seconds() >= borrow_global<TokenOffer>(token_offer_addr).expiration_time,
            error::invalid_state(ENOT_EXPIRED),
        );
        let token_offer_obj = borrow_global_mut<TokenOffer>(token_offer_addr);
        let token_metadata = offer_token_metadata(token_offer_addr);
        let bidder = object::owner(token_offer);

        events::emit_token_offer_canceled(
            token_offer_obj.fee_schedule,
            token_offer_addr,
            bidder,
            token_offer_obj.item_price,
            token_offer_obj.quote,
            0,
            token_metadata,
        );

        refund_and_cleanup(bidder, token_offer);
    }

    /// Sell a tokenv1 to a token offer.
    public entry fun sell_tokenv1_entry(
        seller: &signer,
        token_offer: Object<TokenOffer>,
        token_name: String,
        property_version: u64,
    ) acquires EscrowedFunds, TokenOffer, TokenOfferTokenV1, TokenOfferTokenV2
    {
        sell_tokenv1(seller, token_offer, token_name, property_version);
    }

    /// Sell a tokenv1 to a token offer.
    public fun sell_tokenv1(
        seller: &signer,
        token_offer: Object<TokenOffer>,
        token_name: String,
        property_version: u64,
    ): Option<Object<TokenV1Container>>
    acquires
    EscrowedFunds,
    TokenOffer,
    TokenOfferTokenV1,
    TokenOfferTokenV2
    {
        let token_offer_addr = object::object_address(&token_offer);
        assert!(
            exists<TokenOfferTokenV1>(token_offer_addr),
            error::not_found(ENO_TOKEN_OFFER),
        );
        let token_offer_tokenv1_offer =
            borrow_global_mut<TokenOfferTokenV1>(token_offer_addr);

        // Move the token to its destination

        let token_id = tokenv1::create_token_id_raw(
            token_offer_tokenv1_offer.creator_address,
            token_offer_tokenv1_offer.collection_name,
            token_name,
            property_version,
        );

        let token = tokenv1::withdraw_token(seller, token_id, 1);

        let recipient = object::owner(token_offer);
        let container = if (tokenv1::get_direct_transfer(recipient)) {
            tokenv1::direct_deposit_with_opt_in(recipient, token);
            option::none()
        } else {
            let container = listing::create_tokenv1_container_with_token(seller, token);
            object::transfer(seller, container, recipient);
            option::some(container)
        };

        // Pay fees

        let royalty = tokenv1::get_royalty(token_id);
        settle_payments(
            object::owner(token_offer),
            signer::address_of(seller),
            token_offer_addr,
            tokenv1::get_royalty_payee(&royalty),
            tokenv1::get_royalty_denominator(&royalty),
            tokenv1::get_royalty_numerator(&royalty),
            @0x0,
            events::token_metadata_for_tokenv1(token_id),
        );

        container
    }

    /// Sell a tokenv2 to a token offer.
    public entry fun sell_tokenv2(
        seller: &signer,
        token_offer: Object<TokenOffer>,
    ) acquires EscrowedFunds, TokenOffer, TokenOfferTokenV1, TokenOfferTokenV2 {
        let token_offer_addr = object::object_address(&token_offer);
        assert!(
            exists<TokenOfferTokenV2>(token_offer_addr),
            error::not_found(ENO_TOKEN_OFFER),
        );

        // Check it's the correct token
        let seller_address = signer::address_of(seller);
        let token = borrow_global<TokenOfferTokenV2>(token_offer_addr).token;
        assert!(seller_address == object::owner(token), error::permission_denied(ENOT_TOKEN_OWNER));

        // Move the token to its destination
        let recipient = object::owner(token_offer);
        object::transfer(seller, token, recipient);

        // Pay fees

        let royalty = tokenv2::royalty(token);
        let (royalty_payee, royalty_denominator, royalty_numerator) = if (option::is_some(&royalty)) {
            let royalty = option::destroy_some(royalty);
            let payee_address = royalty::payee_address(&royalty);
            let denominator = royalty::denominator(&royalty);
            let numerator = royalty::numerator(&royalty);
            (payee_address, denominator, numerator)
        } else {
            (signer::address_of(seller), 1, 0)
        };

        settle_payments(
            object::owner(token_offer),
            seller_address,
            token_offer_addr,
            royalty_payee,
            royalty_denominator,
            royalty_numerator,
            object::object_address(&tokenv2::collection_object(token)),
            events::token_metadata_for_tokenv2(token),
        );
    }

    /// From the escrow remove appropriate payment for the token and distribute
    /// to the seller, the creator for royalties, and the marketplace for
    /// commission. Destroys the offer.
    inline fun settle_payments(
        buyer: address,
        seller: address,
        token_offer_addr: address,
        royalty_payee: address,
        royalty_denominator: u64,
        royalty_numerator: u64,
        collection: address,
        token_metadata: events::TokenMetadata,
    ) {
        assert!(exists<TokenOffer>(token_offer_addr), error::not_found(ENO_TOKEN_OFFER));
        let token_offer_obj = borrow_global_mut<TokenOffer>(token_offer_addr);
        assert!(
            timestamp::now_seconds() < token_offer_obj.expiration_time,
            error::invalid_state(EEXPIRED),
        );
        let price = token_offer_obj.item_price;
        let quote = token_offer_obj.quote;
        let fee_schedule = token_offer_obj.fee_schedule;

        assert!(exists<EscrowedFunds>(token_offer_addr), error::not_found(ENO_TOKEN_OFFER));
        let escrow = borrow_global<EscrowedFunds>(token_offer_addr);
        let payment = escrow_withdraw(escrow, object::address_to_object(quote), price);

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

        events::emit_token_offer_filled(
            fee_schedule,
            token_offer_addr,
            buyer,
            seller,
            price,
            royalty_charge,
            commission_charge,
            quote,
            commission_charge,
            token_metadata,
        );

        destroy_offer(object::address_to_object(token_offer_addr));
    }

    /// Refund the full escrow to `bidder` and destroy the offer.
    inline fun refund_and_cleanup(
        bidder: address,
        token_offer: Object<TokenOffer>,
    ) {
        let token_offer_addr = object::object_address(&token_offer);
        let quote = borrow_global<TokenOffer>(token_offer_addr).quote;
        let escrow = borrow_global<EscrowedFunds>(token_offer_addr);
        let refund = escrow_withdraw(escrow, object::address_to_object(quote), escrow.amount);
        primary_fungible_store::deposit(bidder, refund);
        destroy_offer(token_offer);
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
        token_offer: Object<TokenOffer>,
    ) {
        let token_offer_addr = object::object_address(&token_offer);
        let EscrowedFunds { extend_ref: _, amount: _ } = move_from(token_offer_addr);
        let TokenOffer {
            fee_schedule: _,
            quote: _,
            item_price: _,
            expiration_time: _,
            delete_ref,
        } = move_from(token_offer_addr);
        // Escrow was fully withdrawn (fills pay exactly, refunds all);
        // the empty primary store needs no cleanup.
        object::delete(delete_ref);

        if (exists<TokenOfferTokenV2>(token_offer_addr)) {
            move_from<TokenOfferTokenV2>(token_offer_addr);
        } else if (exists<TokenOfferTokenV1>(token_offer_addr)) {
            move_from<TokenOfferTokenV1>(token_offer_addr);
        };
    }

    inline fun offer_token_metadata(token_offer_addr: address): events::TokenMetadata
    {
        if (exists<TokenOfferTokenV2>(token_offer_addr)) {
            events::token_metadata_for_tokenv2(
                borrow_global<TokenOfferTokenV2>(token_offer_addr).token,
            )
        } else {
            let offer_info = borrow_global<TokenOfferTokenV1>(token_offer_addr);
            events::token_metadata_for_tokenv1(
                token_v1_token_id(offer_info)
            )
        }
    }

    // View

    #[view]
    public fun exists_at(token_offer: Object<TokenOffer>): bool {
        exists<TokenOffer>(object::object_address(&token_offer))
    }

    #[view]
    public fun expired(token_offer: Object<TokenOffer>): bool acquires TokenOffer {
        borrow_token_offer(token_offer).expiration_time <= timestamp::now_seconds()
    }

    #[view]
    public fun expiration_time(
        token_offer: Object<TokenOffer>,
    ): u64 acquires TokenOffer {
        borrow_token_offer(token_offer).expiration_time
    }

    #[view]
    public fun fee_schedule(
        token_offer: Object<TokenOffer>,
    ): Object<FeeSchedule> acquires TokenOffer {
        borrow_token_offer(token_offer).fee_schedule
    }

    #[view]
    public fun price(token_offer: Object<TokenOffer>): u64 acquires TokenOffer {
        borrow_token_offer(token_offer).item_price
    }

    #[view]
    public fun quote(token_offer: Object<TokenOffer>): address acquires TokenOffer {
        borrow_token_offer(token_offer).quote
    }

    #[view]
    public fun collectionv1(
        token_offer: Object<TokenOffer>,
    ): TokenOfferTokenV1 acquires TokenOfferTokenV1 {
        let token_offer_addr = object::object_address(&token_offer);
        assert!(
            exists<TokenOfferTokenV1>(token_offer_addr),
            error::not_found(ENO_TOKEN_OFFER),
        );
        *borrow_global(token_offer_addr)
    }

    #[view]
    public fun collectionv2(
        token_offer: Object<TokenOffer>,
    ): TokenOfferTokenV2 acquires TokenOfferTokenV2 {
        let token_offer_addr = object::object_address(&token_offer);
        assert!(
            exists<TokenOffer>(token_offer_addr),
            error::not_found(ENO_TOKEN_OFFER),
        );
        *borrow_global(token_offer_addr)
    }

    inline fun borrow_token_offer(
        token_offer: Object<TokenOffer>,
    ): &TokenOffer  {
        let token_offer_addr = object::object_address(&token_offer);
        assert!(
            exists<TokenOffer>(token_offer_addr),
            error::not_found(ENO_TOKEN_OFFER),
        );
        borrow_global(token_offer_addr)
    }

    inline fun token_v1_token_id(
        token_offer_tokenv1_offer: &TokenOfferTokenV1,
    ): TokenId {
        tokenv1::create_token_id_raw(
            token_offer_tokenv1_offer.creator_address,
            token_offer_tokenv1_offer.collection_name,
            token_offer_tokenv1_offer.token_name,
            token_offer_tokenv1_offer.property_version,
        )
    }
}
}
