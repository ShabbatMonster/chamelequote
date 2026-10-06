//! Hand-encoded CPI into Orca Whirlpools (Anchor discriminators + borsh args), plus readers for
//! the two account types we inspect. Account orders follow Orca's published IDL. Instruction
//! builders are plain functions so host-side tests and clients can reuse them.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
};

pub const WHIRLPOOL_ID: Pubkey = pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
pub const MEMO_ID: Pubkey = pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
pub const NFT_UPDATE_AUTH: Pubkey = pubkey!("3axbTs2z5GBy6usVbNVoqEgZMng3vZvMnAoX29BFfwhr");
pub const TOKEN_2022_ID: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ATA_PROGRAM_ID: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const SYSTEM_ID: Pubkey = pubkey!("11111111111111111111111111111111");
pub const RENT_SYSVAR_ID: Pubkey = pubkey!("SysvarRent111111111111111111111111111111111");

const IX_OPEN_POSITION_TE: [u8; 8] = [212, 47, 95, 92, 114, 102, 131, 250];
const IX_CLOSE_POSITION_TE: [u8; 8] = [1, 182, 135, 59, 155, 25, 99, 223];
const IX_INCREASE_LIQUIDITY_V2: [u8; 8] = [133, 29, 89, 223, 69, 238, 176, 10];
const IX_DECREASE_LIQUIDITY_V2: [u8; 8] = [58, 127, 188, 62, 79, 82, 196, 96];
const IX_COLLECT_FEES_V2: [u8; 8] = [207, 117, 95, 191, 229, 180, 226, 15];
const IX_SWAP_V2: [u8; 8] = [43, 4, 237, 11, 26, 201, 30, 98];
const IX_INITIALIZE_POOL_V2: [u8; 8] = [207, 45, 87, 242, 27, 63, 204, 67];
const IX_INIT_DYNAMIC_TICK_ARRAY: [u8; 8] = [41, 33, 165, 200, 120, 231, 142, 50];

/// `remaining_accounts_info: Option<RemainingAccountsInfo>` = None. We never pass transfer-hook
/// accounts: a quote whose mint turns a hook on stops swapping and must be disabled.
const NO_REMAINING: u8 = 0;

// ---------------------------------------------------------------------------------------------
// PDAs

pub fn whirlpool_address(config: &Pubkey, mint_a: &Pubkey, mint_b: &Pubkey, tick_spacing: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[b"whirlpool", config.as_ref(), mint_a.as_ref(), mint_b.as_ref(), &tick_spacing.to_le_bytes()],
        &WHIRLPOOL_ID,
    )
    .0
}

pub fn position_address(position_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"position", position_mint.as_ref()], &WHIRLPOOL_ID).0
}

pub fn tick_array_address(whirlpool: &Pubkey, start_tick: i32) -> Pubkey {
    Pubkey::find_program_address(
        &[b"tick_array", whirlpool.as_ref(), start_tick.to_string().as_bytes()],
        &WHIRLPOOL_ID,
    )
    .0
}

pub fn oracle_address(whirlpool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"oracle", whirlpool.as_ref()], &WHIRLPOOL_ID).0
}

pub fn fee_tier_address(config: &Pubkey, tick_spacing: u16) -> Pubkey {
    Pubkey::find_program_address(&[b"fee_tier", config.as_ref(), &tick_spacing.to_le_bytes()], &WHIRLPOOL_ID).0
}

pub fn token_badge_address(config: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"token_badge", config.as_ref(), mint.as_ref()], &WHIRLPOOL_ID).0
}

pub fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[owner.as_ref(), token_program.as_ref(), mint.as_ref()], &ATA_PROGRAM_ID).0
}

// ---------------------------------------------------------------------------------------------
// Account readers (fixed offsets, discriminator included)

#[derive(Clone)]
#[cfg_attr(not(target_os = "solana"), derive(Debug))]
pub struct PoolState {
    pub whirlpools_config: Pubkey,
    pub tick_spacing: u16,
    pub fee_rate: u16,
    pub liquidity: u128,
    pub sqrt_price: u128,
    pub tick_current: i32,
    pub mint_a: Pubkey,
    pub vault_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_b: Pubkey,
}

const WHIRLPOOL_DISC: [u8; 8] = [63, 149, 209, 12, 225, 128, 99, 9];
const POSITION_DISC: [u8; 8] = [170, 188, 143, 228, 122, 64, 247, 208];

fn u128_at(d: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(d[o..o + 16].try_into().unwrap())
}
fn pk_at(d: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(d[o..o + 32].try_into().unwrap())
}

