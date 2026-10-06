//! Keeper logic, independent of how accounts are fetched or transactions sent. Given a
//! `Ledger` (an account snapshot source), `Crank` works out the next transaction that moves an
//! in-flight quote switch forward, and the pokes that keep the price averages fresh.
//!
//! The LiteSVM tests drive switches through this same code, so what the keeper runs is what the
//! tests check.

use anchor_lang::{
    prelude::Pubkey,
    solana_program::{
        instruction::{AccountMeta, Instruction},
        system_program,
    },
    AccountDeserialize, InstructionData, ToAccountMetas,
};
use chamelequote::{
    accounts, instruction,
    instructions::{expected_pool, liquidity_for, plan_positions, pool_mints, position_mint_address, InitializeParams},
    math, metaplex,
    raydium as ray,
    state::{
        within_bps, Config, Phase, QuoteEntry, AUTHORITY_SEED, CONFIG_SEED, ESCROW_SEED, PRICE_TOLERANCE_BPS, QUOTE_SEED,
        sentinel_share, SENTINEL_SLOT,
    },
    whirlpool::{self as wp, PoolState, Sides},
    ID as PROGRAM_ID,
};
use solana_keypair::Keypair;

pub const TOKEN_PROGRAM: Pubkey = anchor_lang::prelude::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Where accounts come from: an RPC node, or LiteSVM in tests.
pub trait Ledger {
    /// (owner, data) of an account, or None if it does not exist.
    fn account(&self, key: &Pubkey) -> Option<(Pubkey, Vec<u8>)>;
    /// Cluster unix time.
    fn now(&self) -> i64;
}

/// One transaction to send: instructions plus any extra signers (new pool vaults).
pub struct Action {
    pub label: &'static str,
    pub ixs: Vec<Instruction>,
    pub signers: Vec<Keypair>,
}

impl Action {
    fn new(label: &'static str, ixs: Vec<Instruction>) -> Self {
        Action { label, ixs, signers: vec![] }
    }
}

pub fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &PROGRAM_ID).0
}
pub fn config_pda() -> Pubkey {
    pda(&[CONFIG_SEED])
}
pub fn authority() -> Pubkey {
    pda(&[AUTHORITY_SEED])
}
pub fn escrow() -> Pubkey {
    pda(&[ESCROW_SEED])
}
pub fn quote_pda(mint: &Pubkey) -> Pubkey {
    pda(&[QUOTE_SEED, mint.as_ref()])
}

/// Anchor discriminator of `QuoteEntry`, for listing every entry with a memcmp filter.
pub const QUOTE_ENTRY_DISCRIMINATOR: [u8; 8] = [41, 127, 200, 196, 31, 228, 133, 255];

