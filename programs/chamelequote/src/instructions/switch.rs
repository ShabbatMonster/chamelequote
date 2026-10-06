//! Quote switching. A user burns `burn_amount` (held in escrow until the switch lands) to request
//! a new quote; then anyone cranks it through:
//!
//!   request -> pull -> hop (1-3x) -> [seed] -> [reprice] -> add
//!
//! `pull` takes the liquidity out of the active pool, `hop` swaps the backing one route pool at a
//! time (quote -> hub -> [other hub] -> target), and `add` lays it back into the target pool at
//! the translated price: every index token between the price and the top, every quote token
//! between the floor and the price. The index keeps its value: its price and floor are multiplied
//! by the realised exchange rate.
//!
//! Our pools are Meteora DAMM v2 customizable pools (one per quote, our token as token A); route
//! pools are Orca Whirlpools. `add` creates a new pool at the target price with the floor as its
//! lower bound. A pool the coin has used before keeps its range and price: `reprice` swaps it to
//! the target through the "sentinel", a small position that `seed` leaves in every pool for good
//! (right after the switch that created it), since a pool with no liquidity cannot be moved.
//!
//! `pull` only goes through in a transaction that also runs `add`, so a switch lands whole or not
//! at all: the coin is never without liquidity. A switch that can't land by its deadline is
//! aborted by anyone and the burn refunded.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{program::invoke, system_instruction};
use solana_instructions_sysvar as ix_sysvar;
use anchor_lang::Discriminator;
use anchor_spl::token::{self, Burn, Token, Transfer};

use crate::{
    damm,
    error::ChameleonError as E,
    instructions::oracle,
    math,
    state::*,
    util,
    whirlpool::{self as wp, PoolState},
};

pub fn position_mint_address(pool: &Pubkey, index: u8) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[POSITION_MINT_SEED, pool.as_ref(), &[index]], &crate::ID)
}

/// Our pool for `quote`.
pub fn expected_pool(config: &Config, quote: &Pubkey) -> Pubkey {
    damm::pool_address(&config.mint, quote)
}

/// A target price kept strictly inside a pool's range (DAMM won't trade outside it).
pub fn clamp_to_range(sqrt_price: u128, pool: &damm::PoolState) -> u128 {
    sqrt_price.clamp(pool.sqrt_min + pool.sqrt_min / 10_000 + 1, pool.sqrt_max - pool.sqrt_max / 10_000)
}

/// Liquidity for `index` and `quote` in a pool at `sqrt_price` over [sqrt_min, sqrt_max]: the
/// most both can fund, less a hair so the deposit always fits.
pub fn liquidity_for(index: u64, quote: u64, sqrt_price: u128, sqrt_min: u128, sqrt_max: u128) -> u128 {
    let l = damm::liquidity_from_a(index, sqrt_price, sqrt_max).min(damm::liquidity_from_b(quote, sqrt_min, sqrt_price));
    l.saturating_sub(l / 1_000_000 + 1)
}

/// One of our DAMM v2 pools (which may not exist yet) plus the program's token account for each
/// side. Token A is always our token.
#[derive(Accounts)]
pub struct OurPool<'info> {
    /// CHECK: checked against the quote in `load`, which also checks owner and contents.
    #[account(mut)]
    pub pool: UncheckedAccount<'info>,
    /// CHECK: our mint (token A); checked against the config.
    pub index_mint: UncheckedAccount<'info>,
    /// CHECK: the quote's mint (token B).
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: must be the authority's ATA for our token.
    #[account(mut)]
    pub ours_index: UncheckedAccount<'info>,
    /// CHECK: must be the authority's ATA for the quote.
    #[account(mut)]
    pub ours_quote: UncheckedAccount<'info>,
    /// CHECK: must be the pool's vault A.
    #[account(mut)]
    pub vault_a: UncheckedAccount<'info>,
    /// CHECK: must be the pool's vault B.
    #[account(mut)]
    pub vault_b: UncheckedAccount<'info>,
}

