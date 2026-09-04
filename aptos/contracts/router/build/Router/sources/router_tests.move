/// Router unit tests: fee math, venue gating, admin. The cross-venue
/// fill path itself is proven by mainnet canary buys (Phase 3 gate), since
/// foreign venue code does not exist in the unit-test environment.
#[test_only]
module router::router_tests {
    use std::signer;
    use std::vector;

    use aptos_framework::account;

    use router::admin;
    use router::router;

    #[test(admin = @0x111)]
    fun test_quote_fee_math(admin: &signer) {
        let admin_addr = signer::address_of(admin);
        account::create_account_for_test(admin_addr);
        // 2.5% with floor 10.
        let (config, _cap) = router::init_internal(admin, admin_addr, 250, 10, vector[1]);
        // total 1000 -> 25.
        assert!(router::quote_fee(config, 1000) == 25, 0);
        // total 100 -> 100*250/10000 = 2 -> floor 10.
        assert!(router::quote_fee(config, 100) == 10, 0);
        // zero total -> floor 10.
        assert!(router::quote_fee(config, 0) == 10, 0);
        assert!(router::venue_enabled(config, 1), 0);
        assert!(!router::venue_enabled(config, 2), 0);
    }

    #[test(admin = @0x111)]
    fun test_venue_gating_and_fee_update(admin: &signer) {
        let admin_addr = signer::address_of(admin);
        account::create_account_for_test(admin_addr);
        let (config, cap) = router::init_internal(admin, admin_addr, 0, 0, vector[]);
        assert!(!router::venue_enabled(config, 1), 0);
        router::set_venue_enabled(admin, cap, config, 1, true);
        assert!(router::venue_enabled(config, 1), 0);
        // Zero fee config quotes zero.
        assert!(router::quote_fee(config, 10_000_000) == 0, 0);
        router::set_fee(admin, cap, config, 100, 0);
        // 1% of 10_000_000 = 100_000.
        assert!(router::quote_fee(config, 10_000_000) == 100_000, 0);
    }

    #[test(admin = @0x111, stranger = @0x222)]
    #[expected_failure(abort_code = 0x50001, location = router::admin)]
    fun test_stranger_cannot_retune(admin: &signer, stranger: &signer) {
        let admin_addr = signer::address_of(admin);
        account::create_account_for_test(admin_addr);
        account::create_account_for_test(signer::address_of(stranger));
        let (config, cap) = router::init_internal(admin, admin_addr, 0, 0, vector[]);
        router::set_fee(stranger, cap, config, 100, 0);
    }

    #[test(admin = @0x111, multisig = @0x456)]
    fun test_cap_transfer_moves_admin(admin: &signer, multisig: &signer) {
        let admin_addr = signer::address_of(admin);
        let multisig_addr = signer::address_of(multisig);
        account::create_account_for_test(admin_addr);
        account::create_account_for_test(multisig_addr);
        let (config, cap) = router::init_internal(admin, admin_addr, 0, 0, vector[]);
        admin::transfer_admin_cap(admin, cap, multisig_addr);
        // Old admin's handle no longer authorises; multisig's does.
        router::set_fee(multisig, cap, config, 50, 1);
        assert!(router::quote_fee(config, 10_000) == 50, 0);
    }
}