/// Idempotent associated-token-account creation (works for spl-token and token-2022).
pub fn create_ata_ix(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Instruction {
    Instruction {
        program_id: wp::ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(wp::ata(owner, mint, token_program), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

pub struct Crank<'a, L: Ledger> {
    pub ledger: &'a L,
    /// Signs and pays for crank transactions and the rent of new pools and positions.
    pub cranker: Pubkey,
}

impl<'a, L: Ledger> Crank<'a, L> {
    pub fn new(ledger: &'a L, cranker: Pubkey) -> Self {
        Crank { ledger, cranker }
    }

    // -----------------------------------------------------------------------------------------
    // Reads

    pub fn config(&self) -> Config {
        let (_, data) = self.ledger.account(&config_pda()).expect("config account missing");
        Config::try_deserialize(&mut &data[..]).expect("bad config account")
    }

    pub fn quote(&self, mint: &Pubkey) -> QuoteEntry {
        let (_, data) = self.ledger.account(&quote_pda(mint)).expect("quote entry missing");
        QuoteEntry::try_deserialize(&mut &data[..]).expect("bad quote entry")
    }

    /// An Orca pool (route pools, and our pool from before the move to Raydium).
    pub fn pool(&self, key: &Pubkey) -> Option<PoolState> {
        self.ledger.account(key).and_then(|(_, d)| wp::parse_pool(&d))
    }

    /// Token program of a mint: whichever program owns the mint account.
    pub fn token_program(&self, mint: &Pubkey) -> Pubkey {
        self.ledger.account(mint).map(|(owner, _)| owner).unwrap_or(TOKEN_PROGRAM)
    }

    pub fn balance(&self, account: &Pubkey) -> u64 {
        self.ledger
            .account(account)
            .filter(|(_, d)| d.len() >= 72)
            .map(|(_, d)| u64::from_le_bytes(d[64..72].try_into().unwrap()))
            .unwrap_or(0)
    }

    pub fn ours(&self, mint: &Pubkey) -> Pubkey {
        wp::ata(&authority(), mint, &self.token_program(mint))
    }

    fn ensure_ours(&self, mints: &[Pubkey]) -> Vec<Instruction> {
        mints.iter().map(|m| create_ata_ix(&self.cranker, &authority(), m, &self.token_program(m))).collect()
    }

    // -----------------------------------------------------------------------------------------
    // Keeper pokes

    /// Pokes for the given entries' route pools, a few pairs per transaction.
    pub fn poke_quotes(&self, entries: &[QuoteEntry]) -> Vec<Action> {
        entries
            .iter()
            .filter(|q| !q.is_root())
            .collect::<Vec<_>>()
            .chunks(12)
            .map(|chunk| {
                let mut metas = accounts::PokeQuotes {}.to_account_metas(None);
                for q in chunk {
                    metas.push(AccountMeta::new(quote_pda(&q.mint), false));
                    metas.push(AccountMeta::new_readonly(q.route_pool, false));
                }
                Action::new("poke quotes", vec![Instruction { program_id: PROGRAM_ID, accounts: metas, data: instruction::PokeQuotes {}.data() }])
            })
            .collect()
    }

    pub fn poke_pool(&self) -> Option<Action> {
        let c = self.config();
        if c.active_pool == Pubkey::default() || !matches!(c.switch.phase, Phase::Idle | Phase::Requested) {
            return None;
        }
        Some(Action::new(
            "poke pool",
            vec![Instruction {
                program_id: PROGRAM_ID,
                accounts: accounts::PokePool { config: config_pda(), pool: c.active_pool }.to_account_metas(None),
                data: instruction::PokePool {}.data(),
            }],
        ))
    }

    // -----------------------------------------------------------------------------------------
    // Our pools (Raydium)

    pub fn our_pool(&self, key: &Pubkey) -> Option<ray::PoolState> {
        self.ledger.account(key).filter(|(o, _)| *o == ray::CLMM_ID).and_then(|(_, d)| ray::parse_pool(&d))
    }

    /// Whether `pool` is the Orca pool the liquidity lived in before the move to Raydium.
    pub fn is_legacy(&self, pool: &Pubkey) -> bool {
        self.ledger.account(pool).is_some_and(|(o, _)| o == wp::WHIRLPOOL_ID)
    }

    /// sqrt price (token 1 per token 0) of one of our pools, on either venue.
    pub fn our_sqrt(&self, pool: &Pubkey) -> Option<u128> {
        if self.is_legacy(pool) {
            self.pool(pool).map(|p| p.sqrt_price)
        } else {
            self.our_pool(pool).map(|p| p.sqrt_price)
        }
    }

    fn our_pool_accounts(&self, c: &Config, quote: &Pubkey, pool: Pubkey) -> accounts::OurPool {
        let mints = pool_mints(c, quote);
        let vaults = match self.our_pool(&pool) {
            Some(p) => [p.vault_0, p.vault_1],
            None => mints.map(|m| ray::vault_address(&pool, &m)),
        };
        accounts::OurPool {
            pool,
            mint_0: mints[0],
            mint_1: mints[1],
            ours_0: self.ours(&mints[0]),
            ours_1: self.ours(&mints[1]),
            vault_0: vaults[0],
            vault_1: vaults[1],
        }
    }

    fn tick_spacing(&self, c: &Config, pool: &Pubkey) -> i32 {
        self.our_pool(pool).map(|p| p.tick_spacing).unwrap_or(c.tick_spacing) as i32
    }

    /// The live position in slot `i` of `pool`, if any.
    pub fn position(&self, pool: &Pubkey, i: u8) -> Option<ray::PositionState> {
        let mint = position_mint_address(pool, i).0;
        self.ledger.account(&ray::personal_position_address(&mint)).and_then(|(_, d)| ray::parse_position(&d))
    }

    /// Accounts for slot `i`; tick arrays from `ticks`, or from the live position when None.
    fn slot_accounts(&self, pool: &Pubkey, i: u8, ticks: Option<(i32, i32)>, ts: i32) -> accounts::SlotAccounts {
        let mint = position_mint_address(pool, i).0;
        let (lo, hi) = ticks.or_else(|| self.position(pool, i).map(|p| (p.tick_lower, p.tick_upper))).unwrap_or((0, 0));
        accounts::SlotAccounts {
            nft_mint: mint,
            nft_account: wp::ata(&authority(), &mint, &wp::TOKEN_2022_ID),
            personal: ray::personal_position_address(&mint),
            lower: ray::tick_array_address(pool, ray::tick_array_start(lo, ts)),
            upper: ray::tick_array_address(pool, ray::tick_array_start(hi, ts)),
        }
    }

    fn positions(&self, pool: &Pubkey, ticks: [Option<(i32, i32)>; 2], ts: i32) -> accounts::Positions {
        accounts::Positions { slot_0: self.slot_accounts(pool, 0, ticks[0], ts), slot_1: self.slot_accounts(pool, 1, ticks[1], ts) }
    }

    fn programs() -> accounts::Programs {
        accounts::Programs {
            token_program: TOKEN_PROGRAM,
            token_2022_program: wp::TOKEN_2022_ID,
            memo_program: wp::MEMO_ID,
            system_program: system_program::ID,
            associated_token_program: wp::ATA_PROGRAM_ID,
            rent: wp::RENT_SYSVAR_ID,
            clmm_program: ray::CLMM_ID,
        }
    }

    /// Whether `pool` has its sentinel position.
    pub fn has_sentinel(&self, pool: &Pubkey) -> bool {
        self.position(pool, SENTINEL_SLOT).is_some()
    }

    // -----------------------------------------------------------------------------------------
    // Route pools (Orca)

    pub fn sides(&self, pool: &Pubkey, owner: &Pubkey) -> Sides {
        let p = self.pool(pool).expect("pool missing");
        let (pa, pb) = (self.token_program(&p.mint_a), self.token_program(&p.mint_b));
        Sides {
            mint_a: p.mint_a,
            mint_b: p.mint_b,
            program_a: pa,
            program_b: pb,
            owner_a: wp::ata(owner, &p.mint_a, &pa),
            owner_b: wp::ata(owner, &p.mint_b, &pb),
            vault_a: p.vault_a,
            vault_b: p.vault_b,
        }
    }

    fn pool_sides(&self, pool: &Pubkey) -> accounts::PoolSides {
        let s = self.sides(pool, &authority());
        accounts::PoolSides {
            whirlpool: *pool,
            mint_a: s.mint_a,
            mint_b: s.mint_b,
            token_program_a: s.program_a,
            token_program_b: s.program_b,
            ours_a: s.owner_a,
            ours_b: s.owner_b,
            vault_a: s.vault_a,
            vault_b: s.vault_b,
        }
    }

    /// The three tick arrays a swap starting at the current price walks through.
    pub fn swap_arrays(&self, pool: &Pubkey, a_to_b: bool) -> [Pubkey; 3] {
        let p = self.pool(pool).expect("pool missing");
        let span = p.tick_spacing as i32 * math::TICK_ARRAY_SIZE;
        let start = math::tick_array_start(p.tick_current, p.tick_spacing as i32);
        let step = if a_to_b { -span } else { span };
        [0, 1, 2].map(|k| wp::tick_array_address(pool, start + k * step))
    }

    // -----------------------------------------------------------------------------------------
    // Switch steps

    pub fn pull_ix(&self) -> Instruction {
        let c = self.config();
        if self.is_legacy(&c.active_pool) {
            return self.pull_legacy_ix(&c);
        }
        let ts = self.tick_spacing(&c, &c.active_pool);
        Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Pull {
                cranker: self.cranker,
                config: config_pda(),
                authority: authority(),
                pool: self.our_pool_accounts(&c, &c.active_quote, c.active_pool),
                positions: self.positions(&c.active_pool, [None, None], ts),
                programs: Self::programs(),
            }
            .to_account_metas(None),
            data: instruction::Pull {}.data(),
        }
    }

    fn pull_legacy_ix(&self, c: &Config) -> Instruction {
        let pool = self.pool_sides(&c.active_pool);
        let ts = self.pool(&c.active_pool).map(|p| p.tick_spacing as i32).unwrap_or(1);
        let slot = |i: u8| {
            let mint = position_mint_address(&c.active_pool, i).0;
            let position = wp::position_address(&mint);
            let (lo, hi) = self
                .ledger
                .account(&position)
                .and_then(|(_, d)| wp::parse_position(&d))
                .map(|p| (p.tick_lower, p.tick_upper))
                .unwrap_or((0, 0));
            (
                mint,
                position,
                wp::ata(&authority(), &mint, &wp::TOKEN_2022_ID),
                wp::tick_array_address(&c.active_pool, math::tick_array_start(lo, ts)),
                wp::tick_array_address(&c.active_pool, math::tick_array_start(hi, ts)),
            )
        };
        let (s0, s1) = (slot(0), slot(1));
        let fee = c.fee_recipient;
        Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::PullLegacy {
                cranker: self.cranker,
                config: config_pda(),
                authority: authority(),
                fee_a: wp::ata(&fee, &pool.mint_a, &pool.token_program_a),
                fee_b: wp::ata(&fee, &pool.mint_b, &pool.token_program_b),
                pool,
                positions: accounts::OrcaPositions {
                    mint_0: s0.0,
                    position_0: s0.1,
                    nft_0: s0.2,
                    lower_0: s0.3,
                    upper_0: s0.4,
                    mint_1: s1.0,
                    position_1: s1.1,
                    nft_1: s1.2,
                    lower_1: s1.3,
                    upper_1: s1.4,
                },
                token_2022_program: wp::TOKEN_2022_ID,
                memo_program: wp::MEMO_ID,
                whirlpool_program: wp::WHIRLPOOL_ID,
            }
            .to_account_metas(None),
            data: instruction::PullLegacy {}.data(),
        }
    }

    /// The fee recipient's token accounts for the active pool's two mints (idempotent).
    fn fee_atas(&self, c: &Config) -> Vec<Instruction> {
        let mints = pool_mints(c, &c.active_quote);
        mints.iter().map(|m| create_ata_ix(&self.cranker, &c.fee_recipient, m, &self.token_program(m))).collect()
    }

    /// The claim instruction alone, when there is something to claim from (a Raydium pool with
    /// live positions, idle or requested).
    pub fn claim_ix(&self) -> Option<Instruction> {
        let c = self.config();
        if c.active_pool == Pubkey::default()
            || !matches!(c.switch.phase, Phase::Idle | Phase::Requested)
            || self.is_legacy(&c.active_pool)
            || (0..2).all(|i| self.position(&c.active_pool, i).is_none_or(|p| p.liquidity == 0))
        {
            return None;
        }
        let mints = pool_mints(&c, &c.active_quote);
        let fee = c.fee_recipient;
        let ts = self.tick_spacing(&c, &c.active_pool);
        Some(Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::ClaimFees {
                config: config_pda(),
                authority: authority(),
                pool: self.our_pool_accounts(&c, &c.active_quote, c.active_pool),
                positions: self.positions(&c.active_pool, [None, None], ts),
                fee_0: wp::ata(&fee, &mints[0], &self.token_program(&mints[0])),
                fee_1: wp::ata(&fee, &mints[1], &self.token_program(&mints[1])),
                programs: Self::programs(),
            }
            .to_account_metas(None),
            data: instruction::ClaimFees {}.data(),
        })
    }

    /// Collects the live positions' trading fees and pays the fee recipient its share, creating
    /// the recipient's token accounts first. None when there is nothing to claim from.
    pub fn claim_fees(&self) -> Option<Action> {
        let ix = self.claim_ix()?;
        let mut ixs = self.fee_atas(&self.config());
        ixs.push(ix);
        Some(Action::new("claim fees", ixs))
    }

    /// The entry whose route pool the next hop trades through (mirrors the program's rule).
    pub fn next_via(&self) -> Pubkey {
        let c = self.config();
        let target = self.quote(&c.switch.target);
        self.via_from(&c.switch.holding, &target, &c.usdc)
    }

    /// Mirrors the program's hop rule from any holding.
    fn via_from(&self, holding: &Pubkey, target: &QuoteEntry, usdc: &Pubkey) -> Pubkey {
        let h = self.quote(holding);
        let is_ancestor =
            |m: &Pubkey| (*m == target.hub && !target.is_root()) || (*m == *usdc && target.mint != *usdc);
        if is_ancestor(&h.mint) {
            if target.hub == h.mint {
                target.mint
            } else {
                target.hub
            }
        } else {
            h.mint
        }
    }

    pub fn hop_ix(&self, via: Pubkey) -> Instruction {
        self.hop_ix_arrays(self.config().switch.holding, via, false)
    }

    /// A hop that trades `holding` through `via`'s route pool. `compact` passes only the tick
    /// array holding the current price (as all three): enough when the swap stays inside it,
    /// and saves two accounts; if it doesn't, the transaction fails in simulation and the
    /// caller uses the full set.
    pub fn hop_ix_arrays(&self, holding: Pubkey, via: Pubkey, compact: bool) -> Instruction {
        let c = self.config();
        let route = self.quote(&via).route_pool;
        let a_to_b = self.pool(&route).expect("route pool missing").mint_a == holding;
        let mut ta = self.swap_arrays(&route, a_to_b);
        if compact {
            ta = [ta[0]; 3];
        }
        Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Hop {
                config: config_pda(),
                authority: authority(),
                holding: quote_pda(&holding),
                target: quote_pda(&c.switch.target),
                via: quote_pda(&via),
                pool: self.pool_sides(&route),
                tick_array_0: ta[0],
                tick_array_1: ta[1],
                tick_array_2: ta[2],
                oracle: wp::oracle_address(&route),
                memo_program: wp::MEMO_ID,
                whirlpool_program: wp::WHIRLPOOL_ID,
            }
            .to_account_metas(None),
            data: instruction::Hop {}.data(),
        }
    }

    /// The pool the backing is going into, and the price (pool orientation) it must be set to.
    pub fn target_pool(&self) -> (Pubkey, u128) {
        let c = self.config();
        let t = c.switch.target;
        (expected_pool(&c, &t), math::flip(c.switch.target_index_sqrt, !c.index_is_a(&t)))
    }

    /// Tick arrays a reprice from the pool's current price toward `target_sqrt` reaches: the
    /// initialized ones (from the pool's bitmap, plus `extra` starts about to be initialized)
    /// from the current price up to the target's array and the first one past it.
    fn reprice_arrays(&self, pool: &Pubkey, target_sqrt: u128, extra: &[i32]) -> Vec<Pubkey> {
        let Some((_, d)) = self.ledger.account(pool) else { return vec![] };
        let Some(p) = ray::parse_pool(&d) else { return vec![] };
        let ts = p.tick_spacing as i32;
        let mut starts = ray::initialized_tick_arrays(&d, ts);
        starts.extend_from_slice(extra);
        starts.sort();
        starts.dedup();
        let current = ray::tick_array_start(p.tick_current, ts);
        let target = ray::tick_array_start(math::tick_at_sqrt_price(target_sqrt), ts);
        let down = p.sqrt_price > target_sqrt;
        let path: Vec<i32> = if down {
            starts.into_iter().rev().filter(|s| *s <= current).collect()
        } else {
            starts.into_iter().filter(|s| *s >= current).collect()
        };
        let mut out = vec![];
        for s in path {
            out.push(ray::tick_array_address(pool, s));
            if (down && s < target) || (!down && s > target) || out.len() == 10 {
                break;
            }
        }
        out
    }

    /// Start ticks of the sentinel's two tick arrays.
    fn sentinel_starts(ts: i32) -> [i32; 2] {
        let max_t = math::max_usable_tick(ts);
        [ray::tick_array_start(-max_t, ts), ray::tick_array_start(max_t, ts)]
    }

    /// Creates the target pool at the target price, or moves it there. `price_hint` is the
    /// target used to pick tick arrays when the real one is not known yet; `with_sentinel`
    /// counts the sentinel's arrays as initialized (a `seed` lands first in the same transaction).
    pub fn reprice_ix_at(&self, pool_key: Pubkey, price_hint: u128, with_sentinel: bool) -> Instruction {
        let c = self.config();
        let ts = self.tick_spacing(&c, &pool_key);
        let extra = if with_sentinel { Self::sentinel_starts(ts).to_vec() } else { vec![] };
        let mut metas = accounts::Reprice {
            funder: self.cranker,
            config: config_pda(),
            authority: authority(),
            pool: self.our_pool_accounts(&c, &c.switch.target, pool_key),
            amm_config: c.clmm_config,
            observation: ray::observation_address(&pool_key),
            bitmap: ray::bitmap_address(&pool_key),
            programs: Self::programs(),
        }
        .to_account_metas(None);
        metas.extend(self.reprice_arrays(&pool_key, price_hint, &extra).into_iter().map(|k| AccountMeta::new(k, false)));
        Instruction { program_id: PROGRAM_ID, accounts: metas, data: instruction::Reprice {}.data() }
    }

    pub fn reprice_ix(&self) -> Instruction {
        let (pool_key, target_sqrt) = self.target_pool();
        self.reprice_ix_at(pool_key, target_sqrt, false)
    }

    /// Opens the target pool's sentinel (full range).
    pub fn seed_ix(&self, pool_key: Pubkey) -> Instruction {
        let c = self.config();
        let ts = self.tick_spacing(&c, &pool_key);
        let max_t = math::max_usable_tick(ts);
        Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Seed {
                funder: self.cranker,
                config: config_pda(),
                authority: authority(),
                pool: self.our_pool_accounts(&c, &c.switch.target, pool_key),
                sentinel: self.slot_accounts(&pool_key, SENTINEL_SLOT, Some((-max_t, max_t)), ts),
                programs: Self::programs(),
            }
            .to_account_metas(None),
            data: instruction::Seed {}.data(),
        }
    }

    /// Whether `seed` can fund a sentinel from the given balances (index, quote) at
    /// `pool_sqrt` (mirrors the program's rule).
    pub fn seed_feasible(&self, index_is_0: bool, balances: (u64, u64), pool_sqrt: u128, ts: i32) -> bool {
        let max_t = math::max_usable_tick(ts);
        let (b0, b1) = if index_is_0 { balances } else { (balances.1, balances.0) };
        liquidity_for(true, sentinel_share(b0), pool_sqrt, -max_t, max_t) > 1
            && liquidity_for(false, sentinel_share(b1), pool_sqrt, -max_t, max_t) > 1
    }

    /// `add`, with tick arrays for positions planned at `at` (the pool as `add` will see it).
    pub fn add_ix_at(&self, pool_key: Pubkey, at: &ray::PoolState, floor_sqrt: u128) -> Instruction {
        let c = self.config();
        let index_is_0 = c.index_is_a(&c.switch.target);
        let (index_range, backing) = plan_positions(index_is_0, at, floor_sqrt);
        let ts = at.tick_spacing as i32;
        Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Add {
                funder: self.cranker,
                config: config_pda(),
                authority: authority(),
                pool: self.our_pool_accounts(&c, &c.switch.target, pool_key),
                positions: self.positions(&pool_key, [Some(index_range), Some(backing.unwrap_or(index_range))], ts),
                escrow: escrow(),
                mint: c.mint,
                programs: Self::programs(),
            }
            .to_account_metas(None),
            data: instruction::Add {}.data(),
        }
    }

    /// The pool state `add` will see once the pool sits at `sqrt_price`.
    fn pool_at(&self, pool_key: &Pubkey, sqrt_price: u128) -> ray::PoolState {
        let c = self.config();
        let mints = pool_mints(&c, &c.switch.target);
        let mut p = self.our_pool(pool_key).unwrap_or(ray::PoolState {
            amm_config: c.clmm_config,
            mint_0: mints[0],
            mint_1: mints[1],
            vault_0: Pubkey::default(),
            vault_1: Pubkey::default(),
            observation: Pubkey::default(),
            tick_spacing: c.tick_spacing,
            liquidity: 0,
            sqrt_price: 0,
            tick_current: 0,
        });
        p.sqrt_price = sqrt_price;
        p.tick_current = math::tick_at_sqrt_price(sqrt_price);
        p
    }

    pub fn add_ix(&self) -> Instruction {
        let c = self.config();
        let (pool_key, _) = self.target_pool();
        let p = self.our_pool(&pool_key).expect("target pool missing");
        self.add_ix_at(pool_key, &p, c.switch.target_floor_sqrt)
    }

    pub fn abort_ixs(&self) -> Vec<Instruction> {
        let c = self.config();
        let holding = c.switch.holding;
        vec![
            create_ata_ix(&self.cranker, &c.switch.requester, &c.mint, &TOKEN_PROGRAM),
            create_ata_ix(&self.cranker, &authority(), &holding, &self.token_program(&holding)),
            Instruction {
                program_id: PROGRAM_ID,
                accounts: accounts::Abort {
                    config: config_pda(),
                    authority: authority(),
                    mint: c.mint,
                    escrow: escrow(),
                    requester_token: wp::ata(&c.switch.requester, &c.mint, &TOKEN_PROGRAM),
                    old_quote: quote_pda(&c.active_quote),
                    holding: quote_pda(&holding),
                    wsol_quote: quote_pda(&c.wsol),
                    holding_mint: holding,
                    holding_account: self.ours(&holding),
                    token_program: TOKEN_PROGRAM,
                }
                .to_account_metas(None),
                data: instruction::Abort {}.data(),
            },
        ]
    }

    /// The next transaction for the in-flight switch (or launch), or None when idle.
    /// Expired switches are aborted (burn refunded) rather than retried.
    pub fn next(&self) -> Option<Action> {
        let c = self.config();
        let s = c.switch;
        let expired = self.ledger.now() > s.deadline;
        match s.phase {
            Phase::Idle => None,
            Phase::Requested | Phase::Swapping if expired && s.deadline != 0 => Some(Action::new("abort", self.abort_ixs())),
            Phase::Requested => {
                let mut ixs = if c.fee_share_bps > 0 { self.fee_atas(&c) } else { vec![] };
                ixs.extend(self.claim_ix());
                ixs.push(self.pull_ix());
                Some(Action::new("pull", ixs))
            }
            Phase::Swapping => {
                let via = self.next_via();
                let v = self.quote(&via);
                let out = if via == s.holding { v.hub } else { via };
                let mut ixs = self.poke_quotes(&[v]).into_iter().flat_map(|a| a.ixs).collect::<Vec<_>>();
                ixs.extend(self.ensure_ours(&[out]));
                ixs.push(self.hop_ix(via));
                Some(Action::new("hop", ixs))
            }
            Phase::Repricing => {
                let (pool_key, target_sqrt) = self.target_pool();
                let mut ixs = self.ensure_ours(&[c.mint, s.target]);
                let index_is_0 = c.index_is_a(&s.target);
                let Some(p) = self.our_pool(&pool_key) else {
                    ixs.push(self.reprice_ix());
                    return Some(Action::new("create pool", ixs));
                };
                let balances = (self.balance(&self.ours(&c.mint)), self.balance(&self.ours(&s.target)));
                if !self.has_sentinel(&pool_key) && self.seed_feasible(index_is_0, balances, p.sqrt_price, p.tick_spacing as i32) {
                    ixs.push(self.seed_ix(pool_key));
                    Some(Action::new("seed", ixs))
                } else if !within_bps(p.sqrt_price, target_sqrt, PRICE_TOLERANCE_BPS) {
                    ixs.push(self.reprice_ix());
                    Some(Action::new("reprice", ixs))
                } else {
                    ixs.push(self.add_ix());
                    Some(Action::new("add", ixs))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Admin (launch tooling)

pub fn initialize_ix(admin: &Pubkey, mint: &Pubkey, params: InitializeParams) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::Initialize {
            admin: *admin,
            config: config_pda(),
            authority: authority(),
            mint: *mint,
            supply_vault: wp::ata(&authority(), mint, &TOKEN_PROGRAM),
            escrow: escrow(),
            metadata: metaplex::metadata_address(mint),
            token_metadata_program: metaplex::TOKEN_METADATA_ID,
            token_program: TOKEN_PROGRAM,
            associated_token_program: wp::ATA_PROGRAM_ID,
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: instruction::Initialize { params }.data(),
    }
}

/// `hub_route`: (hub mint, route pool), or None for the root (USDC).
pub fn list_quote_ix(admin: &Pubkey, mint: &Pubkey, hub_route: Option<(Pubkey, Pubkey)>) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::ListQuote {
            admin: *admin,
            config: config_pda(),
            quote_mint: *mint,
            quote: quote_pda(mint),
            hub_entry: hub_route.map(|(h, _)| quote_pda(&h)),
            route_pool: hub_route.map(|(_, r)| r),
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: instruction::ListQuote {}.data(),
    }
}

/// `index_sqrt`: sqrt(raw quote per raw index), Q64.64.
pub fn launch_ix(admin: &Pubkey, quote: &Pubkey, index_sqrt: u128) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::Launch { admin: *admin, config: config_pda(), quote: quote_pda(quote) }.to_account_metas(None),
        data: instruction::Launch { index_sqrt }.data(),
    }
}