impl<'info> OurPool<'info> {
    /// The pool's state (None if not created yet) and its sides, after checking it is our pool
    /// for `quote`, the vaults are its own and the token accounts are the authority's.
    fn load(&self, config: &Config, quote: &Pubkey, authority: &Pubkey) -> Result<(Option<damm::PoolState>, damm::Sides)> {
        let key = self.pool.key();
        require!(
            self.index_mint.key() == config.mint && self.quote_mint.key() == *quote && key == expected_pool(config, quote),
            E::WrongPool
        );
        let state = if self.pool.data_is_empty() { None } else { Some(damm::read_pool(&self.pool)?) };
        let vaults = match &state {
            Some(p) => {
                // Token A must be ours; a pool someone else set up the other way round is not.
                require!(p.mint_a == config.mint && p.mint_b == *quote, E::WrongPool);
                [p.vault_a, p.vault_b]
            }
            None => [damm::vault_address(&key, &config.mint), damm::vault_address(&key, quote)],
        };
        require!(self.vault_a.key() == vaults[0] && self.vault_b.key() == vaults[1], E::WrongPool);
        util::require_ata(&self.ours_index, authority, &self.index_mint)?;
        util::require_ata(&self.ours_quote, authority, &self.quote_mint)?;
        let sides = damm::Sides {
            pool: key,
            mint_a: config.mint,
            mint_b: *quote,
            program_a: *self.index_mint.owner,
            program_b: *self.quote_mint.owner,
            ours_a: self.ours_index.key(),
            ours_b: self.ours_quote.key(),
            vault_a: vaults[0],
            vault_b: vaults[1],
        };
        Ok((state, sides))
    }

    fn balances(&self) -> Result<(u64, u64)> {
        Ok((util::token_amount(&self.ours_index)?, util::token_amount(&self.ours_quote)?))
    }
}

/// One position slot: NFT mint (our PDA) and DAMM's NFT account and position for it.
#[derive(Accounts)]
pub struct Slot<'info> {
    /// CHECK: PDA, verified against the pool.
    #[account(mut)]
    pub nft_mint: UncheckedAccount<'info>,
    /// CHECK: DAMM's NFT account PDA of the mint.
    #[account(mut)]
    pub nft_account: UncheckedAccount<'info>,
    /// CHECK: DAMM's position PDA of the mint.
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
}

impl<'info> Slot<'info> {
    /// Checks the NFT mint is our PDA for (`pool`, `i`) and the other two are DAMM's for it.
    /// Returns the position's liquidity, None if it is not open.
    fn check(&self, pool: &Pubkey, i: u8) -> Result<Option<u128>> {
        let mint = self.nft_mint.key();
        require!(
            mint == position_mint_address(pool, i).0
                && self.position.key() == damm::position_address(&mint)
                && self.nft_account.key() == damm::nft_account_address(&mint),
            E::BadPosition
        );
        Ok(damm::read_position(&self.position)?.map(|(_, l)| l))
    }

    fn mint_seeds(pool: &Pubkey, i: u8) -> ([u8; 1], [u8; 1]) {
        ([i], [position_mint_address(pool, i).1])
    }

    /// Rent of a new position here (position, NFT mint and NFT account).
    fn rent() -> Result<u64> {
        let rent = Rent::get()?;
        Ok(rent.minimum_balance(damm::POSITION_LEN)
            + rent.minimum_balance(damm::NFT_MINT_LEN)
            + rent.minimum_balance(damm::TOKEN_ACCOUNT_LEN))
    }
}

/// Programs DAMM wants to see, plus its two fixed PDAs.
#[derive(Accounts)]
pub struct Programs<'info> {
    pub token_program: Program<'info, Token>,
    /// CHECK: address checked.
    #[account(address = wp::TOKEN_2022_ID)]
    pub token_2022_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: address checked.
    #[account(address = damm::DAMM_ID)]
    pub damm_program: UncheckedAccount<'info>,
    /// CHECK: DAMM's pool authority; DAMM checks it.
    pub pool_authority: UncheckedAccount<'info>,
    /// CHECK: DAMM's event authority; DAMM checks it.
    pub event_authority: UncheckedAccount<'info>,
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

/// DAMM makes the payer of a pool or position the one who funds it, and ours are funded by the
/// authority PDA, so the funder tops the authority up first. Closed positions refund the
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

