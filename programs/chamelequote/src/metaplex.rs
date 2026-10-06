//! Minimal hand-rolled CPI into Metaplex Token Metadata. The `mpl-token-metadata`
//! crate pins its own solana-program version, which fights anchor-lang's, and we
//! only need two instructions.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
};

pub const TOKEN_METADATA_ID: Pubkey = pubkey!("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s");

pub const MAX_NAME_LEN: usize = 32;
pub const MAX_SYMBOL_LEN: usize = 10;
pub const MAX_URI_LEN: usize = 200;

const IX_CREATE_METADATA_ACCOUNT_V3: u8 = 33;
const IX_UPDATE_METADATA_ACCOUNT_V2: u8 = 15;

/// Fungible tokens never use creators, collections or uses, so they are always None.
#[derive(AnchorSerialize)]
struct DataV2 {
    name: String,
    symbol: String,
    uri: String,
    seller_fee_basis_points: u16,
    creators: Option<Vec<[u8; 0]>>,
    collection: Option<[u8; 0]>,
    uses: Option<[u8; 0]>,
}

impl DataV2 {
    fn new(name: String, symbol: String, uri: String) -> Self {
        Self { name, symbol, uri, seller_fee_basis_points: 0, creators: None, collection: None, uses: None }
    }
}

#[derive(AnchorSerialize)]
struct CreateMetadataAccountArgsV3 {
    data: DataV2,
    is_mutable: bool,
    collection_details: Option<[u8; 0]>,
}

#[derive(AnchorSerialize)]
struct UpdateMetadataAccountArgsV2 {
    data: Option<DataV2>,
    new_update_authority: Option<Pubkey>,
    primary_sale_happened: Option<bool>,
    is_mutable: Option<bool>,
}

pub fn metadata_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"metadata", TOKEN_METADATA_ID.as_ref(), mint.as_ref()], &TOKEN_METADATA_ID).0
}

/// Creates mutable metadata whose mint authority and update authority are both `authority`.
#[allow(clippy::too_many_arguments)]
pub fn create_metadata<'info>(
    metadata: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    payer: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    token_metadata_program: &AccountInfo<'info>,
    signer_seeds: &[&[&[u8]]],
    name: String,
    symbol: String,
    uri: String,
) -> Result<()> {
    let mut data = vec![IX_CREATE_METADATA_ACCOUNT_V3];
    CreateMetadataAccountArgsV3 { data: DataV2::new(name, symbol, uri), is_mutable: true, collection_details: None }
        .serialize(&mut data)?;
    let ix = Instruction {
        program_id: TOKEN_METADATA_ID,
        accounts: vec![
            AccountMeta::new(metadata.key(), false),
            AccountMeta::new_readonly(mint.key(), false),
            AccountMeta::new_readonly(authority.key(), true),
            AccountMeta::new(payer.key(), true),
            AccountMeta::new_readonly(authority.key(), true),
            AccountMeta::new_readonly(system_program.key(), false),
        ],
        data,
    };
    invoke_signed(
        &ix,
        &[
            metadata.clone(),
            mint.clone(),
            authority.clone(),
            payer.clone(),
            system_program.clone(),
            token_metadata_program.clone(),
        ],
        signer_seeds,
    )?;
    Ok(())
}

/// Replaces name, symbol and uri; leaves authority and mutability untouched.
pub fn update_metadata<'info>(
    metadata: &AccountInfo<'info>,
    update_authority: &AccountInfo<'info>,
    token_metadata_program: &AccountInfo<'info>,
    signer_seeds: &[&[&[u8]]],
    name: String,
    symbol: String,
    uri: String,
) -> Result<()> {
    let mut data = vec![IX_UPDATE_METADATA_ACCOUNT_V2];
    UpdateMetadataAccountArgsV2 {
        data: Some(DataV2::new(name, symbol, uri)),
        new_update_authority: None,
        primary_sale_happened: None,
        is_mutable: None,
    }
    .serialize(&mut data)?;
    let ix = Instruction {
        program_id: TOKEN_METADATA_ID,
        accounts: vec![
            AccountMeta::new(metadata.key(), false),
            AccountMeta::new_readonly(update_authority.key(), true),
        ],
        data,
    };
    invoke_signed(
        &ix,
        &[metadata.clone(), update_authority.clone(), token_metadata_program.clone()],
        signer_seeds,
    )?;
    Ok(())
}
