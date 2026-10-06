use anchor_lang::prelude::*;

use crate::{error::ChameleonError, math};

pub const CONFIG_SEED: &[u8] = b"config";
pub const AUTHORITY_SEED: &[u8] = b"authority";
pub const QUOTE_SEED: &[u8] = b"quote";
pub const ESCROW_SEED: &[u8] = b"escrow";
pub const POSITION_MINT_SEED: &[u8] = b"position_mint";

/// Price averages: a poke more than MAX_POKE_GAP after the previous one restarts the average at
/// spot, and an average only counts once it has been poked without gaps for WARMUP seconds. A
/// keeper pokes every minute or so; manipulating a price therefore means holding it for minutes.
pub const EMA_HALF_LIFE: i64 = 300;
pub const MAX_POKE_GAP: i64 = 120;
pub const EMA_WARMUP: i64 = 600;

/// After a switch the pool's average restarts from the price the program itself set (nothing to
/// manipulate there), so it only needs this long, not the full warm-up, before the next switch.
pub const SWITCH_COOLDOWN: i64 = 300;

/// A switch that has not finished by its deadline can be aborted by anyone.
pub const SWITCH_TIMEOUT: i64 = 600;

/// Below this USDC value (raw units, 6 decimals) the old backing is dust and the price is
/// translated at the average exchange rate instead of the realised one.
pub const MIN_REALISED_VALUE_USDC: u128 = 1_000_000;

pub const MAX_FEE_SHARE_BPS: u16 = 10_000;

/// Position slots in each of our pools: the main one holds the liquidity and moves with every
/// switch; the sentinel stays behind so the pool can be repriced when we come back.
pub const MAIN_SLOT: u8 = 0;
pub const SENTINEL_SLOT: u8 = 2;

/// Paid by whoever requests a switch into a pool that doesn't exist yet, to the fee recipient
/// (who funds the keeper): the rent of the pool, its vaults and its sentinel.
pub const NEW_POOL_FEE_LAMPORTS: u64 = 30_000_000;
/// The sentinel takes this fraction (1/n) of the backing, or all of a balance smaller than
/// SENTINEL_MIN raw units (dust, where a thousandth would round to nothing).
pub const SENTINEL_DIVISOR: u64 = 1000;
pub const SENTINEL_MIN: u64 = 10_000;

/// Raw units of a balance the sentinel takes.
pub fn sentinel_share(balance: u64) -> u64 {
    (balance / SENTINEL_DIVISOR).max(balance.min(SENTINEL_MIN))
}
/// How close to the target price a pool must be for liquidity to go in. `reprice` gets it
/// exact when it can; this covers moves too small for the sentinel to carry.
pub const PRICE_TOLERANCE_BPS: u16 = 25;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, InitSpace)]
#[cfg_attr(not(target_os = "solana"), derive(Debug))]
pub struct Ema {
    /// Q64.64 sqrt price in the orientation documented where it is stored.
    pub sqrt_price: u128,
    pub last_ts: i64,
    pub streak_start: i64,
}

impl Ema {
    pub fn update(&mut self, spot: u128, now: i64) {
        if self.last_ts == 0 || now - self.last_ts > MAX_POKE_GAP {
            *self = Ema { sqrt_price: spot, last_ts: now, streak_start: now };
            return;
        }
        let dt = now - self.last_ts;
        if dt <= 0 {
            return;
        }
        let (dt, h) = (dt as u128, EMA_HALF_LIFE as u128);
        let ema = self.sqrt_price;
        self.sqrt_price = if spot >= ema {
            ema + math::mul_div(spot - ema, dt, dt + h).unwrap_or(0)
        } else {
            ema - math::mul_div(ema - spot, dt, dt + h).unwrap_or(0)
        };
        self.last_ts = now;
    }

    pub fn is_valid(&self, now: i64) -> bool {
        self.last_ts != 0 && now - self.last_ts <= MAX_POKE_GAP && self.last_ts - self.streak_start >= EMA_WARMUP
    }

    pub fn checked(&self, now: i64) -> Result<u128> {
        require!(self.is_valid(now), ChameleonError::StalePrice);
        Ok(self.sqrt_price)
    }
}

/// |a/b - 1| on prices (squares of the sqrt inputs) is within `bps`.
pub fn within_bps(sqrt_a: u128, sqrt_b: u128, bps: u16) -> bool {
    let ratio = math::div_sqrt(sqrt_a, sqrt_b); // sqrt(a/b), Q64
    let p = math::quote_out(1_000_000_000, ratio); // (a/b) * 1e9
    let one = 1_000_000_000u128;
    let diff = p.abs_diff(one);
    diff * 10_000 <= one * bps as u128
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Default, InitSpace)]
#[cfg_attr(not(target_os = "solana"), derive(Debug))]
pub enum Phase {
    #[default]
    Idle,
    /// Burn escrowed; liquidity not yet pulled.
    Requested,
    /// Liquidity pulled; backing being swapped hop by hop toward the target.
    Swapping,
    /// Backing is in the target quote; target pool price being set.
    Repricing,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, InitSpace)]
