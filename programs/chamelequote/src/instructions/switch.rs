//! Quote switching. A user burns `burn_amount` (held in escrow until the switch lands) to request
//! a new quote; then anyone cranks it through:
//!
//!   request -> pull -> hop (1-3x) -> reprice (0+x) -> add
//!
//! `pull` takes all liquidity out of the active pool, `hop` swaps the backing one route pool at a
//! time (quote -> hub -> [other hub] -> target), `reprice` moves the target pool to the translated
//! price, and `add` lays the liquidity back as two single-sided positions: every index token above
//! the price, every quote token between the floor and the price. The index keeps its value: its
//! price and floor are multiplied by the realised exchange rate.
//!
//! The crank steps go in separate transactions (they do not fit one), ideally as one Jito bundle.
//! While a switch is in flight the pool is empty, so there is nothing to trade against. A switch
//! not finished by its deadline can be aborted by anyone: the burn is refunded and the backing is
//! laid into whatever quote it is held in at that point.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Token, Transfer};

use crate::{
    error::ChameleonError as E,
    instructions::oracle,
    math,
    state::*,
    util,
    whirlpool::{self as wp, PoolState, Sides},
};

pub fn position_mint_address(pool: &Pubkey, index: u8) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[POSITION_MINT_SEED, pool.as_ref(), &[index]], &crate::ID)
}

