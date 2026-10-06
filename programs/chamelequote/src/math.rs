//! Fixed-point helpers. Prices are sqrt prices: Q64.64 `u128` values of sqrt(token_1 / token_0)
//! (Orca: token_b / token_a) in raw units. Tick math follows Raydium CLMM (Apache-2.0) bit for
//! bit, so prices we compute for our pool match the ones Raydium stores.

#![allow(clippy::manual_div_ceil, clippy::assign_op_pattern)]

use uint::construct_uint;

construct_uint! {
    pub struct U256(4);
}

pub const Q64: u128 = 1 << 64;

/// Raydium's bounds for sqrt prices and ticks.
pub const MIN_SQRT_PRICE: u128 = 4295048016;
pub const MAX_SQRT_PRICE: u128 = 79226673521066979257578248091;
pub const MIN_TICK: i32 = -443636;
pub const MAX_TICK: i32 = 443636;
/// Orca's tick array size (route pools).
pub const TICK_ARRAY_SIZE: i32 = 88;

/// sqrt(1.0001)^-(2^i) in Q64, i = 1..18 (Raydium's tick_math constants).
const RATIOS: [u128; 18] = [
    0xfff97272373d4000,
    0xfff2e50f5f657000,
    0xffe5caca7e10f000,
    0xffcb9843d60f7000,
    0xff973b41fa98e800,
    0xff2ea16466c9b000,
    0xfe5dee046a9a3800,
    0xfcbe86c7900bb000,
    0xf987a7253ac65800,
    0xf3392b0822bb6000,
    0xe7159475a2caf000,
    0xd097f3bdfd2f2000,
    0xa9f746462d9f8000,
    0x70d869a156f31c00,
    0x31be135f97ed3200,
    0x9aa508b5b85a500,
    0x5d6af8dedc582c,
    0x2216e584f5fa,
];

pub fn mul_div(a: u128, b: u128, d: u128) -> Option<u128> {
    if d == 0 {
        return None;
    }
    let r = U256::from(a) * U256::from(b) / U256::from(d);
    (r <= U256::from(u128::MAX)).then(|| r.as_u128())
}

pub fn mul_div_u256(a: U256, b: U256, d: U256) -> Option<u128> {
    if d.is_zero() {
        return None;
    }
    let (p, overflow) = a.overflowing_mul(b);
    if overflow {
        return None;
    }
    let r = p / d;
    (r <= U256::from(u128::MAX)).then(|| r.as_u128())
}

/// sqrt(1.0001^tick) in Q64.64, exactly as Raydium computes it (ticks clamped to the bounds).
pub fn sqrt_price_at_tick(tick: i32) -> u128 {
    let tick = tick.clamp(MIN_TICK, MAX_TICK);
    let t = tick.unsigned_abs();
    let mut ratio: u128 = if t & 1 != 0 { 0xfffcb933bd6fb800 } else { Q64 };
    for (i, c) in RATIOS.iter().enumerate() {
        if t & (2 << i) != 0 {
            ratio = (ratio * c) >> 64;
        }
    }
    if tick > 0 {
        ratio = u128::MAX / ratio;
    }
    ratio
}