/// Collects a position's fees into the authority's accounts; returns (index, quote) collected.
fn claim<'info>(
    pool: &OurPool<'info>,
    sides: &damm::Sides,
    nft_mint: Pubkey,
    authority: Pubkey,
    infos: &[AccountInfo<'info>],
    seeds: &[&[u8]],
) -> Result<(u64, u64)> {
    let before = pool.balances()?;
    wp::invoke(&damm::claim_fee_ix(authority, nft_mint, sides), infos, &[seeds])?;
    let after = pool.balances()?;
    Ok((after.0 - before.0, after.1 - before.1))
}

/// Pays the fee recipient its share of fees just collected from `pool`.
fn pay_pool_fees<'info>(
    config: &Config,
    pool: &OurPool<'info>,
    fee_index: &AccountInfo<'info>,
    fee_quote: &AccountInfo<'info>,
    programs: &Programs<'info>,
    authority: &AccountInfo<'info>,
    fees: (u64, u64),
    seeds: &[&[u8]],
) -> Result<()> {
    let (mi, mq) = (pool.index_mint.to_account_info(), pool.quote_mint.to_account_info());
    pay_fee_share(
        config,
        [
            (fees.0, pool.ours_index.as_ref(), fee_index, &mi, &programs.for_mint(&mi)),
            (fees.1, pool.ours_quote.as_ref(), fee_quote, &mq, &programs.for_mint(&mq)),
        ],
        authority,
        seeds,
    )
}

/// Both sides of an Orca route pool plus the program's token account for each.
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
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(mut, has_one = mint, has_one = fee_recipient)]
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

    /// CHECK: our pool for the new quote (address checked); may not exist yet.
    #[account(address = expected_pool(&config, &new_quote.mint) @ E::WrongPool)]
    pub target_pool: UncheckedAccount<'info>,

    /// CHECK: receives the new-pool fee (has_one on config).
    #[account(mut)]
    pub fee_recipient: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn request_switch(ctx: Context<RequestSwitch>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let config = &ctx.accounts.config;
    require!(config.active_quote != Pubkey::default(), E::NotLaunched);
    require!(config.switch.phase == Phase::Idle, E::WrongPhase);
    let (old, new, wsol) = (&ctx.accounts.old_quote, &ctx.accounts.new_quote, &ctx.accounts.wsol_quote);
    require!(new.enabled, E::QuoteDisabled);
    // The pull needs the pool's average; refuse now rather than escrow a burn that would wait.
    let ema = config.pool_ema;
    require!(ema.last_ts != 0 && now - ema.last_ts <= MAX_POKE_GAP, E::StalePrice);
    require!(ema.is_valid(now), E::CoolingDown);
    require_keys_neq!(new.mint, config.active_quote, E::SameQuote);

    let usd_old = usd_sqrt(old, wsol, config, now, true)?;
    let usd_new = usd_sqrt(new, wsol, config, now, true)?;
    // new per old = (USDC per old) / (USDC per new)
    let ema_rate_sqrt = math::div_sqrt(usd_old, usd_new);
    let realised_min = math::quote_out(MIN_REALISED_VALUE_USDC as u64, math::invert_sqrt(usd_old)).min(u64::MAX as u128);

    let target_pool = &ctx.accounts.target_pool;
    if target_pool.data_is_empty() {
        // A new pool: the requester pays for the accounts it leaves behind.
        invoke(
            &system_instruction::transfer(&ctx.accounts.user.key(), &config.fee_recipient, NEW_POOL_FEE_LAMPORTS),
            &[
                ctx.accounts.user.to_account_info(),
                ctx.accounts.fee_recipient.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
            ],
        )?;
    } else {
        // Only a pool we created (our token as token A, our authority as creator) will do, and
        // its fixed floor must leave room for the price the coin will land at.
        let p = damm::read_pool(target_pool)?;
        let authority = Pubkey::create_program_address(&[AUTHORITY_SEED, &[config.authority_bump]], &crate::ID)
            .map_err(|_| error!(E::InvalidParam))?;
        require!(p.creator == authority && p.mint_a == config.mint, E::ForeignPool);
        let landing = math::mul_sqrt(config.pool_ema.checked(now)?, ema_rate_sqrt);
        // A hair below is fine: `add` lands it just inside the range (the coin is at its floor).
        require!(landing >= p.sqrt_min - p.sqrt_min / 10_000, E::BelowPoolFloor);
    }

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

    /// CHECK: PDA signer; receives the closed position's rent (reused at the next `add`).
    #[account(mut, seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    pub pool: OurPool<'info>,
    pub position: Slot<'info>,

    /// CHECK: fee recipient's ATA for our token (checked when a share is paid).
    #[account(mut)]
    pub fee_index: UncheckedAccount<'info>,
    /// CHECK: fee recipient's ATA for the quote (checked when a share is paid).
    #[account(mut)]
    pub fee_quote: UncheckedAccount<'info>,

    /// CHECK: address checked; read to find the `add` this pull must come with.
    #[account(address = ix_sysvar::ID)]
    pub instructions: UncheckedAccount<'info>,

    pub programs: Programs<'info>,
}

