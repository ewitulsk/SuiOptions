module options_protocol::call_option;

use options_protocol::errors;

public struct CallOption has key, store {
    id: UID,
    bucket_id: ID,
    amount: u64,
}

public(package) fun mint(bucket_id: ID, amount: u64, ctx: &mut TxContext): CallOption {
    CallOption { id: object::new(ctx), bucket_id, amount }
}

public(package) fun burn(option: CallOption): u64 {
    let CallOption { id, bucket_id: _, amount } = option;
    id.delete();
    amount
}

public fun split(option: &mut CallOption, amount: u64, ctx: &mut TxContext): CallOption {
    assert!(amount > 0, errors::zero_amount());
    assert!(option.amount >= amount, errors::amount_mismatch());
    option.amount = option.amount - amount;
    CallOption { id: object::new(ctx), bucket_id: option.bucket_id, amount }
}

public fun join(option: &mut CallOption, other: CallOption) {
    assert!(option.bucket_id == other.bucket_id, errors::call_option_bucket_mismatch());
    let CallOption { id, bucket_id: _, amount } = other;
    id.delete();
    option.amount = option.amount + amount;
}

public fun bucket_id(option: &CallOption): ID { option.bucket_id }

public fun amount(option: &CallOption): u64 { option.amount }
