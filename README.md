# Chamelequote

A Solana coin whose holders can change two things by burning 1,000,000 tokens:

- **The quote token.** All of the coin's liquidity sits in an Orca Whirlpool owned by the program. A burn re-pairs it with another approved token (tokenized stocks like TSLAX, USDC, SOL, …). The backing is swapped at the realised exchange rate and the price carries over, the way the Ethereum "LOOP" rotation manager does it.
- **The name, ticker and picture.** The program's PDA is the only Metaplex update authority, so a rename burns and rewrites the metadata in one instruction. No admin can override or revert it.

Status: built and tested locally (LiteSVM against the real Orca and Metaplex programs). **Not audited, not deployed.**

## Layout

| Path | What |
|---|---|
| `programs/chamelequote` | The Anchor program: launch, rename, quote registry, price averages, quote switching. |
| `crank` | Keeper logic: works out the next transaction from an account snapshot. Shared by the keeper and the tests. |
| `keeper` | The keeper binary: pokes price averages every ~50 s and pushes switches through. |
| `tests-svm` | LiteSVM tests (Rust). They drive switches through `crank`, so they test what the keeper sends. |
| `docs` | The website (static, no build step). Served by GitHub Pages. Demo mode until `MINT` is set in `docs/js/config.js`. |
| `data` | The approved-quote source list (stonkfun) and the best Orca route pool for each quote. |

## How a switch works

1. A user calls `request_switch`; the burn goes into escrow.
2. The keeper (anyone, really) cranks it:
   - `pull`: closes the program's positions in the current pool, collecting fees.
   - `hop`: swaps the backing one Orca route pool at a time, via USDC or SOL, checked against on-chain price averages.
   - `reprice`: moves the target pool to the translated price.
   - `add`: lays the liquidity back as two single-sided positions: tokens above the price, backing from the floor up to the price. The escrowed burn is burned here.
3. If a switch is not done within 10 minutes, `abort` refunds the burn and lays the backing into whatever token it is held in.

The price averages refuse trades during sharp moves: after a 2x pump, switching waits roughly 25 minutes.

## Build and test

Needs the Solana CLI (`cargo-build-sbf`) and Rust 1.97.1 for the host crates (pinned per crate).

```sh
tests-svm/fixtures/fetch.sh   # dumps the Orca and Metaplex programs from mainnet
npm test                      # builds the dev program and runs the LiteSVM tests
npm run build                 # the deployable program
npm run keeper:build
npm run site                  # serves docs/ on http://localhost:8765
```

`.cargo/config.toml` holds Windows-specific linker settings (no Visual Studio needed); they are ignored on other platforms.

## Keeper

```sh
keeper status --rpc <url>
keeper run    --rpc <url> --keypair <path> [--priority-fee <micro-lamports per CU>]
keeper once   --rpc <url> --keypair <path>
```

It needs a funded wallet. Each poke round costs a few transactions a minute, and switch steps pay rent for new positions and tick arrays, refunded when the positions are closed.

## Known risks

- Unaudited. The price-average manipulation defence is the part to audit first.
- Tokenized stocks (xStocks) are issued by a company that can freeze or move them, including the coin's backing while it sits in one.
- The admin key can list quotes (it can't touch funds or metadata). Renounce it with `set_admin(default)` once the list is final.
