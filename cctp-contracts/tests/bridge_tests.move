#[test_only]
module cctp_bridge::bridge_tests {
    use std::type_name;
    use sui::{
        coin,
        event::events_by_type,
        test_scenario,
        test_utils::assert_eq,
    };
    use cctp_bridge::bridge;

    public struct BRIDGE_TESTS has drop {}

    const USER: address = @0xA1;
    const AMOUNT: u64 = 100;
    const DESTINATION_DOMAIN: u32 = 5; // Solana
    const MINT_RECIPIENT: address = @0xB2;

    // Full setup of Circle's TokenMessengerMinter/MessageTransmitter state is
    // package-private on their side, so this exercises exactly what our module
    // does: emit BridgeInitiated and build the burn ticket. The composition
    // with Circle's deposit_for_burn_with_package_auth is covered by their own
    // package tests and verified on-chain.
    #[test]
    fun test_prepare_deposit_for_burn_emits_event_and_returns_ticket() {
        let mut scenario = test_scenario::begin(USER);
        {
            let coins = coin::mint_for_testing<BRIDGE_TESTS>(AMOUNT, scenario.ctx());

            let ticket = bridge::prepare_deposit_for_burn(
                coins,
                DESTINATION_DOMAIN,
                MINT_RECIPIENT,
                scenario.ctx(),
            );

            let events = events_by_type<bridge::BridgeInitiated>();
            assert_eq(events.length(), 1);
            assert_eq(
                events[0],
                bridge::bridge_initiated_for_testing(
                    USER,
                    AMOUNT,
                    DESTINATION_DOMAIN,
                    MINT_RECIPIENT,
                    type_name::with_defining_ids<BRIDGE_TESTS>().into_string(),
                ),
            );

            std::unit_test::destroy(ticket);
        };
        scenario.end();
    }
}