pub fn set_quote_enabled_ix(admin: &Pubkey, mint: &Pubkey, enabled: bool) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::AdminQuote { admin: *admin, config: config_pda(), quote: quote_pda(mint) }.to_account_metas(None),
        data: instruction::SetQuoteEnabled { enabled }.data(),
    }
}

// ---------------------------------------------------------------------------------------------
// Fast switches

/// Where an in-flight request is expected to land, worked out before anything moves.
#[derive(Clone, Debug)]
pub struct Estimate {
    /// (holding, via) for each hop, in order.
    pub hops: Vec<(Pubkey, Pubkey)>,
    pub target_pool: Pubkey,
    /// Expected target price and floor: pool orientation, and sqrt(quote per index).
    pub pool_sqrt: u128,
    pub floor_sqrt: u128,
    /// The target pool exists already / has its sentinel.
    pub pool_exists: bool,
    pub has_sentinel: bool,
}

/// One step of a fast switch, with roughly how many instruction-trace entries it uses (the
/// runtime allows 64 per transaction, CPIs included).
pub struct Step {
    pub label: &'static str,
    pub ixs: Vec<Instruction>,
    pub trace: usize,
}

impl<'a, L: Ledger> Crank<'a, L> {
    /// Estimates where a requested switch will land: the price moves by the averages' exchange
    /// rate less each hop's pool fee. It is off by a fraction of a percent, which only matters if
    /// it puts a position edge in a different tick array; then the transaction fails in
    /// simulation and the keeper goes step by step.
    pub fn estimate(&self) -> Option<Estimate> {
        let c = self.config();
        if c.switch.phase != Phase::Requested {
            return None;
        }
        let target = self.quote(&c.switch.target);
        let mut hops = vec![];
        let mut holding = c.active_quote;
        let mut fee_keep = 1.0f64;
        while holding != target.mint {
            if hops.len() == 3 {
                return None;
            }
            let via = self.via_from(&holding, &target, &c.usdc);
            let v = self.quote(&via);
            fee_keep *= 1.0 - self.pool(&v.route_pool)?.fee_rate as f64 / 1e6;
            let out = if via == holding { v.hub } else { via };
            hops.push((holding, via));
            holding = out;
        }
        let old_sqrt = self.our_sqrt(&c.active_pool)?;
        let old_index_sqrt = math::flip(old_sqrt, !c.index_is_a(&c.active_quote));
        let rate = math::mul_sqrt(c.switch.ema_rate_sqrt, (fee_keep.sqrt() * math::Q64 as f64) as u128);
        let index_sqrt = math::mul_sqrt(old_index_sqrt, rate);
        let target_pool = expected_pool(&c, &target.mint);
        Some(Estimate {
            hops,
            target_pool,
            pool_sqrt: math::flip(index_sqrt, !c.index_is_a(&target.mint)),
            floor_sqrt: math::mul_sqrt(c.floor_sqrt, rate),
            pool_exists: self.our_pool(&target_pool).is_some(),
            has_sentinel: self.has_sentinel(&target_pool),
        })
    }

