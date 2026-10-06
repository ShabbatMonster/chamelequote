//! The one step that still touches a Raydium pool of ours: pulling the liquidity out of the
//! Raydium CLMM pool it lived in before the move to Meteora DAMM v2. The next switch after the
//! upgrade runs `pull_legacy` instead of `pull`; everything after that is DAMM v2. It also takes
//! back the sentinel position Raydium pools needed. Can go in a later upgrade.

use anchor_lang::prelude::*;

use crate::{error::ChameleonError as E, math, raydium as ray, state::*, util, whirlpool as wp};

use super::switch::{pay_fee_share, position_mint_address};

/// The Raydium pool (token 0/1 in sorted order) and the program's token account for each side.
#[derive(Accounts)]
pub struct RayPool<'info> {
    /// CHECK: must be the active pool; `load` checks owner and contents.
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

/// One Raydium position slot: NFT mint (our PDA), the authority's NFT account, Raydium's
/// position account, and the tick arrays holding both ends.
#[derive(Accounts)]
pub struct RaySlot<'info> {
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

impl<'info> RaySlot<'info> {
    fn check(&self, pool: &Pubkey, i: u8) -> Result<ray::Slot> {
        require_keys_eq!(self.nft_mint.key(), position_mint_address(pool, i).0, E::BadPosition);
        require_keys_eq!(self.personal.key(), ray::personal_position_address(&self.nft_mint.key()), E::BadPosition);
        Ok(ray::Slot { nft_mint: self.nft_mint.key(), lower_array: self.lower.key(), upper_array: self.upper.key() })
    }
}

#[derive(Accounts)]
pub struct PullLegacy<'info> {
    pub cranker: Signer<'info>,

    #[account(mut)]
    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA signer; receives the closed positions' rent.
    #[account(mut, seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    pub pool: RayPool<'info>,
    /// Index position, backing position, sentinel.
    pub slot_0: RaySlot<'info>,
    pub slot_1: RaySlot<'info>,
    pub slot_2: RaySlot<'info>,

    /// CHECK: fee recipient's ATA for mint 0 (checked when a share is paid).
    #[account(mut)]
    pub fee_0: UncheckedAccount<'info>,
    /// CHECK: fee recipient's ATA for mint 1 (checked when a share is paid).
    #[account(mut)]
    pub fee_1: UncheckedAccount<'info>,

    pub token_program: Program<'info, anchor_spl::token::Token>,
    /// CHECK: address checked.
    #[account(address = wp::TOKEN_2022_ID)]
    pub token_2022_program: UncheckedAccount<'info>,
    /// CHECK: address checked.
    #[account(address = wp::MEMO_ID)]
    pub memo_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: address checked.
    #[account(address = ray::CLMM_ID)]
    pub clmm_program: UncheckedAccount<'info>,
}

pub fn pull_legacy<'info>(ctx: Context<'info, PullLegacy<'info>>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let config = &ctx.accounts.config;
    require!(config.switch.phase == Phase::Requested, E::WrongPhase);
    let p = &ctx.accounts.pool;
    let pool_key = p.pool.key();
    require_keys_eq!(pool_key, config.active_pool, E::WrongPool);
    let ps = ray::read_pool(&p.pool)?;
    let quote = config.active_quote;
    let index_is_0 = config.index_is_a(&quote);
    let mints = if index_is_0 { [config.mint, quote] } else { [quote, config.mint] };
    require!(
        p.mint_0.key() == mints[0]
            && p.mint_1.key() == mints[1]
            && ps.mint_0 == mints[0]
            && ps.mint_1 == mints[1]
            && p.vault_0.key() == ps.vault_0
            && p.vault_1.key() == ps.vault_1,
        E::WrongPool
    );
    let authority = ctx.accounts.authority.key();
    util::require_ata(&p.ours_0, &authority, &p.mint_0)?;
    util::require_ata(&p.ours_1, &authority, &p.mint_1)?;
    let sides = ray::Sides {
        mint: mints,
        program: [*p.mint_0.owner, *p.mint_1.owner],
        ours: [p.ours_0.key(), p.ours_1.key()],
        vault: [ps.vault_0, ps.vault_1],
    };

    // Refuse to pull on a price pushed away from its average (the LOOP "block start" check).
    let spot = math::flip(ps.sqrt_price, !index_is_0);
    let ema = config.pool_ema.checked(now)?;
    require!(within_bps(spot, ema, config.max_price_move_bps), E::PoolManipulated);

    let infos = ctx.accounts.to_account_infos();
    let bump = [config.authority_bump];
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &bump];
    let (ours_0, ours_1) = (p.ours_0.to_account_info(), p.ours_1.to_account_info());
    let mut fees = [0u64; 2];
    for (i, accounts) in [&ctx.accounts.slot_0, &ctx.accounts.slot_1, &ctx.accounts.slot_2].into_iter().enumerate() {
        let slot = accounts.check(&pool_key, i as u8)?;
        let Some(st) = ray::read_position(&accounts.personal)? else { continue };
        let before = [util::token_amount(&ours_0)?, util::token_amount(&ours_1)?];
        wp::invoke(&ray::decrease_liquidity_ix(authority, pool_key, &sides, &slot, 0), &infos, &[seeds])?;
        fees[0] += util::token_amount(&ours_0)? - before[0];
        fees[1] += util::token_amount(&ours_1)? - before[1];
        if st.liquidity > 0 {
            wp::invoke(&ray::decrease_liquidity_ix(authority, pool_key, &sides, &slot, st.liquidity), &infos, &[seeds])?;
        }
        wp::invoke(&ray::close_position_ix(authority, pool_key, &slot), &infos, &[seeds])?;
    }

    let program_of = |mint: &AccountInfo<'info>| -> AccountInfo<'info> {
        if *mint.owner == wp::TOKEN_2022_ID {
            ctx.accounts.token_2022_program.to_account_info()
        } else {
            ctx.accounts.token_program.to_account_info()
        }
    };
    let (m0, m1) = (p.mint_0.to_account_info(), p.mint_1.to_account_info());
    pay_fee_share(
        config,
        [
            (fees[0], &ours_0, &ctx.accounts.fee_0.to_account_info(), &m0, &program_of(&m0)),
            (fees[1], &ours_1, &ctx.accounts.fee_1.to_account_info(), &m1, &program_of(&m1)),
        ],
        &ctx.accounts.authority,
        seeds,
    )?;

    let start = util::token_amount(if index_is_0 { &ours_1 } else { &ours_0 })?;
    let config = &mut ctx.accounts.config;
    config.switch.old_index_sqrt = spot;
    config.switch.start_amount = start;
    config.switch.holding = quote;
    config.switch.phase = Phase::Swapping;
    Ok(())
}