pub fn read_pool(info: &AccountInfo) -> Result<PoolState> {
    require_keys_eq!(*info.owner, WHIRLPOOL_ID, crate::error::ChameleonError::NotAWhirlpool);
    parse_pool(&info.try_borrow_data()?).ok_or(error!(crate::error::ChameleonError::NotAWhirlpool))
}

/// Parses Whirlpool account data (owner not checked).
pub fn parse_pool(d: &[u8]) -> Option<PoolState> {
    if d.len() < 245 || d[..8] != WHIRLPOOL_DISC {
        return None;
    }
    Some(PoolState {
        whirlpools_config: pk_at(d, 8),
        tick_spacing: u16::from_le_bytes([d[41], d[42]]),
        fee_rate: u16::from_le_bytes([d[45], d[46]]),
        liquidity: u128_at(d, 49),
        sqrt_price: u128_at(d, 65),
        tick_current: i32::from_le_bytes(d[81..85].try_into().unwrap()),
        mint_a: pk_at(d, 101),
        vault_a: pk_at(d, 133),
        mint_b: pk_at(d, 181),
        vault_b: pk_at(d, 213),
    })
}

#[derive(Clone)]
#[cfg_attr(not(target_os = "solana"), derive(Debug))]
pub struct PositionState {
    pub whirlpool: Pubkey,
    pub liquidity: u128,
    pub tick_lower: i32,
    pub tick_upper: i32,
}

/// None if the account does not exist (closed or never opened).
pub fn read_position(info: &AccountInfo) -> Result<Option<PositionState>> {
    if info.data_is_empty() {
        return Ok(None);
    }
    require_keys_eq!(*info.owner, WHIRLPOOL_ID, crate::error::ChameleonError::BadPosition);
    parse_position(&info.try_borrow_data()?)
        .map(Some)
        .ok_or(error!(crate::error::ChameleonError::BadPosition))
}

/// Parses Position account data (owner not checked).
pub fn parse_position(d: &[u8]) -> Option<PositionState> {
    if d.len() < 96 || d[..8] != POSITION_DISC {
        return None;
    }
    Some(PositionState {
        whirlpool: pk_at(d, 8),
        liquidity: u128_at(d, 72),
        tick_lower: i32::from_le_bytes(d[88..92].try_into().unwrap()),
        tick_upper: i32::from_le_bytes(d[92..96].try_into().unwrap()),
    })
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
    Instruction { program_id: WHIRLPOOL_ID, accounts, data }
}

/// Both sides of a pool, as seen by one owner.
#[derive(Clone, Copy)]
pub struct Sides {
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub program_a: Pubkey,
    pub program_b: Pubkey,
    pub owner_a: Pubkey,
    pub owner_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
}

#[allow(clippy::too_many_arguments)]
pub fn open_position_ix(
    funder: Pubkey,
    owner: Pubkey,
    position_mint: Pubkey,
    whirlpool: Pubkey,
    tick_lower: i32,
    tick_upper: i32,
) -> Instruction {
    ix(
        vec![
            ws(funder),
            r(owner),
            w(position_address(&position_mint)),
            ws(position_mint),
            w(ata(&owner, &position_mint, &TOKEN_2022_ID)),
            r(whirlpool),
            r(TOKEN_2022_ID),
            r(SYSTEM_ID),
            r(ATA_PROGRAM_ID),
            r(NFT_UPDATE_AUTH),
        ],
        IX_OPEN_POSITION_TE,
        &[&tick_lower.to_le_bytes(), &tick_upper.to_le_bytes(), &[0]],
    )
}

