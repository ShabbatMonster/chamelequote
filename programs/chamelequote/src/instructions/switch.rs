//! Quote switching. A user burns `burn_amount` (held in escrow until the switch lands) to request
//! a new quote; then anyone cranks it through:
//!
//!   request -> pull -> hop (1-3x) -> reprice (0+x) -> [seed] -> add
//!
//! `pull` takes all liquidity out of the active pool, `hop` swaps the backing one route pool at a
//! time (quote -> hub -> [other hub] -> target), `reprice` creates the target pool at the
//! translated price or moves it there, and `add` lays the liquidity back as two positions: every
//! index token above the price, every quote token between the floor and the price. The index
//! keeps its value: its price and floor are multiplied by the realised exchange rate.
//!
//! Our pools are Raydium CLMM pools; route pools are Orca Whirlpools. A Raydium swap cannot move
//! the price of a pool with no liquidity, so every pool we use keeps a small full-range
//! "sentinel" position (`seed`) that a later `reprice` can swap through.
//!
//! While a switch is in flight the pool is empty, so the keeper sends the steps back to back. A
//! switch not finished by its deadline can be aborted by anyone: the burn is refunded and the
//! backing is laid into whatever quote it is held in at that point.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{program::invoke, system_instruction};
use anchor_spl::token::{self, Burn, Token, Transfer};

use crate::{
    error::ChameleonError as E,
    instructions::oracle,
    math,
    raydium::{self as ray, Sides},
    state::*,
    util,
    whirlpool::{self as wp, PoolState},
};

pub fn position_mint_address(pool: &Pubkey, index: u8) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[POSITION_MINT_SEED, pool.as_ref(), &[index]], &crate::ID)
}

/// Our token and `quote` in pool order (sorted).
pub fn pool_mints(config: &Config, quote: &Pubkey) -> [Pubkey; 2] {
    if config.index_is_a(quote) {
        [config.mint, *quote]
    } else {
        [*quote, config.mint]
    }
}

pub fn expected_pool(config: &Config, quote: &Pubkey) -> Pubkey {
    let [m0, m1] = pool_mints(config, quote);
    ray::pool_address(&config.clmm_config, &m0, &m1)
}

/// Tick ranges for the two positions `add` opens: the index position, and the backing (None
/// when the floor leaves it no room). Both meet at a tick boundary next to the price, on the
/// side that keeps the index position out of range: the index position holds only the index
/// token and the backing, the only one that can straddle the price, holds the quote plus a
/// sliver of index. No gap, so the pool always has liquidity at its price.
pub fn plan_positions(index_is_0: bool, pool: &ray::PoolState, floor_index_sqrt: u128) -> ((i32, i32), Option<(i32, i32)>) {
    let ts = pool.tick_spacing as i32;
    let max_t = math::max_usable_tick(ts);
    let lo_b = math::align_down(pool.tick_current, ts);
    let hi_b = if math::sqrt_price_at_tick(lo_b) == pool.sqrt_price { lo_b } else { lo_b + ts };
    let floor_tick =
        math::align_down(math::tick_at_sqrt_price(math::flip(floor_index_sqrt, !index_is_0)), ts).clamp(-max_t, max_t);
    if index_is_0 {
        ((hi_b, max_t), (floor_tick < hi_b).then_some((floor_tick, hi_b)))
    } else {
        ((-max_t, lo_b), (lo_b < floor_tick).then_some((lo_b, floor_tick)))
    }
}

/// Liquidity Raydium will give `amount` of token 0 (`token_0`) or token 1 over [lo, hi] at the
/// pool price, as `open_position` sizes it from a single amount. Used to skip empty positions.
pub fn liquidity_for(token_0: bool, amount: u64, sqrt_price: u128, lo: i32, hi: i32) -> u128 {
    let (sl, sh) = (math::sqrt_price_at_tick(lo), math::sqrt_price_at_tick(hi));
    let p = sqrt_price.clamp(sl, sh);
    if token_0 {
        math::liquidity_for_a(amount, p, sh)
    } else {
        math::liquidity_for_b(amount, sl, p)
    }
}

/// One of our Raydium pools (which may not exist yet) plus the program's token account for each
/// side. Validated against the pool expected for a quote by `load`.
#[derive(Accounts)]
pub struct OurPool<'info> {
    /// CHECK: the caller checks the address; `load` checks owner and contents.
    #[account(mut)]
    pub pool: UncheckedAccount<'info>,
    /// CHECK: must be token 0 of the pool.
    pub mint_0: UncheckedAccount<'info>,
    /// CHECK: must be token 1 of the pool.
    pub mint_1: UncheckedAccount<'info>,
    /// CHECK: must be the authority's ATA for mint 0.
    #[account(mut)]
    pub ours_0: UncheckedAccount<'info>,
    /// CHECK: must be the authority's ATA for mint 1.
    #[account(mut)]
    pub ours_1: UncheckedAccount<'info>,
    /// CHECK: must be the pool's vault 0.
    #[account(mut)]
    pub vault_0: UncheckedAccount<'info>,
    /// CHECK: must be the pool's vault 1.
    #[account(mut)]
    pub vault_1: UncheckedAccount<'info>,
}

