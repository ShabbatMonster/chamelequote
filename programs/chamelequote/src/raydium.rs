//! Hand-encoded CPI into Raydium CLMM (Anchor discriminators + borsh args), plus readers for the
//! account types we inspect. Account orders follow Raydium's program source (Apache-2.0). Builders
//! are plain functions so host-side clients and tests can reuse them.
//!
//! Our own pool lives here (Axiom and most screeners index Raydium pools whatever the quote);
//! route pools for hops stay on Orca (see whirlpool.rs).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};

use crate::whirlpool::{ATA_PROGRAM_ID, MEMO_ID, RENT_SYSVAR_ID, SYSTEM_ID, TOKEN_2022_ID};

pub const CLMM_ID: Pubkey = pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");
pub const TOKEN_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Ticks per tick array.
pub const TICK_ARRAY_SIZE: i32 = 60;
/// Account sizes we pay rent for when opening a position (Raydium's LEN constants; the NFT mint
/// and its token account are upper bounds for the token-2022 extensions Raydium uses).
pub const TICK_ARRAY_LEN: usize = 10240;
pub const PERSONAL_POSITION_LEN: usize = 281;
pub const NFT_MINT_LEN: usize = 400;
pub const NFT_ACCOUNT_LEN: usize = 200;

const IX_CREATE_POOL: [u8; 8] = [233, 146, 209, 142, 207, 104, 64, 188];
const IX_OPEN_POSITION_T22: [u8; 8] = [77, 255, 174, 82, 125, 29, 201, 46];
const IX_DECREASE_LIQUIDITY_V2: [u8; 8] = [58, 127, 188, 62, 79, 82, 196, 96];
const IX_CLOSE_POSITION: [u8; 8] = [123, 134, 81, 0, 49, 68, 98, 98];
const IX_SWAP_V2: [u8; 8] = [43, 4, 237, 11, 26, 201, 30, 98];

const POOL_DISC: [u8; 8] = [247, 237, 227, 245, 215, 195, 222, 70];
const POSITION_DISC: [u8; 8] = [70, 111, 150, 126, 230, 15, 25, 117];
const TICK_ARRAY_DISC: [u8; 8] = [192, 155, 85, 205, 49, 249, 129, 42];

// ---------------------------------------------------------------------------------------------
// PDAs

fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &CLMM_ID).0
}

/// Mints must be sorted (mint_0 < mint_1).
pub fn pool_address(amm_config: &Pubkey, mint_0: &Pubkey, mint_1: &Pubkey) -> Pubkey {
    pda(&[b"pool", amm_config.as_ref(), mint_0.as_ref(), mint_1.as_ref()])
}

pub fn vault_address(pool: &Pubkey, mint: &Pubkey) -> Pubkey {
    pda(&[b"pool_vault", pool.as_ref(), mint.as_ref()])
}

pub fn observation_address(pool: &Pubkey) -> Pubkey {
    pda(&[b"observation", pool.as_ref()])
}

pub fn bitmap_address(pool: &Pubkey) -> Pubkey {
    pda(&[b"pool_tick_array_bitmap_extension", pool.as_ref()])
}

pub fn tick_array_address(pool: &Pubkey, start_tick: i32) -> Pubkey {
    pda(&[b"tick_array", pool.as_ref(), &start_tick.to_be_bytes()])
}

pub fn personal_position_address(nft_mint: &Pubkey) -> Pubkey {
    pda(&[b"position", nft_mint.as_ref()])
}

pub fn tick_array_start(tick: i32, spacing: i32) -> i32 {
    tick.div_euclid(spacing * TICK_ARRAY_SIZE) * spacing * TICK_ARRAY_SIZE
}

// ---------------------------------------------------------------------------------------------
// Account readers (fixed offsets, discriminator included)

#[derive(Clone)]
#[cfg_attr(not(target_os = "solana"), derive(Debug))]
pub struct PoolState {
    pub amm_config: Pubkey,
    pub mint_0: Pubkey,
    pub mint_1: Pubkey,
    pub vault_0: Pubkey,
    pub vault_1: Pubkey,
    pub observation: Pubkey,
    pub tick_spacing: u16,
    pub liquidity: u128,
    pub sqrt_price: u128,
    pub tick_current: i32,
}

fn u128_at(d: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(d[o..o + 16].try_into().unwrap())
}
fn pk_at(d: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(d[o..o + 32].try_into().unwrap())
}
fn i32_at(d: &[u8], o: usize) -> i32 {
    i32::from_le_bytes(d[o..o + 4].try_into().unwrap())
}

pub fn read_pool(info: &AccountInfo) -> Result<PoolState> {
    require_keys_eq!(*info.owner, CLMM_ID, crate::error::ChameleonError::WrongPool);
    parse_pool(&info.try_borrow_data()?).ok_or(error!(crate::error::ChameleonError::WrongPool))
}