/// Whether one of this program's `add` instructions comes later in the current transaction.
fn add_follows(instructions: &AccountInfo) -> Result<bool> {
    let current = ix_sysvar::load_current_index_checked(instructions)? as usize;
    let mut i = current + 1;
    while let Ok(ix) = ix_sysvar::load_instruction_at_checked(i, instructions) {
        if ix.program_id == crate::ID && ix.data.starts_with(crate::instruction::Add::DISCRIMINATOR) {
            return Ok(true);
        }
        i += 1;
    }
    Ok(false)
}

/// Takes the liquidity out of the active pool and closes the position (the sentinel stays).
/// Fees earned since the last claim are paid out as usual. Only in a transaction that also
/// runs `add`: the switch lands whole or not at all.
pub fn pull<'info>(ctx: Context<'info, Pull<'info>>) -> Result<()> {
    let config = &ctx.accounts.config;
    require!(config.switch.phase == Phase::Requested, E::WrongPhase);
    let pool_key = ctx.accounts.pool.pool.key();
    require_keys_eq!(pool_key, config.active_pool, E::WrongPool);
    require!(add_follows(&ctx.accounts.instructions)?, E::NotAtomic);
    let authority = ctx.accounts.authority.key();
    let quote = config.active_quote;
    let (state, sides) = ctx.accounts.pool.load(config, &quote, &authority)?;
    let ps = state.ok_or(E::WrongPool)?;

    // Whatever the price is, traded or pushed, carries over: the new pool opens at this price
    // times the realised exchange rate, on the same curve. Pushing it before a switch and back
    // after only pays the pool fee twice, so the switch doesn't wait for the price to settle.
    // (The route pools the backing swaps through are still checked against their averages.)
    let spot = ps.sqrt_price;

    let infos = ctx.accounts.to_account_infos();
    let bump = [config.authority_bump];
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump];
    let nft = ctx.accounts.position.nft_mint.key();
    let mut fees = (0, 0);
    if ctx.accounts.position.check(&pool_key, MAIN_SLOT)?.is_some() {
        fees = claim(&ctx.accounts.pool, &sides, nft, authority, &infos, seeds)?;
        wp::invoke(&damm::remove_all_liquidity_ix(authority, nft, &sides), &infos, &[seeds])?;
        wp::invoke(&damm::close_position_ix(authority, nft, pool_key), &infos, &[seeds])?;
    }
    pay_pool_fees(
        config,
        &ctx.accounts.pool,
        &ctx.accounts.fee_index,
        &ctx.accounts.fee_quote,
        &ctx.accounts.programs,
        &ctx.accounts.authority,
        fees,
        seeds,
    )?;

    let start = util::token_amount(&ctx.accounts.pool.ours_quote)?;
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
    pub position: Slot<'info>,

    /// CHECK: fee recipient's ATA for our token (checked when a share is paid).
    #[account(mut)]
    pub fee_index: UncheckedAccount<'info>,
    /// CHECK: fee recipient's ATA for the quote (checked when a share is paid).
    #[account(mut)]
    pub fee_quote: UncheckedAccount<'info>,

    pub programs: Programs<'info>,
}