impl<'info> OurPool<'info> {
    /// The pool's state (None if not created yet) and its sides, after checking the mints are our
    /// token and `quote`, the vaults are the pool's and the token accounts are the authority's.
    fn load(&self, config: &Config, quote: &Pubkey, authority: &Pubkey) -> Result<(Option<ray::PoolState>, Sides)> {
        let mints = pool_mints(config, quote);
        require!(self.mint_0.key() == mints[0] && self.mint_1.key() == mints[1], E::WrongPool);
        let key = self.pool.key();
        let state = if self.pool.data_is_empty() { None } else { Some(ray::read_pool(&self.pool)?) };
        let vaults = match &state {
            Some(p) => {
                require!(p.mint_0 == mints[0] && p.mint_1 == mints[1], E::WrongPool);
                [p.vault_0, p.vault_1]
            }
            None => [ray::vault_address(&key, &mints[0]), ray::vault_address(&key, &mints[1])],
        };
        require!(self.vault_0.key() == vaults[0] && self.vault_1.key() == vaults[1], E::WrongPool);
        util::require_ata(&self.ours_0, authority, &self.mint_0)?;
        util::require_ata(&self.ours_1, authority, &self.mint_1)?;
        let sides = Sides {
            mint: mints,
            program: [*self.mint_0.owner, *self.mint_1.owner],
            ours: [self.ours_0.key(), self.ours_1.key()],
            vault: vaults,
        };
        Ok((state, sides))
    }

    fn ours(&self, i: usize) -> &AccountInfo<'info> {
        if i == 0 {
            self.ours_0.as_ref()
        } else {
            self.ours_1.as_ref()
        }
    }
}

/// One position slot: NFT mint (our PDA), the authority's NFT account, Raydium's position
/// account, and the tick arrays holding both ends.
#[derive(Accounts)]
pub struct SlotAccounts<'info> {
    /// CHECK: PDA, verified against the pool.
    #[account(mut)]
    pub nft_mint: UncheckedAccount<'info>,
    /// CHECK: authority's token-2022 ATA for the NFT; Raydium checks it.
    #[account(mut)]
    pub nft_account: UncheckedAccount<'info>,
    /// CHECK: Raydium position PDA of the NFT mint.
    #[account(mut)]
    pub personal: UncheckedAccount<'info>,
    /// CHECK: Raydium validates.
    #[account(mut)]
    pub lower: UncheckedAccount<'info>,
    /// CHECK: Raydium validates.
    #[account(mut)]
    pub upper: UncheckedAccount<'info>,
}

impl<'info> SlotAccounts<'info> {
    /// Checks the NFT mint is our PDA for (`pool`, `i`) and the position account is Raydium's
    /// for that mint.
    fn check(&self, pool: &Pubkey, i: u8) -> Result<ray::Slot> {
        require_keys_eq!(self.nft_mint.key(), position_mint_address(pool, i).0, E::BadPosition);
        require_keys_eq!(self.personal.key(), ray::personal_position_address(&self.nft_mint.key()), E::BadPosition);
        Ok(ray::Slot { nft_mint: self.nft_mint.key(), lower_array: self.lower.key(), upper_array: self.upper.key() })
    }

    /// Rent the authority needs to open a position here (position, NFT, new tick arrays).
    fn rent_needed(&self) -> Result<u64> {
        let rent = Rent::get()?;
        let mut need = rent.minimum_balance(ray::PERSONAL_POSITION_LEN)
            + rent.minimum_balance(ray::NFT_MINT_LEN)
            + rent.minimum_balance(ray::NFT_ACCOUNT_LEN);
        if self.lower.data_is_empty() {
            need += rent.minimum_balance(ray::TICK_ARRAY_LEN);
        }
        if self.upper.data_is_empty() && self.upper.key() != self.lower.key() {
            need += rent.minimum_balance(ray::TICK_ARRAY_LEN);
        }
        Ok(need)
    }

    /// Opens a position in this slot owned (and paid for) by the authority.
    #[allow(clippy::too_many_arguments)]
    fn open(
        &self,
        pool_key: &Pubkey,
        i: u8,
        sides: &Sides,
        ts: i32,
        range: (i32, i32),
        amount_max: [u64; 2],
        base_0: bool,
        authority: Pubkey,
        infos: &[AccountInfo<'info>],
        auth_seeds: &[&[u8]],
    ) -> Result<()> {
        let slot = self.check(pool_key, i)?;
        require!(self.personal.data_is_empty(), E::BadPosition);
        let mint_bump = [position_mint_address(pool_key, i).1];
        let idx = [i];
        let mint_seeds: &[&[u8]] = &[POSITION_MINT_SEED, pool_key.as_ref(), &idx, &mint_bump];
        let ix = ray::open_position_ix(
            authority, *pool_key, sides, &slot, range.0, range.1, ts, amount_max[0], amount_max[1], base_0,
        );
        wp::invoke(&ix, infos, &[auth_seeds, mint_seeds])
    }
}

/// The liquidity slots (0: index, 1: backing).
#[derive(Accounts)]
pub struct Positions<'info> {
    pub slot_0: SlotAccounts<'info>,
    pub slot_1: SlotAccounts<'info>,
}

impl<'info> Positions<'info> {
    fn get(&self, i: u8) -> &SlotAccounts<'info> {
        if i == 0 {
            &self.slot_0
        } else {
            &self.slot_1
        }
    }
}

/// Programs and sysvars Raydium wants to see.
#[derive(Accounts)]
pub struct Programs<'info> {
    pub token_program: Program<'info, Token>,
    /// CHECK: address checked.
    #[account(address = wp::TOKEN_2022_ID)]
    pub token_2022_program: UncheckedAccount<'info>,
    /// CHECK: address checked.
    #[account(address = wp::MEMO_ID)]
    pub memo_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: address checked.
    #[account(address = wp::ATA_PROGRAM_ID)]
    pub associated_token_program: UncheckedAccount<'info>,
    /// CHECK: address checked.
    #[account(address = wp::RENT_SYSVAR_ID)]
    pub rent: UncheckedAccount<'info>,
    /// CHECK: address checked.
    #[account(address = ray::CLMM_ID)]
    pub clmm_program: UncheckedAccount<'info>,
}