/// Greatest tick whose sqrt price is <= `sqrt_price`.
pub fn tick_at_sqrt_price(sqrt_price: u128) -> i32 {
    let (mut lo, mut hi) = (MIN_TICK, MAX_TICK);
    let sqrt_price = sqrt_price.clamp(MIN_SQRT_PRICE, MAX_SQRT_PRICE - 1);
    while lo < hi {
        let mid = lo + (hi - lo + 1) / 2;
        if sqrt_price_at_tick(mid) <= sqrt_price {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

pub fn align_down(tick: i32, spacing: i32) -> i32 {
    tick.div_euclid(spacing) * spacing
}

pub fn max_usable_tick(spacing: i32) -> i32 {
    align_down(MAX_TICK, spacing)
}

pub fn tick_array_start(tick: i32, spacing: i32) -> i32 {
    align_down(tick, spacing * TICK_ARRAY_SIZE)
}

pub fn clamp_sqrt(x: U256) -> u128 {
    if x <= U256::from(MIN_SQRT_PRICE) {
        MIN_SQRT_PRICE
    } else if x >= U256::from(MAX_SQRT_PRICE) {
        MAX_SQRT_PRICE
    } else {
        x.as_u128()
    }
}

/// 1 / x for a Q64.64 sqrt price (switches between b-per-a and a-per-b orientation).
pub fn invert_sqrt(x: u128) -> u128 {
    clamp_sqrt((U256::one() << 128) / U256::from(x.max(1)))
}

pub fn flip(x: u128, invert: bool) -> u128 {
    if invert {
        invert_sqrt(x)
    } else {
        x
    }
}

/// x * y for two Q64.64 sqrt prices, clamped.
pub fn mul_sqrt(x: u128, y: u128) -> u128 {
    clamp_sqrt((U256::from(x) * U256::from(y)) >> 64)
}

/// x / y for two Q64.64 sqrt prices, clamped.
pub fn div_sqrt(x: u128, y: u128) -> u128 {
    clamp_sqrt((U256::from(x) << 64) / U256::from(y.max(1)))
}

/// sqrt(num / den) in Q64.64 (not clamped), or None if den is 0 or the result overflows.
pub fn sqrt_ratio(num: u128, den: u128) -> Option<u128> {
    if den == 0 {
        return None;
    }
    let r = (U256::from(num) << 128) / U256::from(den);
    let s = r.integer_sqrt();
    (s <= U256::from(u128::MAX)).then(|| s.as_u128())
}

/// amount_in * price, where price = sqrt^2 (out per in, raw units).
pub fn quote_out(amount_in: u64, sqrt_out_per_in: u128) -> u128 {
    let p = U256::from(sqrt_out_per_in) * U256::from(sqrt_out_per_in);
    let r = (U256::from(amount_in) * p) >> 128;
    if r > U256::from(u128::MAX) {
        u128::MAX
    } else {
        r.as_u128()
    }
}

/// Liquidity for `amount` of token A over [lower, upper] (range entirely above the price).
pub fn liquidity_for_a(amount: u64, sqrt_lower: u128, sqrt_upper: u128) -> u128 {
    if sqrt_upper <= sqrt_lower {
        return 0;
    }
    let num = U256::from(amount) * ((U256::from(sqrt_lower) * U256::from(sqrt_upper)) >> 64);
    let r = num / U256::from(sqrt_upper - sqrt_lower);
    if r > U256::from(u128::MAX) {
        u128::MAX
    } else {
        r.as_u128()
    }
}

/// Liquidity for `amount` of token B over [lower, upper] (range entirely below the price).
pub fn liquidity_for_b(amount: u64, sqrt_lower: u128, sqrt_upper: u128) -> u128 {
    if sqrt_upper <= sqrt_lower {
        return 0;
    }
    let r = (U256::from(amount) << 64) / U256::from(sqrt_upper - sqrt_lower);
    if r > U256::from(u128::MAX) {
        u128::MAX
    } else {
        r.as_u128()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_bounds_match_raydium() {
        assert_eq!(sqrt_price_at_tick(MIN_TICK), MIN_SQRT_PRICE);
        assert_eq!(sqrt_price_at_tick(MAX_TICK), MAX_SQRT_PRICE);
        assert_eq!(sqrt_price_at_tick(0), Q64);
    }

    #[test]
    fn tick_roundtrip() {
        for t in [-443636, -200000, -12345, -1, 0, 1, 7, 64, 12345, 300000, 443635] {
            let s = sqrt_price_at_tick(t);
            assert_eq!(tick_at_sqrt_price(s), t, "tick {t}");
            if t < MAX_TICK - 1 {
                assert_eq!(tick_at_sqrt_price(s + 1), t);
                assert_eq!(tick_at_sqrt_price(sqrt_price_at_tick(t + 1) - 1), t);
            }
        }
    }

    #[test]
    fn known_values() {
        // sqrt(1.0001^10000) = 1.0001^5000 = 1.648680055...
        let s = sqrt_price_at_tick(10000) as f64 / Q64 as f64;
        assert!((s - 1.648_680_055_9).abs() < 1e-6, "{s}");
        let s = sqrt_price_at_tick(-10000) as f64 / Q64 as f64;
        assert!((s - 1.0 / 1.648_680_055_9).abs() < 1e-6, "{s}");
    }

    #[test]
    fn helpers() {
        assert_eq!(align_down(-1, 128), -128);
        assert_eq!(align_down(127, 128), 0);
        assert_eq!(max_usable_tick(128), 443520);
        assert_eq!(tick_array_start(-1, 128), -11264);
        assert_eq!(sqrt_ratio(4, 1), Some(2 * Q64));
        assert_eq!(quote_out(1000, 2 * Q64), 4000);
        assert_eq!(invert_sqrt(2 * Q64), Q64 / 2);
    }
}