/// Ticks and liquidity for the two positions `add` opens, given the pool at its target price.
/// Public so clients can find the tick arrays to pass.
pub fn plan_positions(
    index_is_a: bool,
    pool: &PoolState,
    floor_index_sqrt: u128,
    index_balance: u64,
    quote_balance: u64,
) -> [(i32, i32, u128); 2] {
    let ts = pool.tick_spacing as i32;
    let max_t = math::max_usable_tick(ts);
    let below = math::align_down(pool.tick_current, ts);
    let above = below + ts;
    let floor_tick = math::align_down(math::tick_at_sqrt_price(math::flip(floor_index_sqrt, !index_is_a)), ts)
        .clamp(-max_t, max_t);
    let s = math::sqrt_price_at_tick;
    let ranges = if index_is_a {
        [(above, max_t, true, index_balance), (floor_tick, below, false, quote_balance)]
    } else {
        [(-max_t, below, false, index_balance), (above, floor_tick, true, quote_balance)]
    };
    ranges.map(|(lo, hi, token_a, amount)| {
        if lo >= hi || amount == 0 {
            return (lo, hi, 0);
        }
        let l = if token_a { math::liquidity_for_a(amount, s(lo), s(hi)) } else { math::liquidity_for_b(amount, s(lo), s(hi)) };
        // Our tick math may differ from Orca's by a few ulps; keep a margin so the deposit fits.
        (lo, hi, l.saturating_sub(l / 1_000_000 + 1))
    })
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
    fn load(&self, authority: &Pubkey) -> Result<(PoolState, Sides)> {
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
        let sides = Sides {
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

/// Our two position slots in a pool (mint, position, NFT account, tick arrays at both ends).
#[derive(Accounts)]
pub struct Positions<'info> {
    /// CHECK: PDA, verified against the pool.
    #[account(mut)]
    pub mint_0: UncheckedAccount<'info>,
    /// CHECK: Orca position PDA.
    #[account(mut)]
    pub position_0: UncheckedAccount<'info>,
    /// CHECK: authority's token-2022 ATA for mint_0.
    #[account(mut)]
    pub nft_0: UncheckedAccount<'info>,
    /// CHECK: Orca validates.
    #[account(mut)]
    pub lower_0: UncheckedAccount<'info>,
    /// CHECK: Orca validates.
    #[account(mut)]
    pub upper_0: UncheckedAccount<'info>,
    /// CHECK: PDA, verified against the pool.
    #[account(mut)]
    pub mint_1: UncheckedAccount<'info>,
    /// CHECK: Orca position PDA.
    #[account(mut)]
    pub position_1: UncheckedAccount<'info>,
    /// CHECK: authority's token-2022 ATA for mint_1.
    #[account(mut)]
    pub nft_1: UncheckedAccount<'info>,
    /// CHECK: Orca validates.
    #[account(mut)]
    pub lower_1: UncheckedAccount<'info>,
    /// CHECK: Orca validates.
    #[account(mut)]
    pub upper_1: UncheckedAccount<'info>,
}

impl<'info> Positions<'info> {
    /// (position mint, position account, lower tick array, upper tick array) for slot `i`,
    /// after checking the mint is our PDA for `pool` and the position is the one Orca derives.
    fn slot(&self, pool: &Pubkey, i: u8) -> Result<(Pubkey, &AccountInfo<'info>, Pubkey, Pubkey)> {
        let (mint, position, lower, upper) = if i == 0 {
            (&self.mint_0, &self.position_0, &self.lower_0, &self.upper_0)
        } else {
            (&self.mint_1, &self.position_1, &self.lower_1, &self.upper_1)
        };
        require_keys_eq!(mint.key(), position_mint_address(pool, i).0, E::BadPosition);
        require_keys_eq!(position.key(), wp::position_address(&mint.key()), E::BadPosition);
        Ok((mint.key(), position.as_ref(), lower.key(), upper.key()))
    }
}

fn expected_pool(config: &Config, quote: &Pubkey) -> Pubkey {
    let (a, b) = if config.index_is_a(quote) { (config.mint, *quote) } else { (*quote, config.mint) };
    wp::whirlpool_address(&config.whirlpools_config, &a, &b, config.tick_spacing)
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
    /// Receives the rent of the closed positions.
    #[account(mut)]
    pub cranker: Signer<'info>,

    #[account(mut)]
    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer.
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    pub pool: PoolSides<'info>,
    pub positions: Positions<'info>,

    /// CHECK: fee recipient's ATA for mint A (only checked when a share is paid).
    #[account(mut)]
    pub fee_a: UncheckedAccount<'info>,
    /// CHECK: fee recipient's ATA for mint B (only checked when a share is paid).
    #[account(mut)]
    pub fee_b: UncheckedAccount<'info>,

    /// CHECK: address checked.
    #[account(address = wp::TOKEN_2022_ID)]
    pub token_2022_program: UncheckedAccount<'info>,
    /// CHECK: address checked.
    #[account(address = wp::MEMO_ID)]
    pub memo_program: UncheckedAccount<'info>,
    /// CHECK: address checked.
    #[account(address = wp::WHIRLPOOL_ID)]
    pub whirlpool_program: UncheckedAccount<'info>,
}

pub fn pull<'info>(ctx: Context<'info, Pull<'info>>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let config = &ctx.accounts.config;
    require!(config.switch.phase == Phase::Requested, E::WrongPhase);
    let pool_key = ctx.accounts.pool.whirlpool.key();
    require_keys_eq!(pool_key, config.active_pool, E::WrongPool);
    let authority = ctx.accounts.authority.key();
    let (ps, sides) = ctx.accounts.pool.load(&authority)?;
    let quote = config.active_quote;
    let index_is_a = config.index_is_a(&quote);

    // Refuse to pull on a price pushed away from its average (the LOOP "block start" check).
    let spot = math::flip(ps.sqrt_price, !index_is_a);
    let ema = config.pool_ema.checked(now)?;
    require!(within_bps(spot, ema, config.max_price_move_bps), E::PoolManipulated);

    let infos = ctx.accounts.to_account_infos();
    let bump = [config.authority_bump];
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump];
    let (ours_a, ours_b) = (ctx.accounts.pool.ours_a.to_account_info(), ctx.accounts.pool.ours_b.to_account_info());
    let mut fees = (0u64, 0u64);

    for i in 0..2u8 {
        let (mint, position, lower, upper) = ctx.accounts.positions.slot(&pool_key, i)?;
        let Some(st) = wp::read_position(position)? else { continue };
        if st.liquidity > 0 {
            let ix = wp::modify_liquidity_ix(false, pool_key, authority, mint, &sides, lower, upper, st.liquidity, 0, 0);
            wp::invoke(&ix, &infos, &[seeds])?;
        }
        let (a0, b0) = (util::token_amount(&ours_a)?, util::token_amount(&ours_b)?);
        wp::invoke(&wp::collect_fees_ix(pool_key, authority, mint, &sides), &infos, &[seeds])?;
        fees.0 += util::token_amount(&ours_a)? - a0;
        fees.1 += util::token_amount(&ours_b)? - b0;
        let receiver = ctx.accounts.cranker.key();
        wp::invoke(&wp::close_position_ix(authority, receiver, mint), &infos, &[seeds])?;
    }

    pay_fee_share(config, &ctx.accounts.pool, &ctx.accounts.fee_a, &ctx.accounts.fee_b, &ctx.accounts.authority, fees, seeds)?;

    let start = util::token_amount(if index_is_a { &ours_b } else { &ours_a })?;
    let config = &mut ctx.accounts.config;
    config.switch.old_index_sqrt = spot;
    config.switch.start_amount = start;
    config.switch.holding = quote;
    config.switch.phase = Phase::Swapping;
    Ok(())
}