impl<'info> Programs<'info> {
    /// The token program that owns `mint`.
    fn for_mint(&self, mint: &AccountInfo) -> AccountInfo<'info> {
        if *mint.owner == wp::TOKEN_2022_ID {
            self.token_2022_program.to_account_info()
        } else {
            self.token_program.to_account_info()
        }
    }
}

/// Raydium makes the position owner pay the rent of what it creates, and our positions are owned
/// by the authority PDA, so the funder tops the authority up first. Closed positions refund the
/// authority, so the balance is mostly reused.
fn fund_authority<'info>(funder: &AccountInfo<'info>, authority: &AccountInfo<'info>, system: &AccountInfo<'info>, need: u64) -> Result<()> {
    // A system account must stay rent exempt (or empty) after paying.
    let need = need + Rent::get()?.minimum_balance(0);
    let have = authority.lamports();
    if have < need {
        invoke(
            &system_instruction::transfer(funder.key, authority.key, need - have),
            &[funder.clone(), authority.clone(), system.clone()],
        )?;
    }
    Ok(())
}

/// Both sides of a Whirlpool plus the program's token account for each.
#[derive(Accounts)]
pub struct PoolSides<'info> {
    /// CHECK: read and validated in `load`; Orca validates the rest.
    #[account(mut)]
    pub whirlpool: UncheckedAccount<'info>,
    /// CHECK: must equal the pool's mint A.
    pub mint_a: UncheckedAccount<'info>,
    /// CHECK: must equal the pool's mint B.
    pub mint_b: UncheckedAccount<'info>,
    /// CHECK: Orca checks it owns mint A.
    pub token_program_a: UncheckedAccount<'info>,
    /// CHECK: Orca checks it owns mint B.
    pub token_program_b: UncheckedAccount<'info>,
    /// CHECK: must be the authority's ATA for mint A.
    #[account(mut)]
    pub ours_a: UncheckedAccount<'info>,
    /// CHECK: must be the authority's ATA for mint B.
    #[account(mut)]
    pub ours_b: UncheckedAccount<'info>,
    /// CHECK: must equal the pool's vault A.
    #[account(mut)]
    pub vault_a: UncheckedAccount<'info>,
    /// CHECK: must equal the pool's vault B.
    #[account(mut)]
    pub vault_b: UncheckedAccount<'info>,
}

impl<'info> PoolSides<'info> {
    pub fn load(&self, authority: &Pubkey) -> Result<(PoolState, wp::Sides)> {
        let p = wp::read_pool(&self.whirlpool)?;
        require!(
            p.mint_a == self.mint_a.key()
                && p.mint_b == self.mint_b.key()
                && p.vault_a == self.vault_a.key()
                && p.vault_b == self.vault_b.key(),
            E::WrongPool
        );
        util::require_ata(&self.ours_a, authority, &self.mint_a)?;
        util::require_ata(&self.ours_b, authority, &self.mint_b)?;
        let sides = wp::Sides {
            mint_a: p.mint_a,
            mint_b: p.mint_b,
            program_a: *self.mint_a.owner,
            program_b: *self.mint_b.owner,
            owner_a: self.ours_a.key(),
            owner_b: self.ours_b.key(),
            vault_a: p.vault_a,
            vault_b: p.vault_b,
        };
        Ok((p, sides))
    }
}


/// Sets the targets once the backing sits in the target quote. The realised rate is used unless
/// the backing was dust, in which case the averages decide.
fn arrive(config: &mut Config, final_amount: u64) {
    let s = &mut config.switch;
    let realised = s.start_amount > 0 && s.start_amount >= s.realised_min_amount && final_amount > 0;
    let rate = if realised {
        math::sqrt_ratio(final_amount as u128, s.start_amount as u128).unwrap_or(s.ema_rate_sqrt)
    } else {
        s.ema_rate_sqrt
    };
    s.target_index_sqrt = math::mul_sqrt(s.old_index_sqrt, rate);
    s.target_floor_sqrt = math::mul_sqrt(config.floor_sqrt, rate);
    s.holding = s.target;
    s.phase = Phase::Repricing;
}

/// sqrt(USDC per quote) from the averages. `strict` refuses stale averages.
fn usd_sqrt(e: &QuoteEntry, wsol: &QuoteEntry, config: &Config, now: i64, strict: bool) -> Result<u128> {
    if e.is_root() {
        return Ok(math::Q64);
    }
    let get = |x: &QuoteEntry| if strict { x.ema.checked(now) } else { Ok(x.ema.sqrt_price.max(1)) };
    let own = get(e)?;
    Ok(if e.hub == config.wsol { math::mul_sqrt(own, get(wsol)?) } else { own })
}

// =============================================================================================
// launch

#[derive(Accounts)]
pub struct Launch<'info> {
    pub admin: Signer<'info>,

    #[account(mut, has_one = admin @ E::NotAdmin)]
    pub config: Box<Account<'info, Config>>,

    pub quote: Box<Account<'info, QuoteEntry>>,
}

/// Starts the first lay-out. `index_sqrt` is sqrt(quote per index) in Q64.64 raw units; it is
/// also the floor. Then anyone runs reprice/add as for a switch.
pub fn launch(ctx: Context<Launch>, index_sqrt: u128) -> Result<()> {
    let config = &mut ctx.accounts.config;
    require!(config.active_quote == Pubkey::default(), E::AlreadyLaunched);
    require!(config.switch.phase == Phase::Idle, E::WrongPhase);
    require!(ctx.accounts.quote.enabled, E::QuoteDisabled);
    require!((math::MIN_SQRT_PRICE..=math::MAX_SQRT_PRICE).contains(&index_sqrt), E::InvalidParam);
    let quote = ctx.accounts.quote.mint;
    config.switch = Switch {
        phase: Phase::Repricing,
        target: quote,
        holding: quote,
        target_index_sqrt: index_sqrt,
        target_floor_sqrt: index_sqrt,
        ..Switch::default()
    };
    Ok(())
}

