use anchor_lang::prelude::*;

pub mod damm;
pub mod error;
pub mod instructions;
pub mod math;
pub mod metaplex;
pub mod raydium;
pub mod state;
pub mod util;
pub mod validate;
pub mod whirlpool;

use instructions::*;

declare_id!("3ZYVePG4LhBWH9JvhGcExo1ysX6mWAwTGavBvyMgM3Ws");

#[program]
pub mod chamelequote {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>, params: InitializeParams) -> Result<()> {
        initialize::initialize_handler(ctx, params)
    }

    pub fn rename(ctx: Context<Rename>, name: String, symbol: String, uri: String) -> Result<()> {
        rename::rename_handler(ctx, name, symbol, uri)
    }

    // Quote registry and admin

    pub fn list_quote(ctx: Context<ListQuote>) -> Result<()> {
        quotes::list_quote(ctx)
    }

    pub fn set_quote_enabled(ctx: Context<AdminQuote>, enabled: bool) -> Result<()> {
        quotes::set_quote_enabled(ctx, enabled)
    }

    pub fn set_quote_route(ctx: Context<SetQuoteRoute>) -> Result<()> {
        quotes::set_quote_route(ctx)
    }

    pub fn set_admin(ctx: Context<AdminConfig>, new_admin: Pubkey) -> Result<()> {
        quotes::set_admin(ctx, new_admin)
    }

    pub fn set_risk_bounds(
        ctx: Context<AdminConfig>,
        max_price_move_bps: u16,
        max_route_deviation_bps: u16,
        max_slippage_bps: u16,
    ) -> Result<()> {
        quotes::set_risk_bounds(ctx, max_price_move_bps, max_route_deviation_bps, max_slippage_bps)
    }

    pub fn set_fee_share(ctx: Context<AdminConfig>, recipient: Pubkey, share_bps: u16) -> Result<()> {
        quotes::set_fee_share(ctx, recipient, share_bps)
    }

    // Price averages (keeper)

    pub fn poke_quotes<'info>(ctx: Context<'info, PokeQuotes>) -> Result<()> {
        oracle::poke_quotes(ctx)
    }

    pub fn poke_pool(ctx: Context<PokePool>) -> Result<()> {
        oracle::poke_pool(ctx)
    }

    // Quote switching

    pub fn launch(ctx: Context<Launch>, index_sqrt: u128) -> Result<()> {
        switch::launch(ctx, index_sqrt)
    }

    pub fn request_switch(ctx: Context<RequestSwitch>) -> Result<()> {
        switch::request_switch(ctx)
    }

    pub fn pull<'info>(ctx: Context<'info, Pull<'info>>) -> Result<()> {
        switch::pull(ctx)
    }

    /// `pull` from the Raydium pool the liquidity lived in before the move to Meteora DAMM v2.
    pub fn pull_legacy<'info>(ctx: Context<'info, PullLegacy<'info>>) -> Result<()> {
        legacy::pull_legacy(ctx)
    }

    pub fn hop<'info>(ctx: Context<'info, Hop<'info>>) -> Result<()> {
        switch::hop(ctx)
    }

    pub fn reprice<'info>(ctx: Context<'info, Reprice<'info>>) -> Result<()> {
        switch::reprice(ctx)
    }

    pub fn seed<'info>(ctx: Context<'info, Seed<'info>>) -> Result<()> {
        switch::seed(ctx)
    }

    pub fn add<'info>(ctx: Context<'info, Add<'info>>) -> Result<()> {
        switch::add(ctx)
    }

    pub fn claim_fees<'info>(ctx: Context<'info, ClaimFees<'info>>) -> Result<()> {
        switch::claim_fees(ctx)
    }

    pub fn abort(ctx: Context<Abort>) -> Result<()> {
        switch::abort(ctx)
    }

    #[cfg(feature = "dev")]
    pub fn dev_transfer(ctx: Context<DevTransfer>, amount: u64) -> Result<()> {
        dev::dev_transfer(ctx, amount)
    }
}
