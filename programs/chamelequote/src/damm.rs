//! Hand-encoded CPI into Meteora DAMM v2 (Anchor discriminators + borsh args), readers for the
//! two account types we inspect, and the liquidity math for its pools. Written from the published
//! interface; no Meteora code is copied (its source is under a non-commercial licence).
//!
//! Our pools are DAMM v2 "customizable" pools, one per (our token, quote): token A is always our
//! token, token B the quote, so the pool price is sqrt(quote per index) directly. A pool has one
//! price range [sqrt_min, sqrt_max] that every position spans: liquidity L holds
//!   index = L (1/√P - 1/√max)    and    quote = L (√P - √min)
//! which is our curve: unsold tokens above the price, backing between the floor (√min) and it.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};

use crate::math::U256;
use crate::whirlpool::{SYSTEM_ID, TOKEN_2022_ID};

pub const DAMM_ID: Pubkey = pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");

/// Fee of the pools we create: 1% (DAMM keeps 20% of it as protocol fee).
pub const FEE_NUMERATOR: u64 = 10_000_000;
pub const FEE_DENOMINATOR: u64 = 1_000_000_000;
/// DAMM's price bounds.
pub const MIN_SQRT_PRICE: u128 = 4295048016;
pub const MAX_SQRT_PRICE: u128 = 79226673521066979257578248091;
/// Account sizes we pay rent for (upper bounds).
pub const POOL_LEN: usize = 1112;
pub const POSITION_LEN: usize = 600;
pub const NFT_MINT_LEN: usize = 400;
pub const TOKEN_ACCOUNT_LEN: usize = 200;

const IX_INIT_CUSTOMIZABLE_POOL: [u8; 8] = [20, 161, 241, 24, 189, 221, 180, 2];
const IX_CREATE_POSITION: [u8; 8] = [48, 215, 197, 153, 96, 203, 180, 133];
const IX_ADD_LIQUIDITY: [u8; 8] = [181, 157, 89, 67, 143, 182, 52, 72];
const IX_REMOVE_ALL_LIQUIDITY: [u8; 8] = [10, 51, 61, 35, 112, 105, 24, 85];
const IX_CLAIM_POSITION_FEE: [u8; 8] = [180, 38, 154, 17, 133, 33, 162, 211];
const IX_CLOSE_POSITION: [u8; 8] = [123, 134, 81, 0, 49, 68, 98, 98];
const IX_SWAP2: [u8; 8] = [65, 75, 63, 76, 235, 91, 91, 136];

const POOL_DISC: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
const POSITION_DISC: [u8; 8] = [170, 188, 143, 228, 122, 64, 247, 208];

// ---------------------------------------------------------------------------------------------
// PDAs

fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &DAMM_ID).0
}

/// The customizable pool of a pair (either order).
pub fn pool_address(mint_a: &Pubkey, mint_b: &Pubkey) -> Pubkey {
    let (hi, lo) = if mint_a.to_bytes() > mint_b.to_bytes() { (mint_a, mint_b) } else { (mint_b, mint_a) };
    pda(&[b"cpool", hi.as_ref(), lo.as_ref()])
}

pub fn vault_address(pool: &Pubkey, mint: &Pubkey) -> Pubkey {
    pda(&[b"token_vault", mint.as_ref(), pool.as_ref()])
}

pub fn position_address(nft_mint: &Pubkey) -> Pubkey {
    pda(&[b"position", nft_mint.as_ref()])
}

pub fn nft_account_address(nft_mint: &Pubkey) -> Pubkey {
    pda(&[b"position_nft_account", nft_mint.as_ref()])
}

pub fn pool_authority() -> Pubkey {
    pda(&[b"pool_authority"])
}

pub fn event_authority() -> Pubkey {
    pda(&[b"__event_authority"])
}

// ---------------------------------------------------------------------------------------------
// Account readers (fixed offsets, discriminator included)