    /// Everything a fast switch needs set up first, as normal transactions: fresh averages for
    /// the pools it reads, token accounts along the path, and the fees earned so far (claimed
    /// now so they are not swept into the backing). None of it touches liquidity.
    pub fn fast_prep(&self, e: &Estimate) -> Vec<Action> {
        let c = self.config();
        let mut out = vec![];
        let mut entries = vec![self.quote(&c.active_quote)];
        for (_, via) in &e.hops {
            entries.push(self.quote(via));
        }
        out.extend(self.poke_quotes(&entries));
        out.extend(self.poke_pool());
        let mut mints = vec![c.mint, c.switch.target];
        for (holding, via) in &e.hops {
            mints.push(*holding);
            let v = self.quote(via);
            mints.push(if *via == *holding { v.hub } else { *via });
        }
        mints.sort();
        mints.dedup();
        let mut ixs = self.ensure_ours(&mints);
        if c.fee_share_bps > 0 {
            ixs.extend(self.fee_atas(&c));
        }
        out.push(Action::new("prep accounts", ixs));
        out.extend(self.claim_fees());
        out
    }

    /// The switch as steps, in order: pull, each hop, then into the target pool. A new pool is
    /// created at the target, seeded, then filled; a known pool without a sentinel is seeded,
    /// repriced, then filled; one with a sentinel is repriced and filled. `compact` trims hop
    /// swaps to one tick array. The keeper packs consecutive steps into as few transactions as
    /// the limits allow.
    pub fn fast_steps(&self, e: &Estimate, compact: bool) -> Vec<Step> {
        let c = self.config();
        let legacy = self.is_legacy(&c.active_pool);
        // Trace estimates are measured maxima (tests-svm prints them) plus a little slack.
        let mut steps = vec![Step { label: "pull", ixs: vec![self.pull_ix()], trace: if legacy { 20 } else { 16 } }];
        for (holding, via) in &e.hops {
            steps.push(Step { label: "hop", ixs: vec![self.hop_ix_arrays(*holding, *via, compact)], trace: 6 });
        }
        let pool = e.target_pool;
        let reprice = |with_sentinel| self.reprice_ix_at(pool, e.pool_sqrt, with_sentinel);
        let seed = Step { label: "seed", ixs: vec![self.seed_ix(pool)], trace: 20 };
        if !e.pool_exists {
            steps.push(Step { label: "create pool", ixs: vec![reprice(false)], trace: 10 });
            steps.push(seed);
        } else if !e.has_sentinel {
            steps.push(seed);
            steps.push(Step { label: "reprice", ixs: vec![reprice(true)], trace: 6 });
        } else {
            steps.push(Step { label: "reprice", ixs: vec![reprice(false)], trace: 6 });
        }
        let at = self.pool_at(&pool, e.pool_sqrt);
        steps.push(Step { label: "add", ixs: vec![self.add_ix_at(pool, &at, e.floor_sqrt)], trace: 34 });
        steps
    }
}