// =============================================================================================
// request

#[derive(Accounts)]
pub struct RequestSwitch<'info> {
    pub user: Signer<'info>,

    #[account(mut, has_one = mint)]
    pub config: Box<Account<'info, Config>>,

    /// CHECK: our mint (has_one on config).
    pub mint: UncheckedAccount<'info>,

    /// CHECK: the token program's transfer checks `user` owns it and that its mint matches the
    /// escrow's (ours).
    #[account(mut)]
    pub user_token: UncheckedAccount<'info>,

    /// CHECK: PDA.
    #[account(mut, seeds = [ESCROW_SEED], bump = config.escrow_bump)]
    pub escrow: UncheckedAccount<'info>,

    #[account(constraint = old_quote.mint == config.active_quote @ E::WrongQuote)]
    pub old_quote: Box<Account<'info, QuoteEntry>>,

    pub new_quote: Box<Account<'info, QuoteEntry>>,

    #[account(constraint = wsol_quote.mint == config.wsol @ E::WrongQuote)]
    pub wsol_quote: Box<Account<'info, QuoteEntry>>,

    pub token_program: Program<'info, Token>,
}

pub fn request_switch(ctx: Context<RequestSwitch>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let config = &ctx.accounts.config;
    require!(config.active_quote != Pubkey::default(), E::NotLaunched);
    require!(config.switch.phase == Phase::Idle, E::WrongPhase);
    let (old, new, wsol) = (&ctx.accounts.old_quote, &ctx.accounts.new_quote, &ctx.accounts.wsol_quote);
    require!(new.enabled, E::QuoteDisabled);
    require_keys_neq!(new.mint, config.active_quote, E::SameQuote);

    let usd_old = usd_sqrt(old, wsol, config, now, true)?;
    let usd_new = usd_sqrt(new, wsol, config, now, true)?;
    // new per old = (USDC per old) / (USDC per new)
    let ema_rate_sqrt = math::div_sqrt(usd_old, usd_new);
    let realised_min = math::quote_out(MIN_REALISED_VALUE_USDC as u64, math::invert_sqrt(usd_old)).min(u64::MAX as u128);

    let burn = config.burn_amount;
    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.key(),
            Transfer {
                from: ctx.accounts.user_token.to_account_info(),
                to: ctx.accounts.escrow.to_account_info(),
                authority: ctx.accounts.user.to_account_info(),
            },
        ),
        burn,
    )?;

    let (from, to, user) = (config.active_quote, new.mint, ctx.accounts.user.key());
    ctx.accounts.config.switch = Switch {
        phase: Phase::Requested,
        requester: user,
        target: to,
        holding: from,
        deadline: now + SWITCH_TIMEOUT,
        escrowed: burn,
        realised_min_amount: realised_min as u64,
        ema_rate_sqrt,
        ..Switch::default()
    };
    emit!(SwitchRequested { requester: user, from, to });
    Ok(())
}

// =============================================================================================
// pull

#[derive(Accounts)]
pub struct Pull<'info> {
    pub cranker: Signer<'info>,

    #[account(mut)]
    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer; receives the closed positions' rent (reused at the next `add`).
    #[account(mut, seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    pub pool: OurPool<'info>,
    pub positions: Positions<'info>,
    pub programs: Programs<'info>,
}

/// Takes the liquidity out of the active pool and closes the positions (the sentinel stays).
/// Fees earned since the last `claim_fees` come out with it and stay as backing.
pub fn pull<'info>(ctx: Context<'info, Pull<'info>>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let config = &ctx.accounts.config;
    require!(config.switch.phase == Phase::Requested, E::WrongPhase);
    let pool_key = ctx.accounts.pool.pool.key();
    require_keys_eq!(pool_key, config.active_pool, E::WrongPool);
    let authority = ctx.accounts.authority.key();
    let quote = config.active_quote;
    let (state, sides) = ctx.accounts.pool.load(config, &quote, &authority)?;
    let ps = state.ok_or(E::WrongPool)?;
    let index_is_0 = config.index_is_a(&quote);

    // Refuse to pull on a price pushed away from its average (the LOOP "block start" check).
    let spot = math::flip(ps.sqrt_price, !index_is_0);
    let ema = config.pool_ema.checked(now)?;
    require!(within_bps(spot, ema, config.max_price_move_bps), E::PoolManipulated);

    let infos = ctx.accounts.to_account_infos();
    let bump = [config.authority_bump];
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump];
    for i in 0..2u8 {
        let accounts = ctx.accounts.positions.get(i);
        let slot = accounts.check(&pool_key, i)?;
        let Some(st) = ray::read_position(&accounts.personal)? else { continue };
        wp::invoke(&ray::decrease_liquidity_ix(authority, pool_key, &sides, &slot, st.liquidity), &infos, &[seeds])?;
        wp::invoke(&ray::close_position_ix(authority, pool_key, &slot), &infos, &[seeds])?;
    }

    let start = util::token_amount(ctx.accounts.pool.ours(index_is_0 as usize))?;
    let config = &mut ctx.accounts.config;
    config.switch.old_index_sqrt = spot;
    config.switch.start_amount = start;
    config.switch.holding = quote;
    config.switch.phase = Phase::Swapping;
    Ok(())
}