#[derive(Clone)]
#[cfg_attr(not(target_os = "solana"), derive(Debug))]
pub struct PoolState {
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    pub liquidity: u128,
    pub sqrt_min: u128,
    pub sqrt_max: u128,
    pub sqrt_price: u128,
    pub creator: Pubkey,
}

fn u128_at(d: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(d[o..o + 16].try_into().unwrap())
}
fn pk_at(d: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(d[o..o + 32].try_into().unwrap())
}

pub fn read_pool(info: &AccountInfo) -> Result<PoolState> {
    require_keys_eq!(*info.owner, DAMM_ID, crate::error::ChameleonError::WrongPool);
    parse_pool(&info.try_borrow_data()?).ok_or(error!(crate::error::ChameleonError::WrongPool))
}

/// Parses Pool account data (owner not checked).
pub fn parse_pool(d: &[u8]) -> Option<PoolState> {
    if d.len() < 680 || d[..8] != POOL_DISC {
        return None;
    }
    Some(PoolState {
        mint_a: pk_at(d, 168),
        mint_b: pk_at(d, 200),
        vault_a: pk_at(d, 232),
        vault_b: pk_at(d, 264),
        liquidity: u128_at(d, 360),
        sqrt_min: u128_at(d, 424),
        sqrt_max: u128_at(d, 440),
        sqrt_price: u128_at(d, 456),
        creator: pk_at(d, 648),
    })
}

/// (pool, liquidity) of a position, or None if it does not exist.
pub fn read_position(info: &AccountInfo) -> Result<Option<(Pubkey, u128)>> {
    if info.data_is_empty() {
        return Ok(None);
    }
    require_keys_eq!(*info.owner, DAMM_ID, crate::error::ChameleonError::BadPosition);
    parse_position(&info.try_borrow_data()?)
        .map(Some)
        .ok_or(error!(crate::error::ChameleonError::BadPosition))
}

/// (pool, unlocked liquidity) from Position account data (owner not checked).
pub fn parse_position(d: &[u8]) -> Option<(Pubkey, u128)> {
    if d.len() < 168 || d[..8] != POSITION_DISC {
        return None;
    }
    Some((pk_at(d, 8), u128_at(d, 152)))
}

// ---------------------------------------------------------------------------------------------
// Liquidity math. DAMM's liquidity is the textbook L scaled by 2^64:
//   Δa = L Δ√P / (√P_lo √P_hi)      Δb = L Δ√P / 2^128      (√P in Q64.64)

fn to_u128(x: U256) -> u128 {
    if x > U256::from(u128::MAX) {
        u128::MAX
    } else {
        x.as_u128()
    }
}

/// Liquidity that `amount` of token A buys over [sqrt_price, sqrt_max] (rounded down).
pub fn liquidity_from_a(amount: u64, sqrt_price: u128, sqrt_max: u128) -> u128 {
    if sqrt_max <= sqrt_price {
        return u128::MAX;
    }
    to_u128(U256::from(amount) * U256::from(sqrt_price) * U256::from(sqrt_max) / U256::from(sqrt_max - sqrt_price))
}

/// Liquidity that `amount` of token B buys over [sqrt_min, sqrt_price] (rounded down).
pub fn liquidity_from_b(amount: u64, sqrt_min: u128, sqrt_price: u128) -> u128 {
    if sqrt_price <= sqrt_min {
        return u128::MAX;
    }
    to_u128((U256::from(amount) << 128) / U256::from(sqrt_price - sqrt_min))
}

/// Token B held between `lo` and `hi` by `liquidity` (rounded up).
pub fn amount_b(liquidity: u128, lo: u128, hi: u128) -> u128 {
    let p = U256::from(liquidity) * U256::from(hi.saturating_sub(lo));
    to_u128((p + (U256::one() << 128) - 1) >> 128)
}

/// Token A held between `lo` and `hi` by `liquidity` (rounded up).
pub fn amount_a(liquidity: u128, lo: u128, hi: u128) -> u128 {
    if lo == 0 || hi <= lo {
        return 0;
    }
    let num = U256::from(liquidity) * U256::from(hi - lo);
    let den = U256::from(lo) * U256::from(hi);
    to_u128((num + den - 1) / den)
}

