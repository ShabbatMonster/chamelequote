use anchor_lang::prelude::*;

use crate::{error::ChameleonError, metaplex};

const URI_SCHEMES: [&str; 3] = ["https://", "ipfs://", "ar://"];

/// Lengths are Metaplex's limits, in bytes. Control characters are refused because wallets and
/// explorers render them unpredictably (a newline in a ticker can spoof a second line of UI).
pub fn validate_metadata(name: &str, symbol: &str, uri: &str) -> Result<()> {
    require!(!name.is_empty() && name.len() <= metaplex::MAX_NAME_LEN, ChameleonError::BadName);
    require!(!symbol.is_empty() && symbol.len() <= metaplex::MAX_SYMBOL_LEN, ChameleonError::BadSymbol);
    require!(
        uri.len() <= metaplex::MAX_URI_LEN && URI_SCHEMES.iter().any(|s| uri.starts_with(s)),
        ChameleonError::BadUri
    );
    require!(
        ![name, symbol, uri].iter().any(|s| s.chars().any(char::is_control)),
        ChameleonError::ControlChars
    );
    Ok(())
}
