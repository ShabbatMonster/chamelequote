//! Test-only: compiled in with `--features dev`, never in a deployable build.
//! Until a pool exists the whole supply sits in the authority vault, so tests need a way out.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use crate::{error::ChameleonError, state::*};

#[derive(Accounts)]
pub struct DevTransfer<'info> {
    pub admin: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ ChameleonError::NotAdmin, has_one = mint)]
    pub config: Account<'info, Config>,

    pub mint: Account<'info, Mint>,

    /// CHECK: PDA signer.
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    #[account(mut, associated_token::mint = mint, associated_token::authority = authority)]
    pub supply_vault: Account<'info, TokenAccount>,

    #[account(mut, token::mint = mint)]
    pub to: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn dev_transfer(ctx: Context<DevTransfer>, amount: u64) -> Result<()> {
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &[ctx.accounts.config.authority_bump]];
    token::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            Transfer {
                from: ctx.accounts.supply_vault.to_account_info(),
                to: ctx.accounts.to.to_account_info(),
                authority: ctx.accounts.authority.to_account_info(),
            },
            &[seeds],
        ),
        amount,
    )
}