/// The lowest √min that `quote` of token B can back for `liquidity` up to `sqrt_price`, so that
/// depositing takes no more than `quote`. Clamped to DAMM's bound.
pub fn floor_for(quote: u64, liquidity: u128, sqrt_price: u128) -> u128 {
    if liquidity == 0 {
        return sqrt_price;
    }
    let span = to_u128((U256::from(quote) << 128) / U256::from(liquidity));
    sqrt_price.saturating_sub(span).max(MIN_SQRT_PRICE)
}

/// Input that moves a pool with `liquidity` from `from` to `to` (√ prices), fee included: token B
/// (fee taken from it) going up, token A going down (fee taken from the output). A little extra
/// covers rounding; the price lands within a hair of `to`.
pub fn input_to_move(liquidity: u128, from: u128, to: u128) -> u128 {
    if to > from {
        let net = amount_b(liquidity, from, to);
        let gross = U256::from(net) * U256::from(FEE_DENOMINATOR) / U256::from(FEE_DENOMINATOR - FEE_NUMERATOR);
        to_u128(gross) + 1
    } else {
        amount_a(liquidity, to, from)
    }
}

// ---------------------------------------------------------------------------------------------
// Instruction builders

fn w(k: Pubkey) -> AccountMeta {
    AccountMeta::new(k, false)
}
fn r(k: Pubkey) -> AccountMeta {
    AccountMeta::new_readonly(k, false)
}
fn ws(k: Pubkey) -> AccountMeta {
    AccountMeta::new(k, true)
}
fn rs(k: Pubkey) -> AccountMeta {
    AccountMeta::new_readonly(k, true)
}

fn ix(mut accounts: Vec<AccountMeta>, disc: [u8; 8], args: &[&[u8]]) -> Instruction {
    // #[event_cpi]: event authority and the program itself go last.
    accounts.push(r(event_authority()));
    accounts.push(r(DAMM_ID));
    let mut data = disc.to_vec();
    for a in args {
        data.extend_from_slice(a);
    }
    Instruction { program_id: DAMM_ID, accounts, data }
}

/// A pool between our token (A) and a quote (B), as seen by one owner.
#[derive(Clone, Copy)]
pub struct Sides {
    pub pool: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub program_a: Pubkey,
    pub program_b: Pubkey,
    pub ours_a: Pubkey,
    pub ours_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
}

/// Creates the pool at `sqrt_price` over [sqrt_min, MAX] with `liquidity` from `owner` (who also
/// pays and receives the position NFT `nft_mint`): 1% fee, collected in token B only.
pub fn create_pool_ix(owner: Pubkey, nft_mint: Pubkey, s: &Sides, sqrt_min: u128, sqrt_price: u128, liquidity: u128) -> Instruction {
    let mut fee = [0u8; 27]; // BorshFeeTimeScheduler: flat cliff fee, no periods, linear mode
    fee[..8].copy_from_slice(&FEE_NUMERATOR.to_le_bytes());
    ix(
        vec![
            r(owner),
            ws(nft_mint),
            w(nft_account_address(&nft_mint)),
            ws(owner),
            r(pool_authority()),
            w(s.pool),
            w(position_address(&nft_mint)),
            r(s.mint_a),
            r(s.mint_b),
            w(s.vault_a),
            w(s.vault_b),
            w(s.ours_a),
            w(s.ours_b),
            r(s.program_a),
            r(s.program_b),
            r(TOKEN_2022_ID),
            r(SYSTEM_ID),
        ],
        IX_INIT_CUSTOMIZABLE_POOL,
        &[
            &fee,
            &0u16.to_le_bytes(), // compounding_fee_bps
            &[0],                // padding
            &[0],                // dynamic_fee: None
            &sqrt_min.to_le_bytes(),
            &MAX_SQRT_PRICE.to_le_bytes(),
            &[0], // has_alpha_vault
            &liquidity.to_le_bytes(),
            &sqrt_price.to_le_bytes(),
            &[1], // activation_type: timestamp
            &[1], // collect_fee_mode: OnlyB
            &[0], // activation_point: None (now)
        ],
    )
}

