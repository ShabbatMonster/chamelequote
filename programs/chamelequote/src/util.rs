use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_spl::token_2022::spl_token_2022;

use crate::{error::ChameleonError, whirlpool};

/// Raw amount of an spl-token or token-2022 account (same offset in both).
pub fn token_amount(info: &AccountInfo) -> Result<u64> {
    let d = info.try_borrow_data()?;
    require!(d.len() >= 72, ChameleonError::WrongTokenAccount);
    Ok(u64::from_le_bytes(d[64..72].try_into().unwrap()))
}

/// Requires `info` to be `owner`'s associated token account for `mint`, whose token program is
/// whichever program owns the mint account.
pub fn require_ata(info: &AccountInfo, owner: &Pubkey, mint: &AccountInfo) -> Result<()> {
    require_keys_eq!(
        *info.key,
        whirlpool::ata(owner, mint.key, mint.owner),
        ChameleonError::WrongTokenAccount
    );
    Ok(())
}

pub fn mint_decimals(mint: &AccountInfo) -> Result<u8> {
    let d = mint.try_borrow_data()?;
    require!(d.len() >= 45, ChameleonError::WrongTokenAccount);
    Ok(d[44])
}

/// transfer_checked through whichever token program owns the mint.
#[allow(clippy::too_many_arguments)]
pub fn transfer<'info>(
    token_program: &AccountInfo<'info>,
    from: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    amount: u64,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    require_keys_eq!(*token_program.key, *mint.owner, ChameleonError::WrongTokenAccount);
    let ix = spl_token_2022::instruction::transfer_checked(
        token_program.key,
        from.key,
        mint.key,
        to.key,
        authority.key,
        &[],
        amount,
        mint_decimals(mint)?,
    )?;
    invoke_signed(
        &ix,
        &[from.clone(), mint.clone(), to.clone(), authority.clone(), token_program.clone()],
        signer_seeds,
    )?;
    Ok(())
}