/// A holder's switch request (burns `burn_amount` of the coin into escrow).
pub fn request_switch_ix(user: &Pubkey, config: &Config, target: &Pubkey) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::RequestSwitch {
            user: *user,
            config: config_pda(),
            mint: config.mint,
            user_token: wp::ata(user, &config.mint, &TOKEN_PROGRAM),
            escrow: escrow(),
            old_quote: quote_pda(&config.active_quote),
            new_quote: quote_pda(target),
            wsol_quote: quote_pda(&config.wsol),
            token_program: TOKEN_PROGRAM,
        }
        .to_account_metas(None),
        data: instruction::RequestSwitch {}.data(),
    }
}

/// Most accounts one transaction may lock, and instruction-trace entries (CPIs included) it may
/// use. Two instructions and one account go to the compute budget.
pub const MAX_TX_ACCOUNTS: usize = 64;
pub const MAX_TRACE: usize = 64;
pub const COMPUTE_BUDGET: Pubkey = anchor_lang::prelude::pubkey!("ComputeBudget111111111111111111111111111111");

/// Unique accounts a transaction with `ixs` (plus compute budget) locks, payer and programs included.
pub fn unique_accounts(ixs: &[Instruction], payer: &Pubkey) -> usize {
    let mut keys: Vec<Pubkey> = ixs.iter().flat_map(|ix| ix.accounts.iter().map(|m| m.pubkey).chain([ix.program_id])).collect();
    keys.push(*payer);
    keys.push(COMPUTE_BUDGET);
    keys.sort();
    keys.dedup();
    keys.len()
}

