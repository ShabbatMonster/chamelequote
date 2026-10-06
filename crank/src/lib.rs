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
    instructions::{plan_positions, position_mint_address, InitializeParams},
    metaplex,
    math,
    state::{Config, Phase, QuoteEntry, AUTHORITY_SEED, CONFIG_SEED, ESCROW_SEED, QUOTE_SEED},
    whirlpool::{self as wp, PoolState, Sides},
    ID as PROGRAM_ID,
};
use solana_keypair::Keypair;
use solana_signer::Signer;

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
    /// Signs and pays for crank transactions; receives the rent of closed positions.
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
        if c.active_pool == Pubkey::default() || c.switch.phase != Phase::Idle {
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
    // Account groups

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

    /// Position slots; tick arrays from `ticks`, or from the live position when None.
    fn positions(&self, pool: &Pubkey, ticks: [Option<(i32, i32)>; 2]) -> accounts::Positions {
        let ts = self.pool(pool).map(|p| p.tick_spacing as i32).unwrap_or(1);
        let slot = |i: u8| {
            let mint = position_mint_address(pool, i).0;
            let position = wp::position_address(&mint);
            let (lo, hi) = ticks[i as usize]
                .or_else(|| {
                    self.ledger.account(&position).and_then(|(_, d)| wp::parse_position(&d)).map(|p| (p.tick_lower, p.tick_upper))
                })
                .unwrap_or((0, 0));
            (
                mint,
                position,
                wp::ata(&authority(), &mint, &wp::TOKEN_2022_ID),
                wp::tick_array_address(pool, math::tick_array_start(lo, ts)),
                wp::tick_array_address(pool, math::tick_array_start(hi, ts)),
            )
        };
        let (s0, s1) = (slot(0), slot(1));
        accounts::Positions {
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
        let pool = self.pool_sides(&c.active_pool);
        let fee = c.fee_recipient;
        let fee_a = wp::ata(&fee, &pool.mint_a, &pool.token_program_a);
        let fee_b = wp::ata(&fee, &pool.mint_b, &pool.token_program_b);
        Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Pull {
                cranker: self.cranker,
                config: config_pda(),
                authority: authority(),
                positions: self.positions(&c.active_pool, [None, None]),
                pool,
                fee_a,
                fee_b,
                token_2022_program: wp::TOKEN_2022_ID,
                memo_program: wp::MEMO_ID,
                whirlpool_program: wp::WHIRLPOOL_ID,
            }
            .to_account_metas(None),
            data: instruction::Pull {}.data(),
        }
    }

    /// Collects the live positions' trading fees and pays the fee recipient its share. Includes
    /// creating the recipient's token accounts. None while a switch is in flight or before launch.
    pub fn claim_fees(&self) -> Option<Action> {
        let c = self.config();
        if c.active_pool == Pubkey::default() || c.switch.phase != Phase::Idle {
            return None;
        }
        let pool = self.pool_sides(&c.active_pool);
        let fee = c.fee_recipient;
        let fee_a = wp::ata(&fee, &pool.mint_a, &pool.token_program_a);
        let fee_b = wp::ata(&fee, &pool.mint_b, &pool.token_program_b);
        let ixs = vec![
            create_ata_ix(&self.cranker, &fee, &pool.mint_a, &pool.token_program_a),
            create_ata_ix(&self.cranker, &fee, &pool.mint_b, &pool.token_program_b),
            Instruction {
                program_id: PROGRAM_ID,
                accounts: accounts::ClaimFees {
                    config: config_pda(),
                    authority: authority(),
                    positions: self.positions(&c.active_pool, [None, None]),
                    pool,
                    fee_a,
                    fee_b,
                    memo_program: wp::MEMO_ID,
                    whirlpool_program: wp::WHIRLPOOL_ID,
                }
                .to_account_metas(None),
                data: instruction::ClaimFees {}.data(),
            },
        ];
        Some(Action::new("claim fees", ixs))
    }

    /// The entry whose route pool the next hop trades through (mirrors the program's rule).
    pub fn next_via(&self) -> Pubkey {
        let c = self.config();
        let holding = self.quote(&c.switch.holding);
        let target = self.quote(&c.switch.target);
        let is_ancestor =
            |h: &Pubkey| (*h == target.hub && !target.is_root()) || (*h == c.usdc && target.mint != c.usdc);
        if is_ancestor(&holding.mint) {
            if target.hub == holding.mint {
                target.mint
            } else {
                target.hub
            }
        } else {
            holding.mint
        }
    }

    pub fn hop_ix(&self, via: Pubkey) -> Instruction {
        self.hop_ix_from(self.config().switch.holding, via)
    }

    /// A hop that trades `holding` through `via`'s route pool.
    pub fn hop_ix_from(&self, holding: Pubkey, via: Pubkey) -> Instruction {
        self.hop_ix_arrays(holding, via, false)
    }

    /// `compact`: pass only the tick array holding the current price (as all three). Enough when
    /// the swap stays inside it, and saves two accounts; if it doesn't, the transaction fails in
    /// simulation and the caller uses the full set.
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
        let index_is_a = c.index_is_a(&t);
        let (a, b) = if index_is_a { (c.mint, t) } else { (t, c.mint) };
        (
            wp::whirlpool_address(&c.whirlpools_config, &a, &b, c.tick_spacing),
            math::flip(c.switch.target_index_sqrt, !index_is_a),
        )
    }

    pub fn reprice_ix(&self) -> Instruction {
        let (pool_key, target_sqrt) = self.target_pool();
        let a_to_b = self.pool(&pool_key).expect("target pool missing").sqrt_price > target_sqrt;
        let ta = self.swap_arrays(&pool_key, a_to_b);
        Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Reprice {
                config: config_pda(),
                authority: authority(),
                pool: self.pool_sides(&pool_key),
                tick_array_0: ta[0],
                tick_array_1: ta[1],
                tick_array_2: ta[2],
                oracle: wp::oracle_address(&pool_key),
                memo_program: wp::MEMO_ID,
                whirlpool_program: wp::WHIRLPOOL_ID,
            }
            .to_account_metas(None),
            data: instruction::Reprice {}.data(),
        }
    }

    /// Tick arrays the two new positions need, and the `add` instruction itself.
    pub fn add_ixs(&self) -> (Vec<Instruction>, Instruction) {
        let c = self.config();
        let (pool_key, _) = self.target_pool();
        let pool = self.pool_sides(&pool_key);
        let ps = self.pool(&pool_key).expect("target pool missing");
        let index_is_a = c.index_is_a(&c.switch.target);
        let (ours_index, ours_quote) = if index_is_a { (pool.ours_a, pool.ours_b) } else { (pool.ours_b, pool.ours_a) };
        let plan = plan_positions(index_is_a, &ps, c.switch.target_floor_sqrt, self.balance(&ours_index), self.balance(&ours_quote));
        let ts = ps.tick_spacing as i32;
        let mut starts: Vec<i32> = plan
            .iter()
            .filter(|p| p.2 > 0)
            .flat_map(|p| [math::tick_array_start(p.0, ts), math::tick_array_start(p.1, ts)])
            .collect();
        starts.sort();
        starts.dedup();
        let init: Vec<_> = starts.iter().map(|s| wp::init_tick_array_ix(pool_key, self.cranker, *s)).collect();
        let add = Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Add {
                funder: self.cranker,
                config: config_pda(),
                authority: authority(),
                positions: self.positions(&pool_key, plan.map(|p| Some((p.0, p.1)))),
                pool,
                escrow: escrow(),
                mint: c.mint,
                token_program: TOKEN_PROGRAM,
                token_2022_program: wp::TOKEN_2022_ID,
                system_program: system_program::ID,
                associated_token_program: wp::ATA_PROGRAM_ID,
                memo_program: wp::MEMO_ID,
                nft_update_auth: wp::NFT_UPDATE_AUTH,
                whirlpool_program: wp::WHIRLPOOL_ID,
            }
            .to_account_metas(None),
            data: instruction::Add {}.data(),
        };
        (init, add)
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
                let p = self.pool(&c.active_pool).expect("active pool missing");
                let mut ixs = vec![];
                if c.fee_share_bps > 0 {
                    for m in [p.mint_a, p.mint_b] {
                        ixs.push(create_ata_ix(&self.cranker, &c.fee_recipient, &m, &self.token_program(&m)));
                    }
                }
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
                let ensure = self.ensure_ours(&[c.mint, s.target]);
                match self.pool(&pool_key) {
                    None => {
                        let (a, b) = if c.index_is_a(&s.target) { (c.mint, s.target) } else { (s.target, c.mint) };
                        let (va, vb) = (Keypair::new(), Keypair::new());
                        let ix = wp::initialize_pool_ix(
                            c.whirlpools_config,
                            a,
                            b,
                            self.token_program(&a),
                            self.token_program(&b),
                            self.cranker,
                            va.pubkey(),
                            vb.pubkey(),
                            c.tick_spacing,
                            target_sqrt,
                        );
                        Some(Action { label: "create pool", ixs: vec![ix], signers: vec![va, vb] })
                    }
                    Some(p) if p.sqrt_price != target_sqrt => {
                        let mut ixs = ensure;
                        ixs.push(self.reprice_ix());
                        Some(Action::new("reprice", ixs))
                    }
                    Some(_) => {
                        let (init, add) = self.add_ixs();
                        let mut ixs = ensure;
                        ixs.extend(init);
                        ixs.push(add);
                        Some(Action::new("add", ixs))
                    }
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
// Atomic switches (Jito bundles)

/// Where an in-flight request is expected to land, worked out before anything moves.
#[derive(Clone, Debug)]
pub struct Estimate {
    /// (holding, via) for each hop, in order.
    pub hops: Vec<(Pubkey, Pubkey)>,
    pub target_pool: Pubkey,
    /// Expected target price, pool orientation.
    pub pool_sqrt: u128,
    /// Ticks of the two positions `add` is expected to open (lower, upper, has liquidity).
    pub positions: [(i32, i32, bool); 2],
}

impl<'a, L: Ledger> Crank<'a, L> {
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

    /// Estimates where a requested switch will land: the price moves by the averages' exchange
    /// rate less each hop's pool fee. It is off by a fraction of a percent, which only matters if
    /// it puts a position edge in a different tick array; then the bundle fails harmlessly and the
    /// keeper falls back to step-by-step.
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
        let old = self.pool(&c.active_pool)?;
        let old_index_is_a = c.index_is_a(&c.active_quote);
        let old_index_sqrt = math::flip(old.sqrt_price, !old_index_is_a);
        let rate = math::mul_sqrt(c.switch.ema_rate_sqrt, (fee_keep.sqrt() * math::Q64 as f64) as u128);
        let index_sqrt = math::mul_sqrt(old_index_sqrt, rate);
        let floor_sqrt = math::mul_sqrt(c.floor_sqrt, rate);
        let index_is_a = c.index_is_a(&target.mint);
        let (a, b) = if index_is_a { (c.mint, target.mint) } else { (target.mint, c.mint) };
        let target_pool = wp::whirlpool_address(&c.whirlpools_config, &a, &b, c.tick_spacing);
        let pool_sqrt = math::flip(index_sqrt, !index_is_a);
        let est = PoolState {
            whirlpools_config: c.whirlpools_config,
            tick_spacing: c.tick_spacing,
            fee_rate: 0,
            liquidity: 0,
            sqrt_price: pool_sqrt,
            tick_current: math::tick_at_sqrt_price(pool_sqrt),
            mint_a: a,
            vault_a: Pubkey::default(),
            mint_b: b,
            vault_b: Pubkey::default(),
        };
        // Only whether each side is non-empty matters for the ticks.
        let (old_index_vault, old_quote_vault) = if old_index_is_a { (old.vault_a, old.vault_b) } else { (old.vault_b, old.vault_a) };
        let index_side = self.balance(&self.ours(&c.mint)) + self.balance(&old_index_vault);
        let quote_side = self.balance(&old_quote_vault);
        let plan = plan_positions(index_is_a, &est, floor_sqrt, index_side.max(1), quote_side);
        Some(Estimate { hops, target_pool, pool_sqrt, positions: plan.map(|p| (p.0, p.1, p.2 > 0)) })
    }

    /// Step one of an atomic switch: everything the bundle needs, created up front with normal
    /// transactions. None of it touches liquidity.
    pub fn bundle_prep(&self, e: &Estimate) -> Vec<Action> {
        let c = self.config();
        let mut out = vec![];
        // Fresh averages for every pool the bundle reads.
        let mut entries = vec![self.quote(&c.active_quote)];
        for (_, via) in &e.hops {
            entries.push(self.quote(via));
        }
        out.extend(self.poke_quotes(&entries));
        out.extend(self.poke_pool());
        // Token accounts: ours along the path, and the fee recipient's for the pool being left.
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
            if let Some(p) = self.pool(&c.active_pool) {
                for m in [p.mint_a, p.mint_b] {
                    ixs.push(create_ata_ix(&self.cranker, &c.fee_recipient, &m, &self.token_program(&m)));
                }
            }
        }
        out.push(Action::new("prep accounts", ixs));
        if self.pool(&e.target_pool).is_none() {
            let (a, b) = if c.index_is_a(&c.switch.target) { (c.mint, c.switch.target) } else { (c.switch.target, c.mint) };
            let (va, vb) = (Keypair::new(), Keypair::new());
            let ix = wp::initialize_pool_ix(
                c.whirlpools_config,
                a,
                b,
                self.token_program(&a),
                self.token_program(&b),
                self.cranker,
                va.pubkey(),
                vb.pubkey(),
                c.tick_spacing,
                e.pool_sqrt,
            );
            out.push(Action { label: "create pool", ixs: vec![ix], signers: vec![va, vb] });
        }
        let ts = c.tick_spacing as i32;
        let mut starts: Vec<i32> = e
            .positions
            .iter()
            .filter(|p| p.2)
            .flat_map(|p| [math::tick_array_start(p.0, ts), math::tick_array_start(p.1, ts)])
            .collect();
        starts.sort();
        starts.dedup();
        let tick_ixs: Vec<_> = starts.iter().map(|s| wp::init_tick_array_ix(e.target_pool, self.cranker, *s)).collect();
        out.push(Action::new("prep tick arrays", tick_ixs));
        out
    }

    /// Step two: the switch itself, one instruction list per transaction (pull, each hop, then
    /// reprice + add together). Call after the prep has landed: it reads the target pool.
    pub fn bundle_txs(&self, e: &Estimate) -> Vec<Vec<Instruction>> {
        self.switch_steps(e, false)
    }

    /// The switch as steps (pull, each hop, reprice + add). `compact` trims every swap to the one
    /// tick array holding the current price, so more steps fit in a single 64-account
    /// transaction.
    pub fn switch_steps(&self, e: &Estimate, compact: bool) -> Vec<Vec<Instruction>> {
        let c = self.config();
        let mut txs = vec![vec![self.pull_ix()]];
        for (holding, via) in &e.hops {
            txs.push(vec![self.hop_ix_arrays(*holding, *via, compact)]);
        }
        let pool = self.pool(&e.target_pool).expect("target pool missing");
        let ts = pool.tick_spacing as i32;
        let span = ts * math::TICK_ARRAY_SIZE;
        let start = math::tick_array_start(pool.tick_current, ts);
        // The pool sits at or near the estimate and the real target is a hair away in either
        // direction, so hand Orca the arrays on both sides.
        let arrays = if compact {
            [wp::tick_array_address(&e.target_pool, start); 3]
        } else {
            [start, start - span, start + span].map(|s| wp::tick_array_address(&e.target_pool, s))
        };
        let reprice = Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Reprice {
                config: config_pda(),
                authority: authority(),
                pool: self.pool_sides(&e.target_pool),
                tick_array_0: arrays[0],
                tick_array_1: arrays[1],
                tick_array_2: arrays[2],
                oracle: wp::oracle_address(&e.target_pool),
                memo_program: wp::MEMO_ID,
                whirlpool_program: wp::WHIRLPOOL_ID,
            }
            .to_account_metas(None),
            data: instruction::Reprice {}.data(),
        };
        let add = Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Add {
                funder: self.cranker,
                config: config_pda(),
                authority: authority(),
                positions: self.positions(&e.target_pool, e.positions.map(|p| Some((p.0, p.1)))),
                pool: self.pool_sides(&e.target_pool),
                escrow: escrow(),
                mint: c.mint,
                token_program: TOKEN_PROGRAM,
                token_2022_program: wp::TOKEN_2022_ID,
                system_program: system_program::ID,
                associated_token_program: wp::ATA_PROGRAM_ID,
                memo_program: wp::MEMO_ID,
                nft_update_auth: wp::NFT_UPDATE_AUTH,
                whirlpool_program: wp::WHIRLPOOL_ID,
            }
            .to_account_metas(None),
            data: instruction::Add {}.data(),
        };
        txs.push(vec![reprice, add]);
        txs
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