/// Parses PoolState account data (owner not checked).
pub fn parse_pool(d: &[u8]) -> Option<PoolState> {
    if d.len() < 273 || d[..8] != POOL_DISC {
        return None;
    }
    Some(PoolState {
        amm_config: pk_at(d, 9),
        mint_0: pk_at(d, 73),
        mint_1: pk_at(d, 105),
        vault_0: pk_at(d, 137),
        vault_1: pk_at(d, 169),
        observation: pk_at(d, 201),
        tick_spacing: u16::from_le_bytes([d[235], d[236]]),
        liquidity: u128_at(d, 237),
        sqrt_price: u128_at(d, 253),
        tick_current: i32_at(d, 269),
    })
}

#[derive(Clone)]
#[cfg_attr(not(target_os = "solana"), derive(Debug))]
pub struct PositionState {
    pub nft_mint: Pubkey,
    pub pool: Pubkey,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity: u128,
}

/// None if the account does not exist (closed or never opened).
pub fn read_position(info: &AccountInfo) -> Result<Option<PositionState>> {
    if info.data_is_empty() {
        return Ok(None);
    }
    require_keys_eq!(*info.owner, CLMM_ID, crate::error::ChameleonError::BadPosition);
    parse_position(&info.try_borrow_data()?)
        .map(Some)
        .ok_or(error!(crate::error::ChameleonError::BadPosition))
}

/// Parses PersonalPositionState account data (owner not checked).
pub fn parse_position(d: &[u8]) -> Option<PositionState> {
    if d.len() < 97 || d[..8] != POSITION_DISC {
        return None;
    }
    Some(PositionState {
        nft_mint: pk_at(d, 9),
        pool: pk_at(d, 41),
        tick_lower: i32_at(d, 73),
        tick_upper: i32_at(d, 77),
        liquidity: u128_at(d, 81),
    })
}

