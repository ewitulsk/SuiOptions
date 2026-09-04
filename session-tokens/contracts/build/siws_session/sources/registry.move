/// Global identity map + replay-protection nonce set (spec §1.3). A single
/// shared object created at publish time via the module initializer.
///
/// `domain` is the registry object's own address — it is the cross-contract
/// replay guard baked into every signed message. `network` is the chain tag
/// rendered in the message; it must match what the SDK uses.
///
/// Contention note (spec §1.3): the nonce table is the only hot write path.
/// For higher throughput, shard the registry by `pk[0] % N` or move nonces
/// into the per-user `Account`. Kept global here for clarity.
module siws_session::registry;

use sui::table::{Self, Table};

public struct Registry has key {
    id: UID,
    /// replay/cross-contract domain separator (this object's address)
    domain: address,
    /// chain tag, e.g. b"testnet"
    network: vector<u8>,
    /// solana_pk -> Account object ID
    accounts: Table<vector<u8>, ID>,
    /// consumed nonces -> true
    nonces: Table<vector<u8>, bool>,
}

fun init(ctx: &mut TxContext) {
    let id = object::new(ctx);
    let domain = id.to_address();
    transfer::share_object(Registry {
        id,
        domain,
        network: b"testnet",
        accounts: table::new(ctx),
        nonces: table::new(ctx),
    });
}

// --- reads ---

public fun domain(self: &Registry): address { self.domain }

public fun network(self: &Registry): vector<u8> { self.network }

public fun nonce_used(self: &Registry, nonce: vector<u8>): bool {
    self.nonces.contains(nonce)
}

public fun has_account(self: &Registry, pk: vector<u8>): bool {
    self.accounts.contains(pk)
}

public fun account_id(self: &Registry, pk: vector<u8>): ID {
    *self.accounts.borrow(pk)
}

// --- writes (package-internal; only session.move calls these) ---

public(package) fun consume_nonce(self: &mut Registry, nonce: vector<u8>) {
    self.nonces.add(nonce, true);
}

public(package) fun register_account(self: &mut Registry, pk: vector<u8>, id: ID) {
    self.accounts.add(pk, id);
}

#[test_only]
public fun new_for_testing(network: vector<u8>, ctx: &mut TxContext): Registry {
    let id = object::new(ctx);
    let domain = id.to_address();
    Registry { id, domain, network, accounts: table::new(ctx), nonces: table::new(ctx) }
}