impl<'a, L: Ledger> Crank<'a, L> {
    /// Second half of a two-transaction switch, built once the pull and hops have landed (phase
    /// Repricing): tick arrays for the final positions (idempotent), the reprice if the pool is
    /// not at the target yet, and the add, all in one transaction. Positions are planned at the
    /// exact target price, which is where `add` will see the pool after the reprice.
    pub fn finish_ixs(&self) -> Option<Vec<Instruction>> {
        let c = self.config();
        if c.switch.phase != Phase::Repricing {
            return None;
        }
        let (pool_key, target_sqrt) = self.target_pool();
        let current = self.pool(&pool_key)?;
        let at_target = PoolState { sqrt_price: target_sqrt, tick_current: math::tick_at_sqrt_price(target_sqrt), ..current.clone() };
        let index_is_a = c.index_is_a(&c.switch.target);
        let ours = self.pool_sides(&pool_key);
        let (ours_index, ours_quote) = if index_is_a { (ours.ours_a, ours.ours_b) } else { (ours.ours_b, ours.ours_a) };
        let plan = plan_positions(index_is_a, &at_target, c.switch.target_floor_sqrt, self.balance(&ours_index), self.balance(&ours_quote));
        let ts = current.tick_spacing as i32;
        let mut starts: Vec<i32> = plan
            .iter()
            .filter(|p| p.2 > 0)
            .flat_map(|p| [math::tick_array_start(p.0, ts), math::tick_array_start(p.1, ts)])
            .collect();
        starts.sort();
        starts.dedup();
        let mut ixs: Vec<Instruction> = starts.iter().map(|s| wp::init_tick_array_ix(pool_key, self.cranker, *s)).collect();
        if current.sqrt_price != target_sqrt {
            ixs.push(self.reprice_ix());
        }
        ixs.push(self.add_ix_planned(&pool_key, plan));
        Some(ixs)
    }

    fn add_ix_planned(&self, pool_key: &Pubkey, plan: [(i32, i32, u128); 2]) -> Instruction {
        let c = self.config();
        Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Add {
                funder: self.cranker,
                config: config_pda(),
                authority: authority(),
                positions: self.positions(pool_key, plan.map(|p| Some((p.0, p.1)))),
                pool: self.pool_sides(pool_key),
                escrow: escrow(),
                mint: c.mint,
                token_program: TOKEN_PROGRAM,
                token_2022_program: wp::TOKEN_2022_ID,
                system_program: system_program::ID,
                associated_token_program: wp::ATA_PROGRAM_ID,
                memo_program: wp::MEMO_ID,
                nft_update_auth: wp::NFT_UPDATE_AUTH,
                whirlpool_program: wp::WHIRLPOOL_ID,
            }
            .to_account_metas(None),
            data: instruction::Add {}.data(),
        }
    }
}
