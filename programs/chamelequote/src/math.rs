//! Fixed-point helpers. Prices are Whirlpool-style sqrt prices: Q64.64 `u128` values of
//! sqrt(token_b / token_a) in raw units. Written from scratch (Orca's own math is not under an
//! open licence); constants were generated with 120-digit decimal arithmetic.

#![allow(clippy::manual_div_ceil, clippy::assign_op_pattern)]

use uint::construct_uint;

construct_uint! {
    pub struct U256(4);
}

pub const Q64: u128 = 1 << 64;

/// Orca's bounds for sqrt prices, ticks and arrays.
pub const MIN_SQRT_PRICE: u128 = 4295048016;
pub const MAX_SQRT_PRICE: u128 = 79226673515401279992447579055;
pub const MIN_TICK: i32 = -443636;
pub const MAX_TICK: i32 = 443636;
pub const TICK_ARRAY_SIZE: i32 = 88;

/// floor(2^128 / sqrt(1.0001)^(2^i)), i = 0..19.
const INV_SQRT_POW: [u128; 20] = [
    340265354078544963557816517032075149313,
    340248342086729790484326174814286782777,
    340214320654664324051920982716015181259,
    340146287995602323631171512101879684303,
    340010263488231146823593991679159461443,
    339738377640345403697157401104375502015,
    339195258003219555707034227454543997024,
    338111622100601834656805679988414885970,
    335954724994790223023589805789778977699,
    331682121138379247127172139078559817299,
    323299236684853023288211250268160618738,
    307163716377032989948697243942600083928,
    277268403626896220162999269216087595045,
    225923453940442621947126027127485391332,
    149997214084966997727330242082538205942,
    66119101136024775622716233608466517925,
    12847376061809297530290974190478138312,
    485053260817066172746253684029974020,
    691415978906521570653435304214167,
    1404880482679654955896180642,
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

/// sqrt(1.0001^tick) in Q64.64, clamped to Orca's bounds.
pub fn sqrt_price_at_tick(tick: i32) -> u128 {
    let t = tick.unsigned_abs();
    // ratio = sqrt(1.0001)^-|t| in Q128
    let mut ratio = U256::one() << 128;
    for (i, c) in INV_SQRT_POW.iter().enumerate() {
        if t & (1 << i) != 0 {
            ratio = (ratio * U256::from(*c)) >> 128;
        }
    }
    let q64 = if tick >= 0 {
        // invert: 2^192 / ratio(Q128) gives Q64
        ((U256::one() << 192) / ratio).as_u128()
    } else {
        (ratio >> 64).as_u128()
    };
    q64.clamp(MIN_SQRT_PRICE, MAX_SQRT_PRICE)
}

/// Greatest tick whose sqrt price is <= `sqrt_price`.
pub fn tick_at_sqrt_price(sqrt_price: u128) -> i32 {
    let (mut lo, mut hi) = (MIN_TICK, MAX_TICK);
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
    fn tick_bounds_match_orca() {
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
