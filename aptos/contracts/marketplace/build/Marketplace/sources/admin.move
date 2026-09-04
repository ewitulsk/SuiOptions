/// Transferable admin capability for the marketplace.
///
/// Every privileged operation (fee-schedule mutation, quote-allowlist
/// edits, collection overrides, wallet discounts) requires presenting an
/// `AdminCap` object owned by the transaction signer. The cap is a plain
/// transferable object: handing admin over to a multisig later is a single
/// `transfer_admin_cap` (or `object::transfer`) transaction, with no
/// contract upgrade.
module marketplace::admin {
    use std::error;
    use std::signer;

    use aptos_framework::object::{Self, Object};

    /// No such admin capability, or the caller does not own it.
    const ENOT_ADMIN: u64 = 1;

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    struct AdminCap has key {}

    /// Create a fresh admin capability object owned by `creator`.
    public fun create_admin_cap(creator: &signer): Object<AdminCap> {
        let constructor_ref = object::create_object_from_account(creator);
        let cap_signer = object::generate_signer(&constructor_ref);
        move_to(&cap_signer, AdminCap {});
        object::object_from_constructor_ref(&constructor_ref)
    }

    /// Transfer the admin capability to a new holder (EOA or multisig).
    public entry fun transfer_admin_cap(
        owner: &signer,
        cap: Object<AdminCap>,
        to: address,
    ) {
        assert_admin(&cap, signer::address_of(owner));
        object::transfer(owner, cap, to);
    }

    /// Abort unless `cap` exists and is owned by `account`.
    public fun assert_admin(cap: &Object<AdminCap>, account: address) {
        let cap_addr = object::object_address(cap);
        assert!(exists<AdminCap>(cap_addr), error::not_found(ENOT_ADMIN));
        assert!(
            object::is_owner(*cap, account),
            error::permission_denied(ENOT_ADMIN),
        );
    }

    #[test(creator = @0x123, multisig = @0x456)]
    fun test_transfer_admin_cap(creator: &signer, multisig: &signer) {
        use aptos_framework::account;
        let creator_addr = signer::address_of(creator);
        let multisig_addr = signer::address_of(multisig);
        account::create_account_for_test(creator_addr);
        account::create_account_for_test(multisig_addr);
        let cap = create_admin_cap(creator);
        assert_admin(&cap, creator_addr);
        transfer_admin_cap(creator, cap, multisig_addr);
        assert_admin(&cap, multisig_addr);
    }

    #[test(creator = @0x123, stranger = @0x999)]
    #[expected_failure(abort_code = 0x50001, location = Self)]
    fun test_assert_admin_wrong_owner(creator: &signer, stranger: &signer) {
        use aptos_framework::account;
        account::create_account_for_test(signer::address_of(creator));
        let cap = create_admin_cap(creator);
        assert_admin(&cap, signer::address_of(stranger));
    }
}
