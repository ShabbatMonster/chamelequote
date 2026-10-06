use anchor_lang::prelude::*;
use anchor_spl::token_interface::Mint;

use crate::{error::ChameleonError, state::*, whirlpool};

/// Checks that `pool` is a Whirlpool between `mint` and `hub`.
fn check_route(pool: &AccountInfo, mint: &Pubkey, hub: &Pubkey) -> Result<()> {
    let p = whirlpool::read_pool(pool)?;
    require!(
        (p.mint_a == *mint && p.mint_b == *hub) || (p.mint_a == *hub && p.mint_b == *mint),
        ChameleonError::BadRoute
    );
    Ok(())
}

#[derive(Accounts)]
pub struct ListQuote<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(has_one = admin @ ChameleonError::NotAdmin)]
    pub config: Box<Account<'info, Config>>,

    /// spl-token or token-2022 mint; the owning program is recorded.
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        init,
        payer = admin,
        space = 8 + QuoteEntry::INIT_SPACE,
        seeds = [QUOTE_SEED, quote_mint.key().as_ref()],
        bump,
    )]
    pub quote: Box<Account<'info, QuoteEntry>>,

    /// The hub's entry (USDC or WSOL). None only when listing USDC itself.
    pub hub_entry: Option<Box<Account<'info, QuoteEntry>>>,

    /// CHECK: validated as a Whirlpool between the quote and the hub. None only for USDC.
    pub route_pool: Option<UncheckedAccount<'info>>,

    pub system_program: Program<'info, System>,
}

/// USDC must be listed first, then WSOL (routed via SOL/USDC), then everything else.
pub fn list_quote(ctx: Context<ListQuote>) -> Result<()> {
    let config = &ctx.accounts.config;
    let mint = ctx.accounts.quote_mint.key();
    require_keys_neq!(mint, config.mint, ChameleonError::QuoteIsSelf);

    let (hub, route_pool) = if mint == config.usdc {
        (mint, Pubkey::default())
    } else {
        let hub = ctx.accounts.hub_entry.as_ref().ok_or(ChameleonError::BadHub)?.mint;
        let allowed = if mint == config.wsol { hub == config.usdc } else { hub == config.usdc || hub == config.wsol };
        require!(allowed, ChameleonError::BadHub);
        let pool = ctx.accounts.route_pool.as_ref().ok_or(ChameleonError::BadRoute)?;
        check_route(pool, &mint, &hub)?;
        (hub, pool.key())
    };

    ctx.accounts.quote.set_inner(QuoteEntry {
        mint,
        token_program: *ctx.accounts.quote_mint.to_account_info().owner,
        decimals: ctx.accounts.quote_mint.decimals,
        enabled: true,
        bump: ctx.bumps.quote,
        hub,
        route_pool,
        ema: Ema::default(),
    });
    emit!(QuoteListed { mint, enabled: true });
    Ok(())
}

#[derive(Accounts)]
pub struct AdminQuote<'info> {
    pub admin: Signer<'info>,

    #[account(has_one = admin @ ChameleonError::NotAdmin)]
    pub config: Box<Account<'info, Config>>,

    #[account(mut)]
    pub quote: Box<Account<'info, QuoteEntry>>,
}

/// Disabling stops future switches into this quote. It never touches liquidity already there.
pub fn set_quote_enabled(ctx: Context<AdminQuote>, enabled: bool) -> Result<()> {
    ctx.accounts.quote.enabled = enabled;
    emit!(QuoteListed { mint: ctx.accounts.quote.mint, enabled });
    Ok(())
}

#[derive(Accounts)]
pub struct SetQuoteRoute<'info> {
    pub admin: Signer<'info>,

    #[account(has_one = admin @ ChameleonError::NotAdmin)]
    pub config: Box<Account<'info, Config>>,

    #[account(mut)]
    pub quote: Box<Account<'info, QuoteEntry>>,

    /// CHECK: validated as a Whirlpool between the quote and its hub.
    pub route_pool: UncheckedAccount<'info>,
}

/// Moves a quote to a deeper route pool. Its price average restarts (and must warm up again).
pub fn set_quote_route(ctx: Context<SetQuoteRoute>) -> Result<()> {
    let q = &mut ctx.accounts.quote;
    require!(!q.is_root(), ChameleonError::BadRoute);
    let s = &ctx.accounts.config.switch;
    require!(
        s.phase == Phase::Idle || (s.holding != q.mint && s.target != q.mint),
        ChameleonError::WrongPhase
    );
    check_route(&ctx.accounts.route_pool, &q.mint, &q.hub)?;
    q.route_pool = ctx.accounts.route_pool.key();
    q.ema = Ema::default();
    Ok(())
}

#[derive(Accounts)]
pub struct AdminConfig<'info> {
    pub admin: Signer<'info>,

    #[account(mut, has_one = admin @ ChameleonError::NotAdmin)]
    pub config: Box<Account<'info, Config>>,
}

/// Pass Pubkey::default() to renounce: the quote list and risk bounds are then frozen.
pub fn set_admin(ctx: Context<AdminConfig>, new_admin: Pubkey) -> Result<()> {
    ctx.accounts.config.admin = new_admin;
    Ok(())
}

/// Bounds are capped so a compromised admin cannot open the backing to manipulation.
pub fn set_risk_bounds(
    ctx: Context<AdminConfig>,
    max_price_move_bps: u16,
    max_route_deviation_bps: u16,
    max_slippage_bps: u16,
) -> Result<()> {
    require!(
        (50..=500).contains(&max_price_move_bps)
            && (10..=500).contains(&max_route_deviation_bps)
            && (10..=500).contains(&max_slippage_bps),
        ChameleonError::InvalidParam
    );
    let c = &mut ctx.accounts.config;
    c.max_price_move_bps = max_price_move_bps;
    c.max_route_deviation_bps = max_route_deviation_bps;
    c.max_slippage_bps = max_slippage_bps;
    Ok(())
}

pub fn set_fee_share(ctx: Context<AdminConfig>, recipient: Pubkey, share_bps: u16) -> Result<()> {
    require!(share_bps <= MAX_FEE_SHARE_BPS, ChameleonError::InvalidParam);
    let c = &mut ctx.accounts.config;
    c.fee_recipient = recipient;
    c.fee_share_bps = share_bps;
    Ok(())
}

