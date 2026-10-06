use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::AssociatedToken,
    token::{self, spl_token::instruction::AuthorityType, Mint, MintTo, SetAuthority, Token, TokenAccount},
};

use crate::{error::ChameleonError, metaplex, state::*, validate::validate_metadata};

#[derive(AnchorSerialize, AnchorDeserialize)]
pub struct InitializeParams {
    pub name: String,
    pub symbol: String,
    pub uri: String,
    /// Raw units, minted once to the authority's vault. Mint authority is revoked right after.
    pub supply: u64,
    /// Raw units burned per rename or quote change.
    pub burn_amount: u64,
    /// Raydium CLMM AmmConfig (fee tier) our pools are created under, and its tick spacing.
    pub clmm_config: Pubkey,
    pub tick_spacing: u16,
    pub usdc: Pubkey,
    pub wsol: Pubkey,
    /// Receives `fee_share_bps` of the trading fees our positions earn; the rest stays as backing.
    pub fee_recipient: Pubkey,
    pub fee_share_bps: u16,
}

#[derive(Accounts)]
#[instruction(params: InitializeParams)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(init, payer = admin, space = 8 + Config::INIT_SPACE, seeds = [CONFIG_SEED], bump)]
    pub config: Account<'info, Config>,

    /// CHECK: PDA that owns the supply, the liquidity and the metadata update authority.
    #[account(seeds = [AUTHORITY_SEED], bump)]
    pub authority: UncheckedAccount<'info>,

    /// A fresh keypair, so the deployer can grind a vanity address for it.
    #[account(init, payer = admin, mint::decimals = 6, mint::authority = authority)]
    pub mint: Account<'info, Mint>,

    #[account(
        init,
        payer = admin,
        associated_token::mint = mint,
        associated_token::authority = authority,
    )]
    pub supply_vault: Account<'info, TokenAccount>,

    /// Holds burns for quote switches until they complete (burned) or abort (refunded).
    #[account(
        init,
        payer = admin,
        seeds = [ESCROW_SEED],
        bump,
        token::mint = mint,
        token::authority = authority,
    )]
    pub escrow: Account<'info, TokenAccount>,

    /// CHECK: created by the token metadata program; address checked.
    #[account(mut, address = metaplex::metadata_address(&mint.key()))]
    pub metadata: UncheckedAccount<'info>,

    /// CHECK: address checked.
    #[account(address = metaplex::TOKEN_METADATA_ID)]
    pub token_metadata_program: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn initialize_handler(ctx: Context<Initialize>, params: InitializeParams) -> Result<()> {
    validate_metadata(&params.name, &params.symbol, &params.uri)?;
    require!(
        params.burn_amount > 0 && params.burn_amount <= params.supply,
        ChameleonError::BadBurnAmount
    );
    require!(
        params.fee_share_bps <= MAX_FEE_SHARE_BPS && params.tick_spacing > 0 && params.tick_spacing < 32768,
        ChameleonError::InvalidParam
    );

    let authority_bump = ctx.bumps.authority;
    let seeds: &[&[u8]] = &[AUTHORITY_SEED, &[authority_bump]];

    token::mint_to(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            MintTo {
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.supply_vault.to_account_info(),
                authority: ctx.accounts.authority.to_account_info(),
            },
            &[seeds],
        ),
        params.supply,
    )?;

    // Metadata needs the mint authority's signature, so create it before revoking.
    metaplex::create_metadata(
        &ctx.accounts.metadata.to_account_info(),
        &ctx.accounts.mint.to_account_info(),
        &ctx.accounts.authority.to_account_info(),
        &ctx.accounts.admin.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        &ctx.accounts.token_metadata_program.to_account_info(),
        &[seeds],
        params.name,
        params.symbol,
        params.uri,
    )?;

    token::set_authority(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            SetAuthority {
                current_authority: ctx.accounts.authority.to_account_info(),
                account_or_mint: ctx.accounts.mint.to_account_info(),
            },
            &[seeds],
        ),
        AuthorityType::MintTokens,
        None,
    )?;

    ctx.accounts.config.set_inner(Config {
        admin: ctx.accounts.admin.key(),
        mint: ctx.accounts.mint.key(),
        burn_amount: params.burn_amount,
        renames: 0,
        quote_changes: 0,
        total_burned: 0,
        bump: ctx.bumps.config,
        authority_bump,
        escrow_bump: ctx.bumps.escrow,
        clmm_config: params.clmm_config,
        tick_spacing: params.tick_spacing,
        usdc: params.usdc,
        wsol: params.wsol,
        fee_recipient: params.fee_recipient,
        fee_share_bps: params.fee_share_bps,
        max_price_move_bps: 300,
        max_route_deviation_bps: 200,
        max_slippage_bps: 200,
        active_quote: Pubkey::default(),
        active_pool: Pubkey::default(),
        floor_sqrt: 0,
        pool_ema: Ema::default(),
        switch: Switch::default(),
    });
    Ok(())
}