/// Sends `fee_share_bps` of collected fees to the fee recipient. Each leg is (fee, our token
/// account, recipient's token account, mint, token program). The rest stays in the program's
/// accounts and becomes backing at the next lay-out.
pub fn pay_fee_share<'info>(
    config: &Config,
    legs: [(u64, &AccountInfo<'info>, &AccountInfo<'info>, &AccountInfo<'info>, &AccountInfo<'info>); 2],
    authority: &AccountInfo<'info>,
    seeds: &[&[u8]],
) -> Result<()> {
    if config.fee_share_bps == 0 {
        return Ok(());
    }
    for (fee, ours, fee_acct, mint, program) in legs {
        let amount = (fee as u128 * config.fee_share_bps as u128 / 10_000) as u64;
        if amount == 0 {
            continue;
        }
        util::require_ata(fee_acct, &config.fee_recipient, mint)?;
        util::transfer(program, ours, mint, fee_acct, authority, amount, &[seeds])?;
    }
    Ok(())
}

// =============================================================================================
// claim fees

#[derive(Accounts)]
pub struct ClaimFees<'info> {
    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer.
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    pub pool: OurPool<'info>,
    pub positions: Positions<'info>,

    /// CHECK: fee recipient's ATA for mint 0 (checked when a share is paid).
    #[account(mut)]
    pub fee_0: UncheckedAccount<'info>,
    /// CHECK: fee recipient's ATA for mint 1 (checked when a share is paid).
    #[account(mut)]
    pub fee_1: UncheckedAccount<'info>,

    pub programs: Programs<'info>,
}

/// Collects the trading fees the live positions have earned and pays the fee recipient its
/// share, without touching liquidity. Anyone can call it; the keeper does so periodically and
/// right before pulling.
pub fn claim_fees<'info>(ctx: Context<'info, ClaimFees<'info>>) -> Result<()> {
    let config = &ctx.accounts.config;
    require!(matches!(config.switch.phase, Phase::Idle | Phase::Requested), E::WrongPhase);
    let pool_key = ctx.accounts.pool.pool.key();
    require_keys_eq!(pool_key, config.active_pool, E::WrongPool);
    let authority = ctx.accounts.authority.key();
    let (_, sides) = ctx.accounts.pool.load(config, &config.active_quote, &authority)?;
    let infos = ctx.accounts.to_account_infos();
    let bump = [config.authority_bump];
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump];
    let p = &ctx.accounts.pool;
    let mut fees = [0u64; 2];
    for i in 0..2u8 {
        let accounts = ctx.accounts.positions.get(i);
        let slot = accounts.check(&pool_key, i)?;
        let Some(st) = ray::read_position(&accounts.personal)? else { continue };
        if st.liquidity == 0 {
            continue;
        }
        let before = [util::token_amount(p.ours(0))?, util::token_amount(p.ours(1))?];
        wp::invoke(&ray::decrease_liquidity_ix(authority, pool_key, &sides, &slot, 0), &infos, &[seeds])?;
        fees[0] += util::token_amount(p.ours(0))? - before[0];
        fees[1] += util::token_amount(p.ours(1))? - before[1];
    }
    let progs = &ctx.accounts.programs;
    let (m0, m1) = (p.mint_0.to_account_info(), p.mint_1.to_account_info());
    pay_fee_share(
        config,
        [
            (fees[0], p.ours(0), &ctx.accounts.fee_0.to_account_info(), &m0, &progs.for_mint(&m0)),
            (fees[1], p.ours(1), &ctx.accounts.fee_1.to_account_info(), &m1, &progs.for_mint(&m1)),
        ],
        &ctx.accounts.authority,
        seeds,
    )
}

// =============================================================================================
// hop

#[derive(Accounts)]
pub struct Hop<'info> {
    #[account(mut)]
    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer.
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    #[account(constraint = holding.mint == config.switch.holding @ E::WrongQuote)]
    pub holding: Box<Account<'info, QuoteEntry>>,

    #[account(constraint = target.mint == config.switch.target @ E::WrongQuote)]
    pub target: Box<Account<'info, QuoteEntry>>,

    /// The entry whose route pool this hop trades through.
    pub via: Box<Account<'info, QuoteEntry>>,

    pub pool: PoolSides<'info>,

    /// CHECK: Orca validates.
    #[account(mut)]
    pub tick_array_0: UncheckedAccount<'info>,
    /// CHECK: Orca validates.
    #[account(mut)]
    pub tick_array_1: UncheckedAccount<'info>,
    /// CHECK: Orca validates.
    #[account(mut)]
    pub tick_array_2: UncheckedAccount<'info>,
    /// CHECK: Orca validates.
    #[account(mut)]
    pub oracle: UncheckedAccount<'info>,
    /// CHECK: address checked.
    #[account(address = wp::MEMO_ID)]
    pub memo_program: UncheckedAccount<'info>,
    /// CHECK: address checked.
    #[account(address = wp::WHIRLPOOL_ID)]
    pub whirlpool_program: UncheckedAccount<'info>,
}