/// Sends `fee_share_bps` of collected fees (raw amounts of mint A, mint B) to the fee recipient.
/// The rest stays in the program's accounts and becomes backing at the next lay-out.
fn pay_fee_share<'info>(
    config: &Config,
    p: &PoolSides<'info>,
    fee_a: &UncheckedAccount<'info>,
    fee_b: &UncheckedAccount<'info>,
    authority: &UncheckedAccount<'info>,
    fees: (u64, u64),
    seeds: &[&[u8]],
) -> Result<()> {
    if config.fee_share_bps == 0 {
        return Ok(());
    }
    let share = |f: u64| (f as u128 * config.fee_share_bps as u128 / 10_000) as u64;
    for (fee, ours, fee_acct, mint, program) in [
        (fees.0, &p.ours_a, fee_a, &p.mint_a, &p.token_program_a),
        (fees.1, &p.ours_b, fee_b, &p.mint_b, &p.token_program_b),
    ] {
        let amount = share(fee);
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

    pub pool: PoolSides<'info>,
    pub positions: Positions<'info>,

    /// CHECK: fee recipient's ATA for mint A (checked when a share is paid).
    #[account(mut)]
    pub fee_a: UncheckedAccount<'info>,
    /// CHECK: fee recipient's ATA for mint B (checked when a share is paid).
    #[account(mut)]
    pub fee_b: UncheckedAccount<'info>,

    /// CHECK: address checked.
    #[account(address = wp::MEMO_ID)]
    pub memo_program: UncheckedAccount<'info>,
    /// CHECK: address checked.
    #[account(address = wp::WHIRLPOOL_ID)]
    pub whirlpool_program: UncheckedAccount<'info>,
}

/// Collects the trading fees the live positions have earned and pays the fee recipient its
/// share, without touching liquidity. Anyone can call it; the keeper does so periodically.
pub fn claim_fees<'info>(ctx: Context<'info, ClaimFees<'info>>) -> Result<()> {
    let config = &ctx.accounts.config;
    require!(config.switch.phase == Phase::Idle, E::WrongPhase);
    let pool_key = ctx.accounts.pool.whirlpool.key();
    require_keys_eq!(pool_key, config.active_pool, E::WrongPool);
    let authority = ctx.accounts.authority.key();
    let (_, sides) = ctx.accounts.pool.load(&authority)?;
    let infos = ctx.accounts.to_account_infos();
    let bump = [config.authority_bump];
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump];
    let (ours_a, ours_b) = (ctx.accounts.pool.ours_a.to_account_info(), ctx.accounts.pool.ours_b.to_account_info());
    let mut fees = (0u64, 0u64);
    for i in 0..2u8 {
        let (mint, position, lower, upper) = ctx.accounts.positions.slot(&pool_key, i)?;
        let Some(st) = wp::read_position(position)? else { continue };
        if st.liquidity == 0 {
            continue;
        }
        wp::invoke(&wp::update_fees_ix(pool_key, mint, lower, upper), &infos, &[])?;
        let (a0, b0) = (util::token_amount(&ours_a)?, util::token_amount(&ours_b)?);
        wp::invoke(&wp::collect_fees_ix(pool_key, authority, mint, &sides), &infos, &[seeds])?;
        fees.0 += util::token_amount(&ours_a)? - a0;
        fees.1 += util::token_amount(&ours_b)? - b0;
    }
    pay_fee_share(config, &ctx.accounts.pool, &ctx.accounts.fee_a, &ctx.accounts.fee_b, &ctx.accounts.authority, fees, seeds)
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
    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer.
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

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

/// Moves the target pool to the target price by swapping up to that price limit. With no one
/// else's liquidity in the way this costs nothing; liquidity in the way gets traded against at
/// prices better than the target. One call crosses at most three tick arrays, so a far-off pool
/// may need several calls.
pub fn reprice<'info>(ctx: Context<'info, Reprice<'info>>) -> Result<()> {
    let config = &ctx.accounts.config;
    require!(config.switch.phase == Phase::Repricing, E::WrongPhase);
    let target = config.switch.target;
    let pool_key = ctx.accounts.pool.whirlpool.key();
    require_keys_eq!(pool_key, expected_pool(config, &target), E::WrongPool);
    let authority = ctx.accounts.authority.key();
    let (ps, sides) = ctx.accounts.pool.load(&authority)?;
    let target_sqrt = math::flip(config.switch.target_index_sqrt, !config.index_is_a(&target));
    if ps.sqrt_price == target_sqrt {
        return Ok(());
    }
    let a_to_b = ps.sqrt_price > target_sqrt;
    let p = &ctx.accounts.pool;
    let amount = util::token_amount(if a_to_b { &p.ours_a } else { &p.ours_b })?;
    require!(amount > 0, E::NothingToReprice);
    // A swap that runs past its three tick arrays fails instead of stopping, so stop this call at
    // the edge of the window they cover; the next call continues from there.
    let ts = ps.tick_spacing as i32;
    let span = ts * math::TICK_ARRAY_SIZE;
    let start = math::tick_array_start(ps.tick_current, ts);
    let limit = if a_to_b {
        target_sqrt.max(math::sqrt_price_at_tick((start - 2 * span).max(math::MIN_TICK)))
    } else {
        target_sqrt.min(math::sqrt_price_at_tick((start + 3 * span - ts).min(math::MAX_TICK)))
    };
    let tick_arrays = [ctx.accounts.tick_array_0.key(), ctx.accounts.tick_array_1.key(), ctx.accounts.tick_array_2.key()];
    let ix = wp::swap_ix(pool_key, authority, &sides, tick_arrays, amount, 0, limit, a_to_b);
    let bump = [config.authority_bump];
    wp::invoke(&ix, &ctx.accounts.to_account_infos(), &[&[AUTHORITY_SEED, &bump]])
}