/// Collects the trading fees the live position has earned and pays the fee recipient its share,
/// without touching liquidity. Anyone can call it; the keeper does so periodically.
pub fn claim_fees<'info>(ctx: Context<'info, ClaimFees<'info>>) -> Result<()> {
    let config = &ctx.accounts.config;
    require!(matches!(config.switch.phase, Phase::Idle | Phase::Requested), E::WrongPhase);
    let pool_key = ctx.accounts.pool.pool.key();
    require_keys_eq!(pool_key, config.active_pool, E::WrongPool);
    let authority = ctx.accounts.authority.key();
    let (_, sides) = ctx.accounts.pool.load(config, &config.active_quote, &authority)?;
    require!(ctx.accounts.position.check(&pool_key, MAIN_SLOT)?.is_some(), E::BadPosition);
    let infos = ctx.accounts.to_account_infos();
    let bump = [config.authority_bump];
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump];
    let fees = claim(&ctx.accounts.pool, &sides, ctx.accounts.position.nft_mint.key(), authority, &infos, seeds)?;
    pay_pool_fees(
        config,
        &ctx.accounts.pool,
        &ctx.accounts.fee_index,
        &ctx.accounts.fee_quote,
        &ctx.accounts.programs,
        &ctx.accounts.authority,
        fees,
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
    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer.
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    pub pool: OurPool<'info>,
    pub programs: Programs<'info>,
}

/// Moves a pool the coin has used before to the target price by swapping through its sentinel
/// (and anyone else's liquidity, which gets traded against at prices better than the target).
/// A no-op for a pool that doesn't exist yet (`add` creates it at the target) or one already
/// within PRICE_TOLERANCE_BPS of it.
pub fn reprice<'info>(ctx: Context<'info, Reprice<'info>>) -> Result<()> {
    let config = &ctx.accounts.config;
    require!(config.switch.phase == Phase::Repricing, E::WrongPhase);
    let target = config.switch.target;
    let authority = ctx.accounts.authority.key();
    let (state, sides) = ctx.accounts.pool.load(config, &target, &authority)?;
    let Some(ps) = state else { return Ok(()) };
    let goal = clamp_to_range(config.switch.target_index_sqrt, &ps);
    if within_bps(ps.sqrt_price, goal, PRICE_TOLERANCE_BPS) {
        return Ok(());
    }
    require!(ps.liquidity > 0, E::NothingToReprice);
    let up = goal > ps.sqrt_price; // buying our token with the quote
    let (index, quote) = ctx.accounts.pool.balances()?;
    let have = if up { quote } else { index };
    let amount = damm::input_to_move(ps.liquidity, ps.sqrt_price, goal).min(have as u128) as u64;
    require!(amount > 0, E::NothingToReprice);
    let ix = damm::swap_ix(authority, &sides, !up, amount, 0);
    let bump = [config.authority_bump];
    wp::invoke(&ix, &ctx.accounts.to_account_infos(), &[&[AUTHORITY_SEED, &bump]])
}

// =============================================================================================
// seed

#[derive(Accounts)]
pub struct Seed<'info> {
    /// Tops up the authority for the position's rent.
    #[account(mut)]
    pub funder: Signer<'info>,

    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer; pays DAMM's rent.
    #[account(mut, seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    pub pool: OurPool<'info>,
    /// The pool's main position (read in the idle case to size the sentinel).
    pub position: Slot<'info>,
    pub sentinel: Slot<'info>,
    pub programs: Programs<'info>,
}

