# Totality

The coin and site at [totality.ws](https://totality.ws/) (the code base is still named chamelequote).

A Solana coin whose holders can change two things by burning 1,000,000 tokens:

- **The quote token.** All of the coin's liquidity sits in a Meteora DAMM v2 pool, in a position owned by the program (screeners like Axiom index DAMM v2 pools whatever the quote; route pools for swapping the backing are Orca Whirlpools). A burn re-pairs it with another approved token (tokenized stocks like TSLAX, USDC, SOL, …). The backing is swapped at the realised exchange rate and the price carries over, the way the Ethereum "LOOP" rotation manager does it.
- **The name, ticker and picture.** The program's PDA is the only Metaplex update authority, so a rename burns and rewrites the metadata in one instruction. No admin can override or revert it.

Status: tested locally with LiteSVM against the real Meteora DAMM v2, Orca, Raydium CLMM and Metaplex programs. **Not audited.** Mainnet runs the earlier Raydium build; `tests-svm/tests/migration.rs` upgrades that exact binary to this one and moves the liquidity to DAMM v2.

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
   - `reprice`: if the coin has used the target pool before, swaps it to the translated price through its sentinel.
   - `seed`: gives an old pool that has none a sentinel first.
   - `add`: lays the liquidity back as one position over the pool's range: unsold tokens from the price up, backing from the floor up to the price. A new pool is created here at the translated price, with the floor as its lower bound; its sentinel (a small position, 0.1% of the backing, left in every pool for good, since a pool with no liquidity can't be moved to a new price) is opened by `seed` right after. The escrowed burn is burned here.
   All of it happens in ONE transaction: `pull` refuses to run unless an `add` follows in the same transaction, so a switch lands whole or not at all and the coin never stops trading. The longest route (three hops into a new pool) fits the 64-account and 64-entry trace limits. The new pool's sentinel is seeded right after, outside that transaction.
3. If a switch can't land within 10 minutes, `abort` refunds the burn; nothing was pulled.

The price averages refuse trades during sharp moves: after a 2x pump, switching waits roughly 25 minutes.

## Build and test

Needs the Solana CLI (`cargo-build-sbf`) and Rust 1.97.1 for the host crates (pinned per crate).

```sh
tests-svm/fixtures/fetch.sh   # dumps Raydium CLMM, Orca, Metaplex and our deployed v1 from mainnet
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
- A DAMM v2 pool's range is fixed when it is created, and there is one pool per quote. When the coin comes back to a quote, part of the backing or of the unsold coins may not fit the old range and waits outside the pool until the next switch; a request that would land below the old floor is refused before anything burns. A pool for a quote created by someone else is refused too.
- A switch to a pool that has no sentinel (only the launch pool, if nothing was bought before leaving it) needs a `seed` inside the switch transaction; on a three-hop route that doesn't fit, and the request is refunded at its deadline.
- A switch into a pool that doesn't exist yet costs the requester 0.03 SOL (to the fee recipient, who funds the keeper): the rent of the pool and its sentinel.