/// One swap along the route tree rooted at USDC: up from the holding toward its hub, or down from
/// a hub toward the target. The whole holding is swapped, against the averages.
pub fn hop<'info>(ctx: Context<'info, Hop<'info>>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let config = &ctx.accounts.config;
    require!(config.switch.phase == Phase::Swapping, E::WrongPhase);
    let (holding, target, via) = (&ctx.accounts.holding, &ctx.accounts.target, &ctx.accounts.via);

    let is_ancestor = |h: &Pubkey| (*h == target.hub && !target.is_root()) || (*h == config.usdc && target.mint != config.usdc);
    let (out_mint, down) = if is_ancestor(&holding.mint) {
        require!(
            via.hub == holding.mint && !via.is_root() && (via.mint == target.mint || via.mint == target.hub),
            E::WrongHop
        );
        (via.mint, true)
    } else {
        require!(via.mint == holding.mint && !holding.is_root(), E::WrongHop);
        (holding.hub, false)
    };

    let pool_key = ctx.accounts.pool.whirlpool.key();
    require_keys_eq!(pool_key, via.route_pool, E::WrongPool);
    let authority = ctx.accounts.authority.key();
    let (ps, sides) = ctx.accounts.pool.load(&authority)?;
    let in_is_a = ps.mint_a == holding.mint;
    require!(
        (in_is_a && ps.mint_b == out_mint) || (!in_is_a && ps.mint_a == out_mint && ps.mint_b == holding.mint),
        E::WrongPool
    );
    let p = &ctx.accounts.pool;
    let (ours_in, ours_out) = if in_is_a { (&p.ours_a, &p.ours_b) } else { (&p.ours_b, &p.ours_a) };

    let amount = util::token_amount(ours_in)?;
    if amount > 0 {
        let (spot, fee_rate) = oracle::route_spot(via, &p.whirlpool)?;
        let ema = via.ema.checked(now)?;
        require!(within_bps(spot, ema, config.max_route_deviation_bps), E::RouteOffAverage);
        // The average is hub per via-quote: going up we receive hub, going down we receive quote.
        let sqrt_out_per_in = if down { math::invert_sqrt(ema) } else { ema };
        let fair = math::quote_out(amount, sqrt_out_per_in);
        let min_out = fair * (1_000_000 - fee_rate as u128) / 1_000_000 * (10_000 - config.max_slippage_bps as u128)
            / 10_000;
        let min_out = min_out.min(u64::MAX as u128) as u64;

        let before = util::token_amount(ours_out)?;
        let tick_arrays = [ctx.accounts.tick_array_0.key(), ctx.accounts.tick_array_1.key(), ctx.accounts.tick_array_2.key()];
        let ix = wp::swap_ix(pool_key, authority, &sides, tick_arrays, amount, min_out, 0, in_is_a);
        let bump = [config.authority_bump];
        wp::invoke(&ix, &ctx.accounts.to_account_infos(), &[&[AUTHORITY_SEED, &bump]])?;
        // Exact-in swaps can stop early when they run out of tick arrays: insist on a full fill.
        require!(util::token_amount(ours_in)? == 0, E::SlippageExceeded);
        require!(util::token_amount(ours_out)? - before >= min_out, E::SlippageExceeded);
    }

    let final_amount = util::token_amount(ours_out)?;
    let config = &mut ctx.accounts.config;
    config.switch.holding = out_mint;
    if out_mint == config.switch.target {
        arrive(config, final_amount);
    }
    Ok(())
}

// =============================================================================================
// reprice

#[derive(Accounts)]
pub struct Reprice<'info> {
    /// Pays for the pool when it has to be created.
    #[account(mut)]
    pub funder: Signer<'info>,

    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer.
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    pub pool: OurPool<'info>,

    /// CHECK: address checked.
    #[account(address = config.clmm_config @ E::WrongPool)]
    pub amm_config: UncheckedAccount<'info>,
    /// CHECK: Raydium's PDA for the pool; Raydium validates.
    #[account(mut)]
    pub observation: UncheckedAccount<'info>,
    /// CHECK: Raydium's PDA for the pool; only used to create it.
    #[account(mut)]
    pub bitmap: UncheckedAccount<'info>,

    pub programs: Programs<'info>,
    // remaining_accounts: the pool's initialized tick arrays the swap reaches, in swap order.
}

/// Puts the target pool at the target price: creates it there if it does not exist, otherwise
/// swaps toward it with the price as the limit. With only our sentinel in the way this costs
/// next to nothing (we trade with ourselves); other liquidity in the way gets traded against at
/// prices better than the target. Within PRICE_TOLERANCE_BPS of the target it does nothing.
pub fn reprice<'info>(ctx: Context<'info, Reprice<'info>>) -> Result<()> {
    let config = &ctx.accounts.config;
    require!(config.switch.phase == Phase::Repricing, E::WrongPhase);
    let target = config.switch.target;
    let pool_key = ctx.accounts.pool.pool.key();
    require_keys_eq!(pool_key, expected_pool(config, &target), E::WrongPool);
    let authority = ctx.accounts.authority.key();
    let (state, sides) = ctx.accounts.pool.load(config, &target, &authority)?;
    let target_sqrt = math::flip(config.switch.target_index_sqrt, !config.index_is_a(&target));
    require!((math::MIN_SQRT_PRICE..math::MAX_SQRT_PRICE).contains(&target_sqrt), E::InvalidParam);
    let mut infos = ctx.accounts.to_account_infos();

    let Some(ps) = state else {
        let ix = ray::create_pool_ix(ctx.accounts.funder.key(), config.clmm_config, pool_key, &sides, target_sqrt);
        return wp::invoke(&ix, &infos, &[]);
    };
    if within_bps(ps.sqrt_price, target_sqrt, PRICE_TOLERANCE_BPS) {
        // Close enough for `add`; a swap this small may not even move a token through.
        return Ok(());
    }
    let zero_for_one = ps.sqrt_price > target_sqrt;
    let input = if zero_for_one { 0 } else { 1 };
    let amount = util::token_amount(ctx.accounts.pool.ours(input))?;
    require!(amount > 0, E::NothingToReprice);
    let arrays: Vec<Pubkey> = ctx.remaining_accounts.iter().map(|a| a.key()).collect();
    infos.extend_from_slice(ctx.remaining_accounts);
    let ix = ray::swap_ix(authority, config.clmm_config, pool_key, &sides, zero_for_one, amount, 0, target_sqrt, &arrays);
    let bump = [config.authority_bump];
    wp::invoke(&ix, &infos, &[&[AUTHORITY_SEED, &bump]])
}