/// Opens a pool's sentinel, a full-range position that stays for good so `reprice` has something
/// to swap through when the coin comes back. Normally right after the switch that created the
/// pool (idle, on the active pool), from the share `add` set aside: at most 1/SENTINEL_DIVISOR
/// of the main position, so idle backing is never locked away. Also, mid-switch, for a pool the
/// coin used before that has none, at whatever price the pool has.
pub fn seed<'info>(ctx: Context<'info, Seed<'info>>) -> Result<()> {
    let config = &ctx.accounts.config;
    let idle = config.switch.phase == Phase::Idle;
    require!(idle || config.switch.phase == Phase::Repricing, E::WrongPhase);
    let quote_mint = if idle { config.active_quote } else { config.switch.target };
    let authority = ctx.accounts.authority.key();
    let (state, sides) = ctx.accounts.pool.load(config, &quote_mint, &authority)?;
    let ps = state.ok_or(E::WrongPool)?;
    let pool_key = sides.pool;
    if idle {
        require_keys_eq!(pool_key, config.active_pool, E::WrongPool);
    }
    if ctx.accounts.sentinel.check(&pool_key, SENTINEL_SLOT)?.is_some() {
        return Ok(());
    }
    let (index, quote) = ctx.accounts.pool.balances()?;
    let l = if idle {
        let main = ctx.accounts.position.check(&pool_key, MAIN_SLOT)?.ok_or(E::BadPosition)?;
        liquidity_for(index, quote, ps.sqrt_price, ps.sqrt_min, ps.sqrt_max).min(main / SENTINEL_DIVISOR as u128)
    } else {
        liquidity_for(sentinel_share(index), sentinel_share(quote), ps.sqrt_price, ps.sqrt_min, ps.sqrt_max)
    };
    require!(l > 0, E::NothingToReprice);
    fund_authority(&ctx.accounts.funder, &ctx.accounts.authority, &ctx.accounts.programs.system_program, Slot::rent()?)?;
    let infos = ctx.accounts.to_account_infos();
    let bump = [config.authority_bump];
    let auth_seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump];
    open(&ctx.accounts.sentinel, &sides, SENTINEL_SLOT, l, index, quote, authority, &infos, auth_seeds)
}

/// Opens slot `i` and adds `liquidity` to it, paying at most (max_index, max_quote).
#[allow(clippy::too_many_arguments)]
fn open<'info>(
    slot: &Slot<'info>,
    sides: &damm::Sides,
    i: u8,
    liquidity: u128,
    max_index: u64,
    max_quote: u64,
    authority: Pubkey,
    infos: &[AccountInfo<'info>],
    auth_seeds: &[&[u8]],
) -> Result<()> {
    require!(slot.check(&sides.pool, i)?.is_none(), E::BadPosition);
    let nft = slot.nft_mint.key();
    let (idx, bump) = Slot::mint_seeds(&sides.pool, i);
    let mint_seeds: &[&[u8]] = &[POSITION_MINT_SEED, sides.pool.as_ref(), &idx, &bump];
    wp::invoke(&damm::create_position_ix(authority, nft, sides.pool), infos, &[auth_seeds, mint_seeds])?;
    wp::invoke(&damm::add_liquidity_ix(authority, nft, sides, liquidity, max_index, max_quote), infos, &[auth_seeds])
}

// =============================================================================================
// add

#[derive(Accounts)]
pub struct Add<'info> {
    /// Tops up the authority for rent (the position's is refunded when it is pulled).
    #[account(mut)]
    pub funder: Signer<'info>,

    #[account(mut, has_one = mint)]
    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer; pays DAMM's rent.
    #[account(mut, seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    pub pool: OurPool<'info>,
    pub position: Slot<'info>,

    /// CHECK: PDA.
    #[account(mut, seeds = [ESCROW_SEED], bump = config.escrow_bump)]
    pub escrow: UncheckedAccount<'info>,

    /// CHECK: our mint (has_one on config).
    #[account(mut)]
    pub mint: UncheckedAccount<'info>,

    pub programs: Programs<'info>,
    // remaining_accounts: Meteora's token badges for our mint and the quote (new pools only).
}