// =============================================================================================
// add

#[derive(Accounts)]
pub struct Add<'info> {
    /// Pays the rent of the new positions (refunded to whoever pulls them next).
    #[account(mut)]
    pub funder: Signer<'info>,

    #[account(mut, has_one = mint)]
    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer.
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    pub pool: PoolSides<'info>,
    pub positions: Positions<'info>,

    /// CHECK: PDA.
    #[account(mut, seeds = [ESCROW_SEED], bump = config.escrow_bump)]
    pub escrow: UncheckedAccount<'info>,

    /// CHECK: our mint (has_one on config).
    #[account(mut)]
    pub mint: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    /// CHECK: address checked.
    #[account(address = wp::TOKEN_2022_ID)]
    pub token_2022_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: address checked.
    #[account(address = wp::ATA_PROGRAM_ID)]
    pub associated_token_program: UncheckedAccount<'info>,
    /// CHECK: address checked.
    #[account(address = wp::MEMO_ID)]
    pub memo_program: UncheckedAccount<'info>,
    /// CHECK: address checked.
    #[account(address = wp::NFT_UPDATE_AUTH)]
    pub nft_update_auth: UncheckedAccount<'info>,
    /// CHECK: address checked.
    #[account(address = wp::WHIRLPOOL_ID)]
    pub whirlpool_program: UncheckedAccount<'info>,
}

pub fn add<'info>(ctx: Context<'info, Add<'info>>) -> Result<()> {
    let config = &ctx.accounts.config;
    require!(config.switch.phase == Phase::Repricing, E::WrongPhase);
    let target = config.switch.target;
    let pool_key = ctx.accounts.pool.whirlpool.key();
    require_keys_eq!(pool_key, expected_pool(config, &target), E::WrongPool);
    let authority = ctx.accounts.authority.key();
    let (ps, sides) = ctx.accounts.pool.load(&authority)?;
    let index_is_a = config.index_is_a(&target);
    require!(
        ps.sqrt_price == math::flip(config.switch.target_index_sqrt, !index_is_a),
        E::NotAtTarget
    );

    let p = &ctx.accounts.pool;
    let (ours_index, ours_quote) = if index_is_a { (&p.ours_a, &p.ours_b) } else { (&p.ours_b, &p.ours_a) };
    let index_balance = util::token_amount(ours_index)?;
    let quote_balance = util::token_amount(ours_quote)?;
    let plan = plan_positions(index_is_a, &ps, config.switch.target_floor_sqrt, index_balance, quote_balance);

    let infos = ctx.accounts.to_account_infos();
    let bump = [config.authority_bump];
    let auth_seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump];
    for (i, (lo, hi, liquidity)) in plan.into_iter().enumerate() {
        let i = i as u8;
        let (mint, position, lower, upper) = ctx.accounts.positions.slot(&pool_key, i)?;
        require!(position.data_is_empty(), E::BadPosition);
        if liquidity == 0 {
            continue;
        }
        let mint_bump = [position_mint_address(&pool_key, i).1];
        let idx = [i];
        let mint_seeds: &[&[u8]] = &[POSITION_MINT_SEED, pool_key.as_ref(), &idx, &mint_bump];
        let open = wp::open_position_ix(ctx.accounts.funder.key(), authority, mint, pool_key, lo, hi);
        wp::invoke(&open, &infos, &[mint_seeds])?;
        let (max_a, max_b) = (util::token_amount(&p.ours_a)?, util::token_amount(&p.ours_b)?);
        let inc = wp::modify_liquidity_ix(true, pool_key, authority, mint, &sides, lower, upper, liquidity, max_a, max_b);
        wp::invoke(&inc, &infos, &[auth_seeds])?;
    }

    let escrowed = config.switch.escrowed;
    if escrowed > 0 {
        token::burn(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
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
    let used_index = index_balance - util::token_amount(ours_index)?;
    let used_quote = quote_balance - util::token_amount(ours_quote)?;
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
    config.pool_ema = Ema { sqrt_price: s.target_index_sqrt, last_ts: now, streak_start: now };
    config.switch = Switch::default();
    emit!(Switched {
        requester: s.requester,
        quote: target,
        pool: pool_key,
        index_sqrt: s.target_index_sqrt,
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