/// Packs consecutive steps into as few transactions as the account and trace limits allow,
/// in order. Returns (step labels, instructions) per transaction.
pub fn pack(steps: Vec<Step>, payer: &Pubkey) -> Vec<(Vec<&'static str>, Vec<Instruction>)> {
    let mut out: Vec<(Vec<&'static str>, Vec<Instruction>, usize)> = vec![];
    for s in steps {
        if let Some(last) = out.last_mut() {
            let mut ixs = last.1.clone();
            ixs.extend(s.ixs.iter().cloned());
            if unique_accounts(&ixs, payer) <= MAX_TX_ACCOUNTS && last.2 + s.trace <= MAX_TRACE - 2 {
                last.0.push(s.label);
                last.1 = ixs;
                last.2 += s.trace;
                continue;
            }
        }
        out.push((vec![s.label], s.ixs, s.trace));
    }
    out.into_iter().map(|(l, i, _)| (l, i)).collect()
}

/// Fee tier future pools are created under (Raydium AmmConfig and its tick spacing).
pub fn set_pool_venue_ix(admin: &Pubkey, clmm_config: &Pubkey, tick_spacing: u16) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::AdminConfig { admin: *admin, config: config_pda() }.to_account_metas(None),
        data: instruction::SetPoolVenue { clmm_config: *clmm_config, tick_spacing }.data(),
    }
}
