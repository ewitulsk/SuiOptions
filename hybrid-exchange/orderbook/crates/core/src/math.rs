/// `floor(a * b / c)` with a u128 intermediate. Panics if `c == 0` or the
/// result overflows u64 (callers guarantee `a <= c`-scale invariants).
pub fn muldiv_floor(a: u64, b: u64, c: u64) -> u64 {
    ((a as u128 * b as u128) / c as u128) as u64
}

/// `ceil(a * b / c)` with a u128 intermediate.
pub fn muldiv_ceil(a: u64, b: u64, c: u64) -> u64 {
    let num = a as u128 * b as u128;
    let c = c as u128;
    (num.div_ceil(c)) as u64
}

/// Fee amount at `bps` basis points, floored (matches on-chain).
pub fn fee_amount(amount: u64, bps: u64) -> u64 {
    muldiv_floor(amount, bps, 10_000)
}

/// True iff price `a_num/a_den >= b_num/b_den` (u128 cross-multiplication).
pub fn price_gte(a_num: u64, a_den: u64, b_num: u64, b_den: u64) -> bool {
    a_num as u128 * b_den as u128 >= b_num as u128 * a_den as u128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding() {
        assert_eq!(muldiv_floor(10, 3, 4), 7);
        assert_eq!(muldiv_ceil(10, 3, 4), 8);
        assert_eq!(muldiv_floor(u64::MAX, u64::MAX, u64::MAX), u64::MAX);
        assert_eq!(fee_amount(999, 10), 0);
        assert_eq!(fee_amount(10_000, 10), 10);
    }

    #[test]
    fn price_compare() {
        assert!(price_gte(1, 2, 1, 3)); // 0.5 >= 0.333
        assert!(!price_gte(1, 3, 1, 2));
        assert!(price_gte(1, 3, 1, 3));
    }
}
