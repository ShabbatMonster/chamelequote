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
    accounts, damm, instruction,
    instructions::{clamp_to_range, expected_pool, liquidity_for, position_mint_address, InitializeParams},
    math, metaplex,
    state::{
        sentinel_share, within_bps, SENTINEL_DIVISOR, Config, Phase, QuoteEntry, AUTHORITY_SEED, CONFIG_SEED, ESCROW_SEED, MAIN_SLOT,
        PRICE_TOLERANCE_BPS, QUOTE_SEED, SENTINEL_SLOT,
    },
    whirlpool::{self as wp, PoolState, Sides},
    ID as PROGRAM_ID,
};
use solana_keypair::Keypair;

pub const TOKEN_PROGRAM: Pubkey = anchor_lang::prelude::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const INSTRUCTIONS_SYSVAR: Pubkey = anchor_lang::prelude::pubkey!("Sysvar1nstructions1111111111111111111111111");

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
    // Our pools (Meteora DAMM v2)

    pub fn our_pool(&self, key: &Pubkey) -> Option<damm::PoolState> {
        self.ledger.account(key).filter(|(o, _)| *o == damm::DAMM_ID).and_then(|(_, d)| damm::parse_pool(&d))
    }

    /// sqrt(quote per index) on our pool.
    pub fn our_sqrt(&self, pool: &Pubkey) -> Option<u128> {
        self.our_pool(pool).map(|p| p.sqrt_price)
    }

    fn our_pool_accounts(&self, c: &Config, quote: &Pubkey) -> accounts::OurPool {
        let pool = expected_pool(c, quote);
        let vaults = match self.our_pool(&pool) {
            Some(p) => [p.vault_a, p.vault_b],
            None => [damm::vault_address(&pool, &c.mint), damm::vault_address(&pool, quote)],
        };
        accounts::OurPool {
            pool,
            index_mint: c.mint,
            quote_mint: *quote,
            ours_index: self.ours(&c.mint),
            ours_quote: self.ours(quote),
            vault_a: vaults[0],
            vault_b: vaults[1],
        }
    }

    fn slot(pool: &Pubkey, i: u8) -> accounts::Slot {
        let nft_mint = position_mint_address(pool, i).0;
        accounts::Slot {
            nft_mint,
            nft_account: damm::nft_account_address(&nft_mint),
            position: damm::position_address(&nft_mint),
        }
    }

    /// Liquidity of the position in slot `i` of `pool`, if it is open.
    pub fn position(&self, pool: &Pubkey, i: u8) -> Option<u128> {
        let nft = position_mint_address(pool, i).0;
        self.ledger.account(&damm::position_address(&nft)).and_then(|(_, d)| damm::parse_position(&d)).map(|p| p.1)
    }

    pub fn has_sentinel(&self, pool: &Pubkey) -> bool {
        self.position(pool, SENTINEL_SLOT).is_some()
    }

    fn programs() -> accounts::Programs {
        accounts::Programs {
            token_program: TOKEN_PROGRAM,
            token_2022_program: wp::TOKEN_2022_ID,
            system_program: system_program::ID,
            damm_program: damm::DAMM_ID,
            pool_authority: damm::pool_authority(),
            event_authority: damm::event_authority(),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Switch steps

    pub fn pull_ix(&self) -> Instruction {
        let c = self.config();
        let fee = c.fee_recipient;
        Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Pull {
                cranker: self.cranker,
                config: config_pda(),
                authority: authority(),
                pool: self.our_pool_accounts(&c, &c.active_quote),
                position: Self::slot(&c.active_pool, MAIN_SLOT),
                fee_index: wp::ata(&fee, &c.mint, &TOKEN_PROGRAM),
                fee_quote: wp::ata(&fee, &c.active_quote, &self.token_program(&c.active_quote)),
                instructions: INSTRUCTIONS_SYSVAR,
                programs: Self::programs(),
            }
            .to_account_metas(None),
            data: instruction::Pull {}.data(),
        }
    }

    /// The fee recipient's token accounts for the active pool's two mints (idempotent).
    fn fee_atas(&self, c: &Config) -> Vec<Instruction> {
        [c.mint, c.active_quote].iter().map(|m| create_ata_ix(&self.cranker, &c.fee_recipient, m, &self.token_program(m))).collect()
    }

    /// The claim instruction alone, when there is something to claim from (a DAMM pool with a
    /// live position, idle or requested).
    pub fn claim_ix(&self) -> Option<Instruction> {
        let c = self.config();
        if c.active_pool == Pubkey::default()
            || !matches!(c.switch.phase, Phase::Idle | Phase::Requested)
            || self.position(&c.active_pool, MAIN_SLOT).is_none()
        {
            return None;
        }
        let fee = c.fee_recipient;
        Some(Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::ClaimFees {
                config: config_pda(),
                authority: authority(),
                pool: self.our_pool_accounts(&c, &c.active_quote),
                position: Self::slot(&c.active_pool, MAIN_SLOT),
                fee_index: wp::ata(&fee, &c.mint, &TOKEN_PROGRAM),
                fee_quote: wp::ata(&fee, &c.active_quote, &self.token_program(&c.active_quote)),
                programs: Self::programs(),
            }
            .to_account_metas(None),
            data: instruction::ClaimFees {}.data(),
        })
    }

    /// Collects the live position's trading fees and pays the fee recipient its share, creating
    /// the recipient's token accounts first. None when there is nothing to claim from.
    pub fn claim_fees(&self) -> Option<Action> {
        let ix = self.claim_ix()?;
        let mut ixs = self.fee_atas(&self.config());
        ixs.push(ix);
        Some(Action::new("claim fees", ixs))
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
    /// The pool the backing is going into, and the price (quote per index) it must be set to.
    pub fn target_pool(&self) -> (Pubkey, u128) {
        let c = self.config();
        (expected_pool(&c, &c.switch.target), c.switch.target_index_sqrt)
    }

    pub fn reprice_ix(&self) -> Instruction {
        let c = self.config();
        Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Reprice {
                config: config_pda(),
                authority: authority(),
                pool: self.our_pool_accounts(&c, &c.switch.target),
                programs: Self::programs(),
            }
            .to_account_metas(None),
            data: instruction::Reprice {}.data(),
        }
    }

    /// Opens a pool's sentinel: the active pool's when idle, the target's mid-switch.
    pub fn seed_ix(&self) -> Instruction {
        let c = self.config();
        let quote = if c.switch.phase == Phase::Idle { c.active_quote } else { c.switch.target };
        let pool = expected_pool(&c, &quote);
        Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Seed {
                funder: self.cranker,
                config: config_pda(),
                authority: authority(),
                pool: self.our_pool_accounts(&c, &quote),
                position: Self::slot(&pool, MAIN_SLOT),
                sentinel: Self::slot(&pool, SENTINEL_SLOT),
                programs: Self::programs(),
            }
            .to_account_metas(None),
            data: instruction::Seed {}.data(),
        }
    }

    pub fn add_ix(&self) -> Instruction {
        let c = self.config();
        let pool = expected_pool(&c, &c.switch.target);
        let mut metas = accounts::Add {
            funder: self.cranker,
            config: config_pda(),
            authority: authority(),
            pool: self.our_pool_accounts(&c, &c.switch.target),
            position: Self::slot(&pool, MAIN_SLOT),
            escrow: escrow(),
            mint: c.mint,
            programs: Self::programs(),
        }
        .to_account_metas(None);
        // Token badges for a new pool (Meteora reads the one a token-2022 quote needs).
        for m in [c.mint, c.switch.target] {
            metas.push(AccountMeta::new_readonly(damm::token_badge_address(&m), false));
        }
        Instruction { program_id: PROGRAM_ID, accounts: metas, data: instruction::Add {}.data() }
    }

    /// Whether `seed` can fund a sentinel from the program's balances (mirrors the program's
    /// rule): idle, for the active pool from what `add` set aside; mid-switch, for the target.
    pub fn seed_feasible(&self) -> bool {
        let c = self.config();
        let idle = c.switch.phase == Phase::Idle;
        let quote = if idle { c.active_quote } else { c.switch.target };
        let pool = expected_pool(&c, &quote);
        let Some(p) = self.our_pool(&pool) else { return false };
        let (i, q) = (self.balance(&self.ours(&c.mint)), self.balance(&self.ours(&quote)));
        let l = if idle {
            let Some(main) = self.position(&pool, MAIN_SLOT) else { return false };
            liquidity_for(i, q, p.sqrt_price, p.sqrt_min, p.sqrt_max).min(main / SENTINEL_DIVISOR as u128)
        } else {
            liquidity_for(sentinel_share(i), sentinel_share(q), p.sqrt_price, p.sqrt_min, p.sqrt_max)
        };
        l > 0
    }

    /// Token accounts a switch needs that don't exist yet (ours along the route, the fee
    /// recipient's for the pool being left), as idempotent creations.
    pub fn missing_accounts(&self, e: &Estimate) -> Vec<Instruction> {
        let c = self.config();
        let mut mints = vec![c.mint, c.switch.target];
        for (holding, via) in &e.hops {
            mints.push(*holding);
            let v = self.quote(via);
            mints.push(if *via == *holding { v.hub } else { *via });
        }
        mints.sort();
        mints.dedup();
        let mut ixs: Vec<Instruction> = mints
            .iter()
            .filter(|m| self.ledger.account(&self.ours(m)).is_none())
            .map(|m| create_ata_ix(&self.cranker, &authority(), m, &self.token_program(m)))
            .collect();
        for m in [c.mint, c.active_quote] {
            if self.ledger.account(&wp::ata(&c.fee_recipient, &m, &self.token_program(&m))).is_none() {
                ixs.push(create_ata_ix(&self.cranker, &c.fee_recipient, &m, &self.token_program(&m)));
            }
        }
        ixs
    }

    /// The whole switch as ONE transaction (pull, hops, then reprice and add on the target), or
    /// an error if it doesn't fit: the program only lets the pull go through together with the
    /// add, so a switch lands whole or not at all.
    pub fn switch_tx(&self, e: &Estimate, compact: bool) -> Result<Vec<Instruction>, String> {
        let txs = pack(self.fast_steps(e, compact), &self.cranker);
        match <[_; 1]>::try_from(txs) {
            Ok([(_, ixs)]) => Ok(ixs),
            Err(txs) => Err(format!("switch needs {} transactions: {:?}", txs.len(), txs.iter().map(|t| &t.0).collect::<Vec<_>>())),
        }
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
    /// The next transaction for the in-flight switch (or launch), or None when idle.
    /// Expired switches are aborted (burn refunded) rather than retried.
    pub fn next(&self) -> Option<Action> {
        let c = self.config();
        let s = c.switch;
        let expired = self.ledger.now() > s.deadline;
        match s.phase {
            // Right after a switch into a new pool: its sentinel, from the share `add` set aside.
            Phase::Idle => (c.active_pool != Pubkey::default()
                && self.our_pool(&c.active_pool).is_some()
                && !self.has_sentinel(&c.active_pool)
                && self.seed_feasible())
            .then(|| Action::new("seed", vec![self.seed_ix()])),
            Phase::Requested | Phase::Swapping if expired && s.deadline != 0 => Some(Action::new("abort", self.abort_ixs())),
            Phase::Requested => {
                let e = self.estimate()?;
                let prep = self.missing_accounts(&e);
                if !prep.is_empty() {
                    return Some(Action::new("prep accounts", prep));
                }
                // Not fitting means it can't be done; the request waits out its deadline.
                let ixs = self.switch_tx(&e, true).or_else(|_| self.switch_tx(&e, false)).ok()?;
                Some(Action::new("switch", ixs))
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
                let Some(p) = self.our_pool(&pool_key) else {
                    ixs.push(self.add_ix());
                    return Some(Action::new("create pool + add", ixs));
                };
                if !self.has_sentinel(&pool_key) && self.seed_feasible() {
                    ixs.push(self.seed_ix());
                    Some(Action::new("seed", ixs))
                } else if !within_bps(p.sqrt_price, clamp_to_range(target_sqrt, &p), PRICE_TOLERANCE_BPS) {
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
    /// The route a requested switch takes, and what the target pool needs.
    pub fn estimate(&self) -> Option<Estimate> {
        let c = self.config();
        if c.switch.phase != Phase::Requested {
            return None;
        }
        let target = self.quote(&c.switch.target);
        let mut hops = vec![];
        let mut holding = c.active_quote;
        while holding != target.mint {
            if hops.len() == 3 {
                return None;
            }
            let via = self.via_from(&holding, &target, &c.usdc);
            let v = self.quote(&via);
            let out = if via == holding { v.hub } else { via };
            hops.push((holding, via));
            holding = out;
        }
        let target_pool = expected_pool(&c, &target.mint);
        Some(Estimate {
            hops,
            target_pool,
            pool_exists: self.our_pool(&target_pool).is_some(),
            has_sentinel: self.has_sentinel(&target_pool),
        })
    }

    /// Everything a fast switch needs set up first, as normal transactions: fresh averages for
    /// the pools it reads, token accounts along the path, and the fees earned so far. None of it
    /// touches liquidity.
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
        ixs.extend(self.fee_atas(&c));
        out.push(Action::new("prep accounts", ixs));
        out.extend(self.claim_fees());
        out
    }

    /// The switch as steps, in order: pull, each hop, then into the target pool. A new pool is
    /// created by `add` (its sentinel comes after, from idle); a known pool without a sentinel is
    /// seeded, then repriced; one with a sentinel is repriced. `compact` trims hop swaps to one
    /// tick array. They have to pack into one transaction (`switch_tx`).
    pub fn fast_steps(&self, e: &Estimate, compact: bool) -> Vec<Step> {
        // Trace estimates are measured maxima (tests-svm prints them) plus a little slack.
        let mut steps = vec![Step { label: "pull", ixs: vec![self.pull_ix()], trace: 12 }];
        for (holding, via) in &e.hops {
            steps.push(Step { label: "hop", ixs: vec![self.hop_ix_arrays(*holding, *via, compact)], trace: 5 });
        }
        if e.pool_exists {
            if !e.has_sentinel {
                steps.push(Step { label: "seed", ixs: vec![self.seed_ix()], trace: 18 });
            }
            steps.push(Step { label: "reprice", ixs: vec![self.reprice_ix()], trace: 5 });
            steps.push(Step { label: "add", ixs: vec![self.add_ix()], trace: 17 });
        } else {
            steps.push(Step { label: "create pool + add", ixs: vec![self.add_ix()], trace: 28 });
        }
        steps
    }
}

/// A holder's switch request (burns `burn_amount` of the coin into escrow; a switch into a new
/// pool also pays NEW_POOL_FEE_LAMPORTS to the fee recipient).
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
            target_pool: expected_pool(config, target),
            fee_recipient: config.fee_recipient,
            token_program: TOKEN_PROGRAM,
            system_program: system_program::ID,
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