#[cfg_attr(not(target_os = "solana"), derive(Debug))]
pub struct Switch {
    pub phase: Phase,
    pub requester: Pubkey,
    pub target: Pubkey,
    pub holding: Pubkey,
    pub deadline: i64,
    /// Tokens sitting in the escrow account; burned on success, refunded on abort.
    pub escrowed: u64,
    /// Backing (raw units of the old quote) when liquidity was pulled.
    pub start_amount: u64,
    /// Old-quote amount worth MIN_REALISED_VALUE_USDC at request time.
    pub realised_min_amount: u64,
    /// sqrt(new quote per old quote) from the price averages, for dust backing.
    pub ema_rate_sqrt: u128,
    /// sqrt(quote per index) of the pool we left, and the floor at that time.
    pub old_index_sqrt: u128,
    /// Where the new pool must be set before liquidity goes back in (quote-per-index orientation).
    pub target_index_sqrt: u128,
    pub target_floor_sqrt: u128,
}

/// Global state. One per deployment.
#[account]
#[derive(InitSpace)]
pub struct Config {
    /// Manages the quote registry and risk bounds. No power over metadata, liquidity or funds.
    /// Pubkey::default() once renounced.
    pub admin: Pubkey,
    pub mint: Pubkey,
    /// Raw units burned per action (rename or quote change).
    pub burn_amount: u64,
    pub renames: u64,
    pub quote_changes: u64,
    pub total_burned: u64,
    pub bump: u8,
    pub authority_bump: u8,
    pub escrow_bump: u8,

    // Pools
    /// Unused since the move to Meteora DAMM v2 (held the Orca WhirlpoolsConfig, then the
    /// Raydium AmmConfig); kept for the account layout.
    pub clmm_config: Pubkey,
    pub tick_spacing: u16,
    pub usdc: Pubkey,
    pub wsol: Pubkey,
    pub fee_recipient: Pubkey,
    pub fee_share_bps: u16,

    // Risk bounds
    /// Max distance of our pool's spot price from its average when liquidity is pulled.
    pub max_price_move_bps: u16,
    /// Max distance of a route pool's spot price from its average before swapping through it.
    pub max_route_deviation_bps: u16,
    /// Max shortfall of a swap's output against the average price (after the pool fee).
    pub max_slippage_bps: u16,

    // Live position
    /// Pubkey::default() before launch.
    pub active_quote: Pubkey,
    pub active_pool: Pubkey,
    /// Lowest price of the curve, sqrt(quote per index), translated at every switch.
    pub floor_sqrt: u128,
    /// Average of our pool's price, sqrt(quote per index).
    pub pool_ema: Ema,

    pub switch: Switch,
}

impl Config {
    pub fn authority_seeds(&self) -> [&[u8]; 2] {
        [AUTHORITY_SEED, std::slice::from_ref(&self.authority_bump)]
    }

    /// Whether our token is token 0 (Orca: token A) in a pool with `quote`; both venues sort mints.
    pub fn index_is_a(&self, quote: &Pubkey) -> bool {
        self.mint.to_bytes() < quote.to_bytes()
    }
}

/// One approved quote token. The PDA is derived from the mint, so a mint can only be listed once.
#[account]
#[derive(InitSpace)]
pub struct QuoteEntry {
    pub mint: Pubkey,
    /// spl-token or token-2022.
    pub token_program: Pubkey,
    pub decimals: u8,
    pub enabled: bool,
    pub bump: u8,
    /// USDC or WSOL; USDC's own entry points at itself.
    pub hub: Pubkey,
    /// Whirlpool pairing `mint` with `hub` (default for USDC).
    pub route_pool: Pubkey,
    /// Average of sqrt(hub per quote) on the route pool.
    pub ema: Ema,
}

impl QuoteEntry {
    pub fn is_root(&self) -> bool {
        self.hub == self.mint
    }
}

#[event]
pub struct Renamed {
    pub user: Pubkey,
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub burned: u64,
}

#[event]
pub struct QuoteListed {
    pub mint: Pubkey,
    pub enabled: bool,
}

#[event]
pub struct SwitchRequested {
    pub requester: Pubkey,
    pub from: Pubkey,
    pub to: Pubkey,
}

#[event]
pub struct Switched {
    pub requester: Pubkey,
    pub quote: Pubkey,
    pub pool: Pubkey,
    pub index_sqrt: u128,
    pub index_amount: u64,
    pub quote_amount: u64,
    pub burned: u64,
}

#[event]
pub struct SwitchAborted {
    pub requester: Pubkey,
    pub wanted: Pubkey,
    pub landed: Pubkey,
}
