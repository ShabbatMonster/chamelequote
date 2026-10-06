use anchor_lang::prelude::*;

#[error_code]
pub enum ChameleonError {
    #[msg("Only the admin can do this")]
    NotAdmin,
    #[msg("Name must be 1-32 bytes")]
    BadName,
    #[msg("Symbol must be 1-10 bytes")]
    BadSymbol,
    #[msg("URI must be at most 200 bytes and start with https://, ipfs:// or ar://")]
    BadUri,
    #[msg("Text contains control characters")]
    ControlChars,
    #[msg("Burn amount must be non-zero and no more than the supply")]
    BadBurnAmount,
    #[msg("Quote token is not enabled")]
    QuoteDisabled,
    #[msg("Quote cannot be the token itself")]
    QuoteIsSelf,
    #[msg("Invalid parameter")]
    InvalidParam,
    #[msg("Account is not a Whirlpool")]
    NotAWhirlpool,
    #[msg("Account is not a Whirlpool position of ours")]
    BadPosition,
    #[msg("A required account was not passed")]
    MissingAccount,
    #[msg("Route pool must pair the quote with its hub")]
    BadRoute,
    #[msg("Hub must be USDC or WSOL and already listed")]
    BadHub,
    #[msg("Price average is stale or still warming up; poke it")]
    StalePrice,
    #[msg("Already launched")]
    AlreadyLaunched,
    #[msg("Not launched yet")]
    NotLaunched,
    #[msg("Wrong phase for this step")]
    WrongPhase,
    #[msg("That is already the active quote")]
    SameQuote,
    #[msg("Pool is not the expected one")]
    WrongPool,
    #[msg("Token account is not the program's")]
    WrongTokenAccount,
    #[msg("Pool price moved too far from its average")]
    PoolManipulated,
    #[msg("Route pool price is too far from its average")]
    RouteOffAverage,
    #[msg("Swap output below the slippage bound")]
    SlippageExceeded,
    #[msg("Wrong route step for the current holding")]
    WrongHop,
    #[msg("Pool is not at the target price yet")]
    NotAtTarget,
    #[msg("Need some of the input token to move the pool price")]
    NothingToReprice,
    #[msg("Switch deadline has not passed")]
    NotExpired,
    #[msg("Quote entry is not the one expected")]
    WrongQuote,
    #[msg("Math overflow")]
    MathOverflow,
}
