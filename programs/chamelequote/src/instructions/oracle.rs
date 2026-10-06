//! Permissionless price averages. A keeper pokes every enabled quote and our own pool about once
//! a minute; switches refuse to trade on an average that is stale or still warming up.

use anchor_lang::prelude::*;

use crate::{error::ChameleonError, math, state::*, whirlpool};

/// sqrt(hub per quote) on the quote's route pool.
pub fn route_spot(entry: &QuoteEntry, pool: &AccountInfo) -> Result<(u128, u16)> {
    require_keys_eq!(*pool.key, entry.route_pool, ChameleonError::WrongPool);
    let p = whirlpool::read_pool(pool)?;
    // Whirlpool price is b per a: hub per quote when the quote is token A.
    Ok((math::flip(p.sqrt_price, p.mint_a != entry.mint), p.fee_rate))
}

/// sqrt(quote per index) on one of our pools.
pub fn index_spot(config: &Config, pool: &AccountInfo, quote: &Pubkey) -> Result<u128> {
    let p = whirlpool::read_pool(pool)?;
    Ok(math::flip(p.sqrt_price, !config.index_is_a(quote)))
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
        require_keys_eq!(
            entry_info.key(),
            Pubkey::create_program_address(&[QUOTE_SEED, entry.mint.as_ref(), &[entry.bump]], &crate::ID)
                .map_err(|_| ChameleonError::MissingAccount)?,
            ChameleonError::MissingAccount
        );
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
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,

    /// CHECK: must be the active pool.
    #[account(address = config.active_pool @ ChameleonError::WrongPool)]
    pub pool: UncheckedAccount<'info>,
}

/// No-op while a switch is in flight (the pool is empty then).
pub fn poke_pool(ctx: Context<PokePool>) -> Result<()> {
    let config = &mut ctx.accounts.config;
    if config.switch.phase != Phase::Idle || config.active_quote == Pubkey::default() {
        return Ok(());
    }
    let spot = index_spot(config, &ctx.accounts.pool, &config.active_quote.clone())?;
    config.pool_ema.update(spot, Clock::get()?.unix_timestamp);
    Ok(())
}
