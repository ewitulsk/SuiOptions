// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {ChainId} from "../src/libraries/ChainId.sol";

contract ChainIdTest is Test {
    function test_encode_round_trips() public pure {
        uint32 hyper = ChainId.encode(ChainId.FAMILY_EVM, 998);
        uint32 sui = ChainId.encode(ChainId.FAMILY_SUI, 0);

        // Matches the Move/Sui constants exactly.
        assertEq(hyper, 268436454);
        assertEq(sui, 134217728);

        assertEq(ChainId.family(hyper), ChainId.FAMILY_EVM);
        assertEq(uint256(ChainId.local(hyper)), 998);
        assertEq(ChainId.family(sui), ChainId.FAMILY_SUI);
        assertEq(uint256(ChainId.local(sui)), 0);
    }

    function test_isValidFamily() public pure {
        assertTrue(ChainId.isValidFamily(ChainId.FAMILY_SUI));
        assertTrue(ChainId.isValidFamily(ChainId.FAMILY_APTOS));
        assertFalse(ChainId.isValidFamily(0));
        assertFalse(ChainId.isValidFamily(5));
    }

    function test_encode_rejects_oversized_local() public {
        vm.expectRevert(
            abi.encodeWithSelector(ChainId.LocalTooLarge.selector, ChainId.LOCAL_MASK + 1)
        );
        this.encodeExt(ChainId.FAMILY_EVM, ChainId.LOCAL_MASK + 1);
    }

    function test_encode_rejects_bad_family() public {
        vm.expectRevert(abi.encodeWithSelector(ChainId.UnknownFamily.selector, uint8(9)));
        this.encodeExt(9, 1);
    }

    /// External wrapper so `vm.expectRevert` sees a call boundary.
    function encodeExt(uint8 family, uint32 local) external pure returns (uint32) {
        return ChainId.encode(family, local);
    }
}