pub fn close_position_ix(authority: Pubkey, receiver: Pubkey, position_mint: Pubkey) -> Instruction {
    ix(
        vec![
            rs(authority),
            w(receiver),
            w(position_address(&position_mint)),
            w(position_mint),
            w(ata(&authority, &position_mint, &TOKEN_2022_ID)),
            r(TOKEN_2022_ID),
        ],
        IX_CLOSE_POSITION_TE,
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
pub fn modify_liquidity_ix(
    increase: bool,
    whirlpool: Pubkey,
    authority: Pubkey,
    position_mint: Pubkey,
    s: &Sides,
    tick_array_lower: Pubkey,
    tick_array_upper: Pubkey,
    liquidity: u128,
    limit_a: u64,
    limit_b: u64,
) -> Instruction {
    ix(
        vec![
            w(whirlpool),
            r(s.program_a),
            r(s.program_b),
            r(MEMO_ID),
            rs(authority),
            w(position_address(&position_mint)),
            r(ata(&authority, &position_mint, &TOKEN_2022_ID)),
            r(s.mint_a),
            r(s.mint_b),
            w(s.owner_a),
            w(s.owner_b),
            w(s.vault_a),
            w(s.vault_b),
            w(tick_array_lower),
            w(tick_array_upper),
        ],
        if increase { IX_INCREASE_LIQUIDITY_V2 } else { IX_DECREASE_LIQUIDITY_V2 },
        &[&liquidity.to_le_bytes(), &limit_a.to_le_bytes(), &limit_b.to_le_bytes(), &[NO_REMAINING]],
    )
}

pub fn collect_fees_ix(whirlpool: Pubkey, authority: Pubkey, position_mint: Pubkey, s: &Sides) -> Instruction {
    ix(
        vec![
            r(whirlpool),
            rs(authority),
            w(position_address(&position_mint)),
            r(ata(&authority, &position_mint, &TOKEN_2022_ID)),
            r(s.mint_a),
            r(s.mint_b),
            w(s.owner_a),
            w(s.vault_a),
            w(s.owner_b),
            w(s.vault_b),
            r(s.program_a),
            r(s.program_b),
            r(MEMO_ID),
        ],
        IX_COLLECT_FEES_V2,
        &[&[NO_REMAINING]],
    )
}

/// Exact-input swap of `amount`, stopping at `sqrt_price_limit` (0 = no limit).
#[allow(clippy::too_many_arguments)]
pub fn swap_ix(
    whirlpool: Pubkey,
    authority: Pubkey,
    s: &Sides,
    tick_arrays: [Pubkey; 3],
    amount: u64,
    min_out: u64,
    sqrt_price_limit: u128,
    a_to_b: bool,
) -> Instruction {
    ix(
        vec![
            r(s.program_a),
            r(s.program_b),
            r(MEMO_ID),
            rs(authority),
            w(whirlpool),
            r(s.mint_a),
            r(s.mint_b),
            w(s.owner_a),
            w(s.vault_a),
            w(s.owner_b),
            w(s.vault_b),
            w(tick_arrays[0]),
            w(tick_arrays[1]),
            w(tick_arrays[2]),
            w(oracle_address(&whirlpool)),
        ],
        IX_SWAP_V2,
        &[
            &amount.to_le_bytes(),
            &min_out.to_le_bytes(),
            &sqrt_price_limit.to_le_bytes(),
            &[1],
            &[a_to_b as u8],
            &[NO_REMAINING],
        ],
    )
}

/// Permissionless pool creation (used by clients and tests, never by the program).
#[allow(clippy::too_many_arguments)]
pub fn initialize_pool_ix(
    config: Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    program_a: Pubkey,
    program_b: Pubkey,
    funder: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
    tick_spacing: u16,
    initial_sqrt_price: u128,
) -> Instruction {
    ix(
        vec![
            r(config),
            r(mint_a),
            r(mint_b),
            r(token_badge_address(&config, &mint_a)),
            r(token_badge_address(&config, &mint_b)),
            ws(funder),
            w(whirlpool_address(&config, &mint_a, &mint_b, tick_spacing)),
            ws(vault_a),
            ws(vault_b),
            r(fee_tier_address(&config, tick_spacing)),
            r(program_a),
            r(program_b),
            r(SYSTEM_ID),
            r(RENT_SYSVAR_ID),
        ],
        IX_INITIALIZE_POOL_V2,
        &[&tick_spacing.to_le_bytes(), &initial_sqrt_price.to_le_bytes()],
    )
}

/// Permissionless, idempotent tick array creation (clients and tests).
pub fn init_tick_array_ix(whirlpool: Pubkey, funder: Pubkey, start_tick: i32) -> Instruction {
    ix(
        vec![r(whirlpool), ws(funder), w(tick_array_address(&whirlpool, start_tick)), r(SYSTEM_ID)],
        IX_INIT_DYNAMIC_TICK_ARRAY,
        &[&start_tick.to_le_bytes(), &[1]],
    )
}

// ---------------------------------------------------------------------------------------------
// CPI

/// Invokes `ix` with the whole instruction's account list; the runtime picks the ones it needs.
/// Passing the full slice (instead of collecting the exact accounts per call) keeps every CPI
/// allocation-free: the default heap is 32 KiB and never frees.
pub fn invoke(ix: &Instruction, infos: &[AccountInfo], signer_seeds: &[&[&[u8]]]) -> Result<()> {
    invoke_signed(ix, infos, signer_seeds)?;
    Ok(())
}