// =============================================================================================
// seed

#[derive(Accounts)]
pub struct Seed<'info> {
    /// Tops up the authority for the position's rent.
    #[account(mut)]
    pub funder: Signer<'info>,

    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer; pays Raydium's rent.
    #[account(mut, seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    pub pool: OurPool<'info>,
    pub sentinel: SlotAccounts<'info>,
    pub programs: Programs<'info>,
}

/// Opens the target pool's sentinel: a full-range position holding 1/SENTINEL_DIVISOR of the
/// backing (all of it when it is dust) and the matching index, never pulled. Later visits reprice by swapping
/// through it, which a Raydium pool with no liquidity cannot do. Runs during repricing, at
/// whatever price the pool has; a no-op if the pool already has one.
pub fn seed<'info>(ctx: Context<'info, Seed<'info>>) -> Result<()> {
    let config = &ctx.accounts.config;
    require!(config.switch.phase == Phase::Repricing, E::WrongPhase);
    let target = config.switch.target;
    let pool_key = ctx.accounts.pool.pool.key();
    require_keys_eq!(pool_key, expected_pool(config, &target), E::WrongPool);
    let authority = ctx.accounts.authority.key();
    let (state, sides) = ctx.accounts.pool.load(config, &target, &authority)?;
    let ps = state.ok_or(E::WrongPool)?;
    let sentinel = &ctx.accounts.sentinel;
    sentinel.check(&pool_key, SENTINEL_SLOT)?;
    if !sentinel.personal.data_is_empty() {
        return Ok(());
    }

    let ts = ps.tick_spacing as i32;
    let max_t = math::max_usable_tick(ts);
    let p = &ctx.accounts.pool;
    let have = [util::token_amount(p.ours(0))?, util::token_amount(p.ours(1))?];
    let want = have.map(sentinel_share);
    let l0 = liquidity_for(true, want[0], ps.sqrt_price, -max_t, max_t);
    let l1 = liquidity_for(false, want[1], ps.sqrt_price, -max_t, max_t);
    require!(l0 > 1 && l1 > 1, E::NothingToReprice);
    // Size from the scarcer side; the other side's need is then within its share.
    let base_0 = l0 <= l1;
    let amount_max = if base_0 { [want[0], have[1]] } else { [have[0], want[1]] };

    fund_authority(
        &ctx.accounts.funder,
        &ctx.accounts.authority,
        &ctx.accounts.programs.system_program,
        sentinel.rent_needed()?,
    )?;
    let infos = ctx.accounts.to_account_infos();
    let bump = [config.authority_bump];
    let auth_seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump];
    sentinel.open(&pool_key, SENTINEL_SLOT, &sides, ts, (-max_t, max_t), amount_max, base_0, authority, &infos, auth_seeds)
}

// =============================================================================================
// add

#[derive(Accounts)]
pub struct Add<'info> {
    /// Tops up the authority for the positions' rent (refunded to it when they are pulled).
    #[account(mut)]
    pub funder: Signer<'info>,

    #[account(mut, has_one = mint)]
    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer; pays Raydium's rent.
    #[account(mut, seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    pub pool: OurPool<'info>,
    pub positions: Positions<'info>,

    /// CHECK: PDA.
    #[account(mut, seeds = [ESCROW_SEED], bump = config.escrow_bump)]
    pub escrow: UncheckedAccount<'info>,

    /// CHECK: our mint (has_one on config).
    #[account(mut)]
    pub mint: UncheckedAccount<'info>,

    pub programs: Programs<'info>,
}

