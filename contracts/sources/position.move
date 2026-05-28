module options_protocol::position;

public struct Position has key, store {
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
): Position {
    Position { id: object::new(ctx), bucket_id, range_start, range_end }
}

public(package) fun burn(position: Position): (ID, ID, u128, u128) {
    let position_id = object::id(&position);
    let Position { id, bucket_id, range_start, range_end } = position;
    id.delete();
    (position_id, bucket_id, range_start, range_end)
}

public fun bucket_id(p: &Position): ID { p.bucket_id }

public fun range_start(p: &Position): u128 { p.range_start }

public fun range_end(p: &Position): u128 { p.range_end }

public fun amount(p: &Position): u128 { p.range_end - p.range_start }
