//! The one step that still touches an Orca pool of ours: pulling the liquidity out of the
//! Whirlpool it lived in before the move to Raydium. The next switch after the upgrade runs
//! `pull_legacy` instead of `pull`; everything after that is Raydium. Can go in a later upgrade.

use anchor_lang::prelude::*;

use crate::{
    error::ChameleonError as E,
    math,
    state::*,
    util,
    whirlpool as wp,
};

use super::switch::*;

/// Our two position slots in an Orca pool (mint, position, NFT account, tick arrays at both ends).
#[derive(Accounts)]
pub struct OrcaPositions<'info> {
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

impl<'info> OrcaPositions<'info> {
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

#[derive(Accounts)]
pub struct PullLegacy<'info> {
    /// Receives the rent of the closed positions.
    #[account(mut)]
    pub cranker: Signer<'info>,

    #[account(mut)]
    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer.
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    pub pool: PoolSides<'info>,
    pub positions: OrcaPositions<'info>,

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

pub fn pull_legacy<'info>(ctx: Context<'info, PullLegacy<'info>>) -> Result<()> {
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
    let p = &ctx.accounts.pool;
    let (ours_a, ours_b) = (p.ours_a.to_account_info(), p.ours_b.to_account_info());
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

    let fee_a = ctx.accounts.fee_a.to_account_info();
    let fee_b = ctx.accounts.fee_b.to_account_info();
    pay_fee_share(
        config,
        [
            (fees.0, &ours_a, &fee_a, &p.mint_a.to_account_info(), &p.token_program_a.to_account_info()),
            (fees.1, &ours_b, &fee_b, &p.mint_b.to_account_info(), &p.token_program_b.to_account_info()),
        ],
        &ctx.accounts.authority,
        seeds,
    )?;

    let start = util::token_amount(if index_is_a { &ours_b } else { &ours_a })?;
    let config = &mut ctx.accounts.config;
    config.switch.old_index_sqrt = spot;
    config.switch.start_amount = start;
    config.switch.holding = quote;
    config.switch.phase = Phase::Swapping;
    Ok(())
}
