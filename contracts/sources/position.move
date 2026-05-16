module options_protocol::position;

public struct PositionNFT has key, store {
    id: UID,
    bucket_id: ID,
    range_start: u128,
    range_end: u128,
}

public(package) fun mint(
    bucket_id: ID,
    range_start: u128,
    range_end: u128,
    ctx: &mut TxContext,
): PositionNFT {
    PositionNFT { id: object::new(ctx), bucket_id, range_start, range_end }
}

public(package) fun burn(position: PositionNFT): (ID, ID, u128, u128) {
    let position_id = object::id(&position);
    let PositionNFT { id, bucket_id, range_start, range_end } = position;
    id.delete();
    (position_id, bucket_id, range_start, range_end)
}

public fun bucket_id(p: &PositionNFT): ID { p.bucket_id }

public fun range_start(p: &PositionNFT): u128 { p.range_start }

public fun range_end(p: &PositionNFT): u128 { p.range_end }

public fun amount(p: &PositionNFT): u128 { p.range_end - p.range_start }