/// (start tick, initialized tick count) of a TickArrayState (owner not checked).
pub fn parse_tick_array(d: &[u8]) -> Option<(Pubkey, i32, u8)> {
    if d.len() < TICK_ARRAY_LEN || d[..8] != TICK_ARRAY_DISC {
        return None;
    }
    let count_at = 8 + 32 + 4 + 168 * TICK_ARRAY_SIZE as usize;
    Some((pk_at(d, 8), i32_at(d, 40), d[count_at]))
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

fn ix(accounts: Vec<AccountMeta>, disc: [u8; 8], args: &[&[u8]]) -> Instruction {
    let mut data = disc.to_vec();
    for a in args {
        data.extend_from_slice(a);
    }
    Instruction { program_id: CLMM_ID, accounts, data }
}

/// Both sides of a pool as seen by one owner: mints, their token programs, the owner's token
/// accounts and the pool's vaults (index 0 and 1 in Raydium's sorted order).
#[derive(Clone, Copy)]
pub struct Sides {
    pub mint: [Pubkey; 2],
    pub program: [Pubkey; 2],
    pub ours: [Pubkey; 2],
    pub vault: [Pubkey; 2],
}

/// Permissionless pool creation at `sqrt_price` (token 1 per token 0, Q64.64).
pub fn create_pool_ix(creator: Pubkey, amm_config: Pubkey, pool: Pubkey, s: &Sides, sqrt_price: u128) -> Instruction {
    ix(
        vec![
            ws(creator),
            r(amm_config),
            w(pool),
            r(s.mint[0]),
            r(s.mint[1]),
            w(s.vault[0]),
            w(s.vault[1]),
            w(observation_address(&pool)),
            w(bitmap_address(&pool)),
            r(s.program[0]),
            r(s.program[1]),
            r(SYSTEM_ID),
            r(RENT_SYSVAR_ID),
        ],
        IX_CREATE_POOL,
        &[&sqrt_price.to_le_bytes(), &0u64.to_le_bytes()],
    )
}

/// One position: its NFT mint, and the tick arrays holding its two ends.
#[derive(Clone, Copy)]
pub struct Slot {
    pub nft_mint: Pubkey,
    pub lower_array: Pubkey,
    pub upper_array: Pubkey,
}

impl Slot {
    pub fn nft_account(&self, owner: &Pubkey) -> Pubkey {
        crate::whirlpool::ata(owner, &self.nft_mint, &TOKEN_2022_ID)
    }
    pub fn personal(&self) -> Pubkey {
        personal_position_address(&self.nft_mint)
    }
}

/// Opens a position whose NFT (token-2022, no metadata) goes to `owner`, who also pays and
/// deposits. With `base_0 = Some(b)` and zero `liquidity`, Raydium sizes the liquidity from
/// `amount_0_max` (b) or `amount_1_max` (!b) and takes up to the other max of the other token.
#[allow(clippy::too_many_arguments)]
pub fn open_position_ix(
    owner: Pubkey,
    pool: Pubkey,
    s: &Sides,
    slot: &Slot,
    tick_lower: i32,
    tick_upper: i32,
    tick_spacing: i32,
    amount_0_max: u64,
    amount_1_max: u64,
    base_0: bool,
) -> Instruction {
    let (lo_start, hi_start) = (tick_array_start(tick_lower, tick_spacing), tick_array_start(tick_upper, tick_spacing));
    ix(
        vec![
            ws(owner),
            r(owner),
            ws(slot.nft_mint),
            w(slot.nft_account(&owner)),
            w(pool),
            r(CLMM_ID), // protocol_position: deprecated and unchecked
            w(slot.lower_array),
            w(slot.upper_array),
            w(slot.personal()),
            w(s.ours[0]),
            w(s.ours[1]),
            w(s.vault[0]),
            w(s.vault[1]),
            r(RENT_SYSVAR_ID),
            r(SYSTEM_ID),
            r(TOKEN_ID),
            r(ATA_PROGRAM_ID),
            r(TOKEN_2022_ID),
            r(s.mint[0]),
            r(s.mint[1]),
        ],
        IX_OPEN_POSITION_T22,
        &[
            &tick_lower.to_le_bytes(),
            &tick_upper.to_le_bytes(),
            &lo_start.to_le_bytes(),
            &hi_start.to_le_bytes(),
            &0u128.to_le_bytes(),
            &amount_0_max.to_le_bytes(),
            &amount_1_max.to_le_bytes(),
            &[0],          // with_metadata
            &[1, base_0 as u8], // base_flag: Some(base_0)
        ],
    )
}

/// Takes `liquidity` out (0 collects fees only); amounts and owed fees go to `owner`'s accounts.
pub fn decrease_liquidity_ix(owner: Pubkey, pool: Pubkey, s: &Sides, slot: &Slot, liquidity: u128) -> Instruction {
    ix(
        vec![
            rs(owner),
            r(slot.nft_account(&owner)),
            w(slot.personal()),
            w(pool),
            r(CLMM_ID), // protocol_position: deprecated and unchecked
            w(s.vault[0]),
            w(s.vault[1]),
            w(slot.lower_array),
            w(slot.upper_array),
            w(s.ours[0]),
            w(s.ours[1]),
            r(TOKEN_ID),
            r(TOKEN_2022_ID),
            r(MEMO_ID),
            r(s.mint[0]),
            r(s.mint[1]),
        ],
        IX_DECREASE_LIQUIDITY_V2,
        &[&liquidity.to_le_bytes(), &0u64.to_le_bytes(), &0u64.to_le_bytes()],
    )
}

/// Burns an empty position's NFT and closes its accounts; rent goes to `owner`.
pub fn close_position_ix(owner: Pubkey, pool: Pubkey, slot: &Slot) -> Instruction {
    ix(
        vec![
            ws(owner),
            w(slot.nft_mint),
            w(slot.nft_account(&owner)),
            w(slot.personal()),
            r(SYSTEM_ID),
            r(TOKEN_2022_ID),
            // Only read when the NFT is frozen (restricted vault mints): the pool signs the thaw.
            r(pool),
        ],
        IX_CLOSE_POSITION,
        &[],
    )
}

/// Exact-input swap of `amount` that stops at `sqrt_price_limit`. `tick_arrays` must list the
/// initialized tick arrays the swap reaches, in swap order.
#[allow(clippy::too_many_arguments)]
pub fn swap_ix(
    payer: Pubkey,
    amm_config: Pubkey,
    pool: Pubkey,
    s: &Sides,
    zero_for_one: bool,
    amount: u64,
    min_out: u64,
    sqrt_price_limit: u128,
    tick_arrays: &[Pubkey],
) -> Instruction {
    let (i, o) = if zero_for_one { (0, 1) } else { (1, 0) };
    let mut accounts = vec![
        rs(payer),
        r(amm_config),
        w(pool),
        w(s.ours[i]),
        w(s.ours[o]),
        w(s.vault[i]),
        w(s.vault[o]),
        w(observation_address(&pool)),
        r(TOKEN_ID),
        r(TOKEN_2022_ID),
        r(MEMO_ID),
        r(s.mint[i]),
        r(s.mint[o]),
    ];
    accounts.extend(tick_arrays.iter().map(|k| w(*k)));
    ix(
        accounts,
        IX_SWAP_V2,
        &[&amount.to_le_bytes(), &min_out.to_le_bytes(), &sqrt_price_limit.to_le_bytes(), &[1]],
    )
}

/// Start ticks of the pool's initialized tick arrays, ascending, from the bitmap in the pool
/// account (it covers every array for tick spacings of 15 and up; the extension the rest).
pub fn initialized_tick_arrays(pool_data: &[u8], tick_spacing: i32) -> Vec<i32> {
    const BITMAP_AT: usize = 904;
    if pool_data.len() < BITMAP_AT + 128 {
        return vec![];
    }
    let span = tick_spacing * TICK_ARRAY_SIZE;
    (0..1024usize)
        .filter(|k| {
            let at = BITMAP_AT + (k / 64) * 8;
            let word = u64::from_le_bytes(pool_data[at..at + 8].try_into().unwrap());
            (word >> (k % 64)) & 1 == 1
        })
        .map(|k| (k as i32 - 512) * span)
        .collect()
}