pub fn create_position_ix(owner: Pubkey, nft_mint: Pubkey, pool: Pubkey) -> Instruction {
    ix(
        vec![
            r(owner),
            ws(nft_mint),
            w(nft_account_address(&nft_mint)),
            w(pool),
            w(position_address(&nft_mint)),
            r(pool_authority()),
            ws(owner),
            r(TOKEN_2022_ID),
            r(SYSTEM_ID),
        ],
        IX_CREATE_POSITION,
        &[],
    )
}

/// Adds `liquidity`, paying at most the thresholds.
pub fn add_liquidity_ix(owner: Pubkey, nft_mint: Pubkey, s: &Sides, liquidity: u128, max_a: u64, max_b: u64) -> Instruction {
    ix(
        vec![
            w(s.pool),
            w(position_address(&nft_mint)),
            w(s.ours_a),
            w(s.ours_b),
            w(s.vault_a),
            w(s.vault_b),
            r(s.mint_a),
            r(s.mint_b),
            r(nft_account_address(&nft_mint)),
            rs(owner),
            r(s.program_a),
            r(s.program_b),
        ],
        IX_ADD_LIQUIDITY,
        &[&liquidity.to_le_bytes(), &max_a.to_le_bytes(), &max_b.to_le_bytes()],
    )
}

fn position_tokens_ix(disc: [u8; 8], owner: Pubkey, nft_mint: Pubkey, s: &Sides, args: &[&[u8]]) -> Instruction {
    ix(
        vec![
            r(pool_authority()),
            w(s.pool),
            w(position_address(&nft_mint)),
            w(s.ours_a),
            w(s.ours_b),
            w(s.vault_a),
            w(s.vault_b),
            r(s.mint_a),
            r(s.mint_b),
            r(nft_account_address(&nft_mint)),
            rs(owner),
            r(s.program_a),
            r(s.program_b),
        ],
        disc,
        args,
    )
}

pub fn remove_all_liquidity_ix(owner: Pubkey, nft_mint: Pubkey, s: &Sides) -> Instruction {
    position_tokens_ix(IX_REMOVE_ALL_LIQUIDITY, owner, nft_mint, s, &[&0u64.to_le_bytes(), &0u64.to_le_bytes()])
}

pub fn claim_fee_ix(owner: Pubkey, nft_mint: Pubkey, s: &Sides) -> Instruction {
    position_tokens_ix(IX_CLAIM_POSITION_FEE, owner, nft_mint, s, &[])
}

/// Closes an empty position (fees claimed); rent goes to `owner`.
pub fn close_position_ix(owner: Pubkey, nft_mint: Pubkey, pool: Pubkey) -> Instruction {
    ix(
        vec![
            w(nft_mint),
            w(nft_account_address(&nft_mint)),
            w(pool),
            w(position_address(&nft_mint)),
            r(pool_authority()),
            w(owner),
            rs(owner),
            r(TOKEN_2022_ID),
        ],
        IX_CLOSE_POSITION,
        &[],
    )
}

/// Exact-input swap of `amount` from `payer`'s accounts (A in when `a_to_b`).
pub fn swap_ix(payer: Pubkey, s: &Sides, a_to_b: bool, amount: u64, min_out: u64) -> Instruction {
    let (input, output) = if a_to_b { (s.ours_a, s.ours_b) } else { (s.ours_b, s.ours_a) };
    ix(
        vec![
            r(pool_authority()),
            w(s.pool),
            w(input),
            w(output),
            w(s.vault_a),
            w(s.vault_b),
            r(s.mint_a),
            r(s.mint_b),
            rs(payer),
            r(s.program_a),
            r(s.program_b),
            r(DAMM_ID), // referral token account: None
        ],
        IX_SWAP2,
        &[&amount.to_le_bytes(), &min_out.to_le_bytes(), &[0]],
    )
}
