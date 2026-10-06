use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Mint, Token, TokenAccount};

use crate::{metaplex, state::*, validate::validate_metadata};

/// Burns `config.burn_amount` from the caller and rewrites name, symbol and uri in the same
/// instruction. There is no admin override: the program PDA is the only update authority.
#[derive(Accounts)]
pub struct Rename<'info> {
    pub user: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = mint)]
    pub config: Account<'info, Config>,

    #[account(mut)]
    pub mint: Account<'info, Mint>,

    #[account(mut, token::mint = mint, token::authority = user)]
    pub user_token: Account<'info, TokenAccount>,

    /// CHECK: PDA signer for the metadata update.
    #[account(seeds = [AUTHORITY_SEED], bump = config.authority_bump)]
    pub authority: UncheckedAccount<'info>,

    /// CHECK: address checked; the token metadata program checks ownership and authority.
    #[account(mut, address = metaplex::metadata_address(&mint.key()))]
    pub metadata: UncheckedAccount<'info>,

    /// CHECK: address checked.
    #[account(address = metaplex::TOKEN_METADATA_ID)]
    pub token_metadata_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
}

pub fn rename_handler(ctx: Context<Rename>, name: String, symbol: String, uri: String) -> Result<()> {
    validate_metadata(&name, &symbol, &uri)?;

    let config = &mut ctx.accounts.config;
    let burned = config.burn_amount;
    token::burn(
        CpiContext::new(
            ctx.accounts.token_program.key(),
            Burn {
                mint: ctx.accounts.mint.to_account_info(),
                from: ctx.accounts.user_token.to_account_info(),
                authority: ctx.accounts.user.to_account_info(),
            },
        ),
        burned,
    )?;

    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &[config.authority_bump]];
    metaplex::update_metadata(
        &ctx.accounts.metadata.to_account_info(),
        &ctx.accounts.authority.to_account_info(),
        &ctx.accounts.token_metadata_program.to_account_info(),
        &[seeds],
        name.clone(),
        symbol.clone(),
        uri.clone(),
    )?;

    config.renames += 1;
    config.total_burned += burned;
    emit!(Renamed { user: ctx.accounts.user.key(), name, symbol, uri, burned });
    Ok(())
}