/// Lays the backing and every index token into the target pool. A new pool is created at the
/// target price with the floor that the backing reaches as its lower bound (its sentinel's
/// share stays aside for `seed`). A pool used before keeps its range: it must be at the target already
/// (`reprice`), and whatever its range can't take of one side stays with the program until the
/// next switch.
pub fn add<'info>(ctx: Context<'info, Add<'info>>) -> Result<()> {
    let config = &ctx.accounts.config;
    require!(config.switch.phase == Phase::Repricing, E::WrongPhase);
    let target = config.switch.target;
    let authority = ctx.accounts.authority.key();
    let (state, sides) = ctx.accounts.pool.load(config, &target, &authority)?;
    let pool_key = sides.pool;
    let (index, quote) = ctx.accounts.pool.balances()?;
    let rent = Rent::get()?;
    let bump = [config.authority_bump];
    let auth_seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump];

    let (sqrt_price, sqrt_min) = match state {
        None => {
            let p = config.switch.target_index_sqrt.clamp(damm::MIN_SQRT_PRICE + 1, damm::MAX_SQRT_PRICE / 2);
            // DAMM moves at least one unit of each token into a new pool.
            require!(quote > 0 && index > 0, E::NothingToReprice);
            // The sentinel's share stays with the program for `seed`, which runs right after
            // (outside this transaction, which has to fit the whole switch). It needs some quote
            // of its own, with at least a unit left for the pool.
            let with_sentinel = quote >= 2;
            let (si, sq) = if with_sentinel { (sentinel_share(index), sentinel_share(quote).min(quote - 1)) } else { (0, 0) };
            let (main_index, main_quote) = (index - si, (quote - sq).max(1));
            let l = damm::liquidity_from_a(main_index, p, damm::MAX_SQRT_PRICE);
            let l = l.saturating_sub(l / 1_000_000 + 1);
            let sqrt_min = damm::floor_for(main_quote, l, p).min(p);
            let need = rent.minimum_balance(damm::POOL_LEN) + 2 * rent.minimum_balance(damm::TOKEN_ACCOUNT_LEN) + Slot::rent()?;
            fund_authority(&ctx.accounts.funder, &ctx.accounts.authority, &ctx.accounts.programs.system_program, need)?;
            let mut infos = ctx.accounts.to_account_infos();
            infos.extend_from_slice(ctx.remaining_accounts);
            let nft = ctx.accounts.position.nft_mint.key();
            require!(ctx.accounts.position.check(&pool_key, MAIN_SLOT)?.is_none(), E::BadPosition);
            let (idx, mb) = Slot::mint_seeds(&pool_key, MAIN_SLOT);
            let mint_seeds: &[&[u8]] = &[POSITION_MINT_SEED, pool_key.as_ref(), &idx, &mb];
            let ix = damm::create_pool_ix(authority, nft, &sides, sqrt_min, p, l);
            wp::invoke(&ix, &infos, &[auth_seeds, mint_seeds])?;
            (p, sqrt_min)
        }
        Some(ps) => {
            let goal = clamp_to_range(config.switch.target_index_sqrt, &ps);
            require!(within_bps(ps.sqrt_price, goal, PRICE_TOLERANCE_BPS), E::NotAtTarget);
            let l = liquidity_for(index, quote, ps.sqrt_price, ps.sqrt_min, ps.sqrt_max);
            require!(l > 0, E::NothingToReprice);
            fund_authority(&ctx.accounts.funder, &ctx.accounts.authority, &ctx.accounts.programs.system_program, Slot::rent()?)?;
            let infos = ctx.accounts.to_account_infos();
            open(&ctx.accounts.position, &sides, MAIN_SLOT, l, index, quote, authority, &infos, auth_seeds)?;
            (ps.sqrt_price, ps.sqrt_min)
        }
    };

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
    let (index_left, quote_left) = ctx.accounts.pool.balances()?;
    let config = &mut ctx.accounts.config;
    let s = config.switch;
    if escrowed > 0 {
        config.total_burned += escrowed;
        config.quote_changes += 1;
    }
    config.active_quote = target;
    config.active_pool = pool_key;
    config.floor_sqrt = sqrt_min;
    // The average restarts here from the price we just set, counted as already part warmed up:
    // the next switch can go after SWITCH_COOLDOWN of pokes.
    config.pool_ema = Ema { sqrt_price, last_ts: now, streak_start: now - (EMA_WARMUP - SWITCH_COOLDOWN) };
    config.switch = Switch::default();
    emit!(Switched {
        requester: s.requester,
        quote: target,
        pool: pool_key,
        index_sqrt: sqrt_price,
        index_amount: index - index_left,
        quote_amount: quote - quote_left,
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
