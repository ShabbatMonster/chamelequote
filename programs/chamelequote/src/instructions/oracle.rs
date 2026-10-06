//! Permissionless price averages. A keeper pokes every enabled quote and our own pool about once
//! a minute; switches refuse to trade on an average that is stale or still warming up.

use anchor_lang::prelude::*;

use crate::{damm, error::ChameleonError, math, state::*, whirlpool};

/// sqrt(hub per quote) on the quote's route pool.
pub fn route_spot(entry: &QuoteEntry, pool: &AccountInfo) -> Result<(u128, u16)> {
    require_keys_eq!(*pool.key, entry.route_pool, ChameleonError::WrongPool);
    let p = whirlpool::read_pool(pool)?;
    // Whirlpool price is b per a: hub per quote when the quote is token A.
    Ok((math::flip(p.sqrt_price, p.mint_a != entry.mint), p.fee_rate))
}

/// sqrt(quote per index) on our pool (token A is ours, so the pool price is already that).
pub fn index_spot(pool: &AccountInfo) -> Result<u128> {
    Ok(damm::read_pool(pool)?.sqrt_price)
}

#[derive(Accounts)]
pub struct PokeQuotes {}

/// remaining_accounts: (quote entry [writable], route pool) pairs.
pub fn poke_quotes<'info>(ctx: Context<'info, PokeQuotes>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let rem = ctx.remaining_accounts;
    require!(rem.len() % 2 == 0, ChameleonError::MissingAccount);
    for pair in rem.chunks(2) {
        let (entry_info, pool) = (&pair[0], &pair[1]);
        let mut entry: Account<QuoteEntry> = Account::try_from(entry_info)?;
        // Account::try_from checks owner and type; entries only ever exist at their mint's PDA.
        if entry.is_root() {
            continue;
        }
        let (spot, _) = route_spot(&entry, pool)?;
        entry.ema.update(spot, now);
        entry.exit(&crate::ID)?;
    }
    Ok(())
}

#[derive(Accounts)]
pub struct PokePool<'info> {
    #[account(mut)]
    pub config: Box<Account<'info, Config>>,

    /// CHECK: must be the active pool.
    #[account(address = config.active_pool @ ChameleonError::WrongPool)]
    pub pool: UncheckedAccount<'info>,
}

/// Keeps going after a switch is requested (the pull checks the price against this average), and
/// is a no-op once the liquidity is out.
pub fn poke_pool(ctx: Context<PokePool>) -> Result<()> {
    let config = &mut ctx.accounts.config;
    if !matches!(config.switch.phase, Phase::Idle | Phase::Requested) || config.active_quote == Pubkey::default() {
        return Ok(());
    }
    let spot = index_spot(&ctx.accounts.pool)?;
    config.pool_ema.update(spot, Clock::get()?.unix_timestamp);
    Ok(())
}