pub fn add<'info>(ctx: Context<'info, Add<'info>>) -> Result<()> {
    let config = &ctx.accounts.config;
    require!(config.switch.phase == Phase::Repricing, E::WrongPhase);
    let target = config.switch.target;
    let pool_key = ctx.accounts.pool.pool.key();
    require_keys_eq!(pool_key, expected_pool(config, &target), E::WrongPool);
    let authority = ctx.accounts.authority.key();
    let (state, sides) = ctx.accounts.pool.load(config, &target, &authority)?;
    let ps = state.ok_or(E::WrongPool)?;
    let index_is_0 = config.index_is_a(&target);
    require!(
        within_bps(ps.sqrt_price, math::flip(config.switch.target_index_sqrt, !index_is_0), PRICE_TOLERANCE_BPS),
        E::NotAtTarget
    );

    let p = &ctx.accounts.pool;
    let (qi, ii) = if index_is_0 { (1, 0) } else { (0, 1) };
    let index_balance = util::token_amount(p.ours(ii))?;
    let quote_balance = util::token_amount(p.ours(qi))?;
    let (index_range, backing_range) = plan_positions(index_is_0, &ps, config.switch.target_floor_sqrt);
    let ts = ps.tick_spacing as i32;
    let pos = &ctx.accounts.positions;

    fund_authority(
        &ctx.accounts.funder,
        &ctx.accounts.authority,
        &ctx.accounts.programs.system_program,
        pos.slot_0.rent_needed()? + pos.slot_1.rent_needed()?,
    )?;
    let infos = ctx.accounts.to_account_infos();
    let bump = [config.authority_bump];
    let auth_seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump];

    // Backing first: when it straddles the price it takes a sliver of index too.
    if let Some(range) = backing_range {
        if liquidity_for(!index_is_0, quote_balance, ps.sqrt_price, range.0, range.1) > 1 {
            let mut max = [0u64; 2];
            max[qi] = quote_balance;
            max[ii] = index_balance;
            pos.slot_1.open(&pool_key, 1, &sides, ts, range, max, !index_is_0, authority, &infos, auth_seeds)?;
        }
    }
    let index_left = util::token_amount(p.ours(ii))?;
    if index_range.0 < index_range.1 && liquidity_for(index_is_0, index_left, ps.sqrt_price, index_range.0, index_range.1) > 1 {
        let mut max = [0u64; 2];
        max[ii] = index_left;
        max[qi] = util::token_amount(p.ours(qi))?;
        pos.slot_0.open(&pool_key, 0, &sides, ts, index_range, max, index_is_0, authority, &infos, auth_seeds)?;
    }

    let escrowed = config.switch.escrowed;
    if escrowed > 0 {
        token::burn(
            CpiContext::new_with_signer(
                ctx.accounts.programs.token_program.key(),
                Burn {
                    mint: ctx.accounts.mint.to_account_info(),
                    from: ctx.accounts.escrow.to_account_info(),
                    authority: ctx.accounts.authority.to_account_info(),
                },
                &[auth_seeds],
            ),
            escrowed,
        )?;
    }

    let now = Clock::get()?.unix_timestamp;
    let used_index = index_balance - util::token_amount(p.ours(ii))?;
    let used_quote = quote_balance - util::token_amount(p.ours(qi))?;
    let index_sqrt = math::flip(ps.sqrt_price, !index_is_0);
    let config = &mut ctx.accounts.config;
    let s = config.switch;
    if escrowed > 0 {
        config.total_burned += escrowed;
        config.quote_changes += 1;
    }
    config.active_quote = target;
    config.active_pool = pool_key;
    config.floor_sqrt = s.target_floor_sqrt;
    // The average restarts here, so the next pull waits out the warm-up: a natural cooldown.
    config.pool_ema = Ema { sqrt_price: index_sqrt, last_ts: now, streak_start: now };
    config.switch = Switch::default();
    emit!(Switched {
        requester: s.requester,
        quote: target,
        pool: pool_key,
        index_sqrt,
        index_amount: used_index,
        quote_amount: used_quote,
        burned: escrowed,
    });
    Ok(())
}

// =============================================================================================
// abort

#[derive(Accounts)]
pub struct Abort<'info> {
    #[account(mut, has_one = mint)]
    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer.
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    /// CHECK: our mint (has_one on config).
    pub mint: UncheckedAccount<'info>,

    /// CHECK: PDA.
    #[account(mut, seeds = [ESCROW_SEED], bump = config.escrow_bump)]
    pub escrow: UncheckedAccount<'info>,

    /// CHECK: must be the requester's associated token account (checked in the handler), so a
    /// refund can only go back to whoever burned.
    #[account(mut)]
    pub requester_token: UncheckedAccount<'info>,

    #[account(constraint = old_quote.mint == config.active_quote @ E::WrongQuote)]
    pub old_quote: Box<Account<'info, QuoteEntry>>,

    #[account(constraint = holding.mint == config.switch.holding @ E::WrongQuote)]
    pub holding: Box<Account<'info, QuoteEntry>>,

    #[account(constraint = wsol_quote.mint == config.wsol @ E::WrongQuote)]
    pub wsol_quote: Box<Account<'info, QuoteEntry>>,

    /// CHECK: the holding quote's mint.
    #[account(address = config.switch.holding)]
    pub holding_mint: UncheckedAccount<'info>,
    /// CHECK: the authority's ATA for the holding quote.
    pub holding_account: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
}

pub fn abort(ctx: Context<Abort>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let config = &ctx.accounts.config;
    let s = config.switch;
    require!(matches!(s.phase, Phase::Requested | Phase::Swapping), E::WrongPhase);
    require!(now > s.deadline, E::NotExpired);
    util::require_ata(&ctx.accounts.requester_token, &s.requester, &ctx.accounts.mint)?;

    let bump = [config.authority_bump];
    let auth_seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump];
    token::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            Transfer {
                from: ctx.accounts.escrow.to_account_info(),
                to: ctx.accounts.requester_token.to_account_info(),
                authority: ctx.accounts.authority.to_account_info(),
            },
            &[auth_seeds],
        ),
        s.escrowed,
    )?;

    let landed = if s.phase == Phase::Requested {
        ctx.accounts.config.switch = Switch::default();
        config_active(&ctx.accounts.config)
    } else {
        util::require_ata(&ctx.accounts.holding_account, &ctx.accounts.authority.key(), &ctx.accounts.holding_mint)?;
        let final_amount = util::token_amount(&ctx.accounts.holding_account)?;
        // Averages may be stale here; abort must always be possible, and they only matter for dust.
        let usd_old = usd_sqrt(&ctx.accounts.old_quote, &ctx.accounts.wsol_quote, config, now, false)?;
        let usd_holding = usd_sqrt(&ctx.accounts.holding, &ctx.accounts.wsol_quote, config, now, false)?;
        let config = &mut ctx.accounts.config;
        config.switch.escrowed = 0;
        config.switch.target = s.holding;
        config.switch.ema_rate_sqrt = math::div_sqrt(usd_old, usd_holding);
        arrive(config, final_amount);
        s.holding
    };
    emit!(SwitchAborted { requester: s.requester, wanted: s.target, landed });
    Ok(())
}

fn config_active(config: &Config) -> Pubkey {
    config.active_quote
}
