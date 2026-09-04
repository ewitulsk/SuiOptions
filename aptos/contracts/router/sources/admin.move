/// Transferable admin capability for the router. Same model as
/// `marketplace::admin`: moving admin to a multisig is one transfer.
module router::admin {
    use std::error;
    use std::signer;

    use aptos_framework::object::{Self, Object};

    const ENOT_ADMIN: u64 = 1;

    #[resource_group_member(group = aptos_framework::object::ObjectGroup)]
    struct AdminCap has key {}

    public fun create_admin_cap(creator: &signer): Object<AdminCap> {
        let constructor_ref = object::create_object_from_account(creator);
        let cap_signer = object::generate_signer(&constructor_ref);
        move_to(&cap_signer, AdminCap {});
        object::object_from_constructor_ref(&constructor_ref)
    }

    public entry fun transfer_admin_cap(
        owner: &signer,
        cap: Object<AdminCap>,
        to: address,
    ) {
        assert_admin(&cap, signer::address_of(owner));
        object::transfer(owner, cap, to);
    }

    public fun assert_admin(cap: &Object<AdminCap>, account: address) {
        let cap_addr = object::object_address(cap);
        assert!(exists<AdminCap>(cap_addr), error::not_found(ENOT_ADMIN));
        assert!(
            object::is_owner(*cap, account),
            error::permission_denied(ENOT_ADMIN),
        );
    }
}
