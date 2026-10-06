//! LiteSVM harness: our dev build, Metaplex, Orca Whirlpools, Meteora DAMM v2 and Raydium CLMM
//! (dumped from mainnet), Orca's mainnet WhirlpoolsConfig and fee tiers, Raydium's 1% AmmConfig
//! (for the migration test), and a small market (route pools on Orca; our own pools are DAMM v2):
//!
//!   USDC (6dp)  -- root hub
//!   WSOL (9dp)  -- hub, routed via a SOL/USDC pool ($150)
//!   X    (8dp)  -- "xStock", routed via X/USDC ($400)
//!   Y    (6dp)  -- "memecoin", routed via Y/WSOL ($0.001)
//!
//! Route pools get deep full-range liquidity from a test LP. `crank` drives a switch through
//! pull / hop / reprice / add the way a real client would.

use anchor_lang::{
    prelude::Pubkey,
    solana_program::{instruction::Instruction, program_pack::Pack, system_instruction, system_program},
    AccountDeserialize, InstructionData, ToAccountMetas,
};
use anchor_spl::{associated_token::spl_associated_token_account, token::spl_token, token_2022::spl_token_2022};
use base64::Engine;
use chamelequote::{
    accounts, instruction,
    instructions::InitializeParams,
    math,
    metaplex::{metadata_address, TOKEN_METADATA_ID},
    damm,
    state::{Config, QuoteEntry, AUTHORITY_SEED, CONFIG_SEED, ESCROW_SEED, QUOTE_SEED},
    whirlpool::{self as wp, PoolState, Sides},
    ID as PROGRAM_ID,
};
use chamelequote_crank::{Crank, Ledger};
use litesvm::LiteSVM;
use solana_account::Account;
use solana_clock::Clock;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;
use solana_transaction::Transaction;

pub use solana_keypair::Keypair as Kp;
pub use solana_signer::Signer as _;

pub const DECIMALS: u32 = 6;
pub const SUPPLY: u64 = 1_000_000_000 * 10u64.pow(DECIMALS);
pub const BURN: u64 = 1_000_000 * 10u64.pow(DECIMALS);
pub const TS_OURS: u16 = 120;
pub const TS_ROUTE: u16 = 64;
pub const FEE_SHARE_BPS: u16 = 1000;
pub const WP_CONFIG: Pubkey = anchor_lang::prelude::pubkey!("2LecshUwdy9xi7meFgHtFJQNSKk4KdTrcpvaB56dP2NQ");
/// Raydium CLMM's 1% fee tier (tick spacing 120).
pub const CLMM_CONFIG: Pubkey = anchor_lang::prelude::pubkey!("A1BBtTYJd4i3xU8D6Tc2FzU6ZN4oXZWXKZnCxwbHXr8x");
const COMPUTE_BUDGET: Pubkey = anchor_lang::prelude::pubkey!("ComputeBudget111111111111111111111111111111");

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
fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    wp::ata(owner, mint, &spl_token::ID)
}

/// sqrt(price of `x` in `y`, raw units = num/den) in the orientation of the pool between them.
pub fn pool_sqrt(x: &Pubkey, y: &Pubkey, num: u128, den: u128) -> u128 {
    let s = math::sqrt_ratio(num, den).unwrap(); // sqrt(y per x)
    if x.to_bytes() < y.to_bytes() {
        s
    } else {
        math::invert_sqrt(s)
    }
}

/// USD price of 1 raw unit, as a float, for assertions.
pub fn f(x: u128) -> f64 {
    let s = x as f64 / math::Q64 as f64;
    s * s
}

pub struct Env {
    pub svm: LiteSVM,
    pub admin: Keypair,
    pub lp: Keypair,
    pub fee_recipient: Keypair,
    pub mint: Pubkey,
    pub usdc: Pubkey,
    pub wsol: Pubkey,
    pub x: Pubkey,
    pub y: Pubkey,
    pub hub_pool: Pubkey,
    pub x_pool: Pubkey,
    pub y_pool: Pubkey,
    /// Quotes listed by a test on top of the standard four (poked like them).
    pub extra_quotes: Vec<Pubkey>,
    /// Compute units of the last successful transaction (for budget checks).
    pub last_cu: u64,
    /// Instruction-trace entries (top level + CPIs) of the last successful transaction, counted
    /// as on mainnet (with both compute-budget instructions).
    pub last_trace: usize,
    /// (step, compute units) of the last `crank`.
    pub crank_cu: Vec<(&'static str, u64)>,
    /// The program's instructions the last `crank` sent, by name, in order.
    pub crank_steps: Vec<&'static str>,
}

impl Env {
    // -----------------------------------------------------------------------------------------
    // Basics

    pub fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), String> {
        let mut all = vec![Instruction {
            program_id: COMPUTE_BUDGET,
            accounts: vec![],
            data: [vec![2u8], 1_400_000u32.to_le_bytes().to_vec()].concat(),
        }];
        all.extend_from_slice(ixs);
        let payer = signers[0].pubkey();
        let tx = Transaction::new(signers, Message::new(&all, Some(&payer)), self.svm.latest_blockhash());
        let res = self
            .svm
            .send_transaction(tx)
            .map(|m| (m.compute_units_consumed, m.inner_instructions.iter().map(|v| v.len()).sum::<usize>()))
            .map_err(|e| format!("{:?}\n{}", e.err, e.meta.logs.join("\n")));
        self.svm.expire_blockhash();
        if let Ok((cu, inner)) = res {
            self.last_cu = cu;
            // Mainnet transactions carry a second compute-budget instruction (the priority fee).
            self.last_trace = all.len() + 1 + inner;
        }
        res.map(|_| ())
    }

    pub fn funded(&mut self) -> Keypair {
        let k = Keypair::new();
        self.svm.airdrop(&k.pubkey(), 100_000_000_000).unwrap();
        k
    }

    pub fn now(&self) -> i64 {
        self.svm.get_sysvar::<Clock>().unix_timestamp
    }

    pub fn warp(&mut self, secs: i64) {
        let mut c = self.svm.get_sysvar::<Clock>();
        c.unix_timestamp += secs;
        c.slot += (secs as u64) * 5 / 2;
        self.svm.set_sysvar(&c);
        self.svm.expire_blockhash();
    }

    pub fn balance(&self, account: &Pubkey) -> u64 {
        self.svm.get_account(account).map(|a| u64::from_le_bytes(a.data[64..72].try_into().unwrap())).unwrap_or(0)
    }

    pub fn supply(&self) -> u64 {
        spl_token::state::Mint::unpack(&self.svm.get_account(&self.mint).unwrap().data).unwrap().supply
    }

    pub fn config(&self) -> Config {
        Config::try_deserialize(&mut &self.svm.get_account(&config_pda()).unwrap().data[..]).unwrap()
    }

    pub fn quote(&self, mint: &Pubkey) -> QuoteEntry {
        QuoteEntry::try_deserialize(&mut &self.svm.get_account(&quote_pda(mint)).unwrap().data[..]).unwrap()
    }

    /// An Orca pool (route pools).
    pub fn pool(&self, pool: &Pubkey) -> PoolState {
        wp::parse_pool(&self.svm.get_account(pool).unwrap().data).unwrap()
    }

    /// One of our DAMM v2 pools.
    pub fn our_pool(&self, pool: &Pubkey) -> damm::PoolState {
        damm::parse_pool(&self.svm.get_account(pool).unwrap().data).unwrap()
    }

    pub fn create_mint(&mut self, decimals: u8) -> Pubkey {
        let mint = Keypair::new();
        let admin = self.admin.insecure_clone();
        let rent = self.svm.minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN);
        let ixs = [
            system_instruction::create_account(&admin.pubkey(), &mint.pubkey(), rent, spl_token::state::Mint::LEN as u64, &spl_token::ID),
            spl_token::instruction::initialize_mint2(&spl_token::ID, &mint.pubkey(), &admin.pubkey(), None, decimals).unwrap(),
        ];
        self.send(&ixs, &[&admin, &mint]).unwrap();
        mint.pubkey()
    }

    /// The token program that owns `mint`.
    pub fn program_of(&self, mint: &Pubkey) -> Pubkey {
        self.svm.get_account(mint).map(|a| a.owner).unwrap_or(spl_token::ID)
    }

    /// `owner`'s associated token account for `mint`, under whichever program owns the mint.
    pub fn ata_of(&self, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
        wp::ata(owner, mint, &self.program_of(mint))
    }

    pub fn ensure_ata(&mut self, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
        let admin = self.admin.insecure_clone();
        let program = self.program_of(mint);
        let ix = spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            &admin.pubkey(),
            owner,
            mint,
            &program,
        );
        self.send(&[ix], &[&admin]).unwrap();
        wp::ata(owner, mint, &program)
    }

    /// A token-2022 mint set up like an xStock: a permanent delegate and a transfer-hook
    /// authority (no hook program), which makes Orca and Meteora want a token badge for it. Both
    /// badges are written straight into the ledger (only their admins can create them).
    pub fn create_xstock_mint(&mut self, decimals: u8) -> Pubkey {
        use spl_token_2022::extension::ExtensionType;
        let mint = Keypair::new();
        let admin = self.admin.insecure_clone();
        let t22 = spl_token_2022::ID;
        let len = ExtensionType::try_calculate_account_len::<spl_token_2022::state::Mint>(&[
            ExtensionType::PermanentDelegate,
            ExtensionType::TransferHook,
        ])
        .unwrap();
        let rent = self.svm.minimum_balance_for_rent_exemption(len);
        let ixs = [
            system_instruction::create_account(&admin.pubkey(), &mint.pubkey(), rent, len as u64, &t22),
            spl_token_2022::instruction::initialize_permanent_delegate(&t22, &mint.pubkey(), &admin.pubkey()).unwrap(),
            spl_token_2022::extension::transfer_hook::instruction::initialize(&t22, &mint.pubkey(), Some(admin.pubkey()), None).unwrap(),
            spl_token_2022::instruction::initialize_mint2(&t22, &mint.pubkey(), &admin.pubkey(), None, decimals).unwrap(),
        ];
        self.send(&ixs, &[&admin, &mint]).unwrap();
        let m = mint.pubkey();
        const BADGE_DISC: [u8; 8] = [116, 219, 204, 229, 249, 116, 255, 150];
        let orca_badge = wp::token_badge_address(&WP_CONFIG, &m);
        let mut data = BADGE_DISC.to_vec();
        data.extend(WP_CONFIG.to_bytes());
        data.extend(m.to_bytes());
        data.extend([0u8; 128]);
        self.svm
            .set_account(orca_badge, Account { lamports: 2_282_880, data, owner: wp::WHIRLPOOL_ID, executable: false, rent_epoch: 0 })
            .unwrap();
        let mut data = BADGE_DISC.to_vec();
        data.extend(m.to_bytes());
        data.extend([0u8; 128]);
        self.svm
            .set_account(damm::token_badge_address(&m), Account { lamports: 2_060_160, data, owner: damm::DAMM_ID, executable: false, rent_epoch: 0 })
            .unwrap();
        m
    }

    /// Mints test tokens (any mint but ours, whose authority is revoked).
    pub fn mint_to(&mut self, owner: &Pubkey, mint: &Pubkey, amount: u64) -> Pubkey {
        let to = self.ensure_ata(owner, mint);
        let admin = self.admin.insecure_clone();
        let ix = spl_token_2022::instruction::mint_to(&self.program_of(mint), mint, &to, &admin.pubkey(), &[], amount).unwrap();
        self.send(&[ix], &[&admin]).unwrap();
        to
    }

    /// Hands out our token from the supply vault (dev-only instruction).
    pub fn give(&mut self, owner: &Pubkey, amount: u64) -> Pubkey {
        let to = self.ensure_ata(owner, &self.mint.clone());
        let admin = self.admin.insecure_clone();
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::DevTransfer {
                admin: admin.pubkey(),
                config: config_pda(),
                mint: self.mint,
                authority: authority(),
                supply_vault: ata(&authority(), &self.mint),
                to,
                token_program: spl_token::ID,
            }
            .to_account_metas(None),
            data: instruction::DevTransfer { amount }.data(),
        };
        self.send(&[ix], &[&admin]).unwrap();
        to
    }

    // -----------------------------------------------------------------------------------------
    // Orca helpers (as an outside user would call them)

    pub fn sides_for(&self, pool: &Pubkey, owner: &Pubkey) -> Sides {
        let p = self.pool(pool);
        Sides {
            mint_a: p.mint_a,
            mint_b: p.mint_b,
            program_a: self.program_of(&p.mint_a),
            program_b: self.program_of(&p.mint_b),
            owner_a: self.ata_of(owner, &p.mint_a),
            owner_b: self.ata_of(owner, &p.mint_b),
            vault_a: p.vault_a,
            vault_b: p.vault_b,
        }
    }

    /// Creates the pool between two mints at `sqrt` (pool orientation). Returns its address.
    pub fn init_pool(&mut self, m1: &Pubkey, m2: &Pubkey, tick_spacing: u16, sqrt: u128) -> Pubkey {
        let (a, b) = if m1.to_bytes() < m2.to_bytes() { (*m1, *m2) } else { (*m2, *m1) };
        let (va, vb) = (Keypair::new(), Keypair::new());
        let admin = self.admin.insecure_clone();
        let (pa, pb) = (self.program_of(&a), self.program_of(&b));
        let ix = wp::initialize_pool_ix(WP_CONFIG, a, b, pa, pb, admin.pubkey(), va.pubkey(), vb.pubkey(), tick_spacing, sqrt);
        self.send(&[ix], &[&admin, &va, &vb]).unwrap();
        wp::whirlpool_address(&WP_CONFIG, &a, &b, tick_spacing)
    }

    pub fn init_tick_arrays(&mut self, pool: &Pubkey, starts: &[i32]) {
        let admin = self.admin.insecure_clone();
        let ixs: Vec<_> = starts.iter().map(|s| wp::init_tick_array_ix(*pool, admin.pubkey(), *s)).collect();
        self.send(&ixs, &[&admin]).unwrap();
    }

    /// Full-range liquidity from the test LP, sized from `amount_a` / `amount_b`.
    pub fn add_lp(&mut self, pool: &Pubkey, amount_a: u64, amount_b: u64) {
        let p = self.pool(pool);
        let ts = p.tick_spacing as i32;
        let max_t = math::max_usable_tick(ts);
        self.init_tick_arrays(pool, &[math::tick_array_start(-max_t, ts), math::tick_array_start(max_t, ts)]);
        let lp = self.lp.insecure_clone();
        self.mint_to(&lp.pubkey(), &p.mint_a, amount_a);
        self.mint_to(&lp.pubkey(), &p.mint_b, amount_b);
        let (lo, hi) = (math::sqrt_price_at_tick(-max_t), math::sqrt_price_at_tick(max_t));
        let l = math::liquidity_for_a(amount_a, p.sqrt_price, hi).min(math::liquidity_for_b(amount_b, lo, p.sqrt_price));
        let pos_mint = Keypair::new();
        let sides = self.sides_for(pool, &lp.pubkey());
        let ixs = [
            wp::open_position_ix(lp.pubkey(), lp.pubkey(), pos_mint.pubkey(), *pool, -max_t, max_t),
            wp::modify_liquidity_ix(
                true,
                *pool,
                lp.pubkey(),
                pos_mint.pubkey(),
                &sides,
                wp::tick_array_address(pool, math::tick_array_start(-max_t, ts)),
                wp::tick_array_address(pool, math::tick_array_start(max_t, ts)),
                l * 99 / 100,
                amount_a,
                amount_b,
            ),
        ];
        self.send(&ixs, &[&lp, &pos_mint]).unwrap();
    }

    pub fn swap_arrays(&self, pool: &Pubkey, a_to_b: bool) -> [Pubkey; 3] {
        let p = self.pool(pool);
        let span = p.tick_spacing as i32 * math::TICK_ARRAY_SIZE;
        let start = math::tick_array_start(p.tick_current, p.tick_spacing as i32);
        let step = if a_to_b { -span } else { span };
        [0, 1, 2].map(|k| wp::tick_array_address(pool, start + k * step))
    }

    /// A plain user trade through a pool (buying, selling, or pushing a price around).
    pub fn user_swap(&mut self, user: &Keypair, pool: &Pubkey, a_to_b: bool, amount: u64) -> Result<(), String> {
        let p = self.pool(pool);
        self.ensure_ata(&user.pubkey(), &p.mint_a);
        self.ensure_ata(&user.pubkey(), &p.mint_b);
        let sides = self.sides_for(pool, &user.pubkey());
        let ix = wp::swap_ix(*pool, user.pubkey(), &sides, self.swap_arrays(pool, a_to_b), amount, 0, 0, a_to_b);
        self.send(&[ix], &[user])
    }

    /// A plain user trade through one of our DAMM v2 pools (`a_to_b`: selling our token).
    pub fn our_swap(&mut self, user: &Keypair, pool: &Pubkey, a_to_b: bool, amount: u64) -> Result<(), String> {
        let p = self.our_pool(pool);
        self.ensure_ata(&user.pubkey(), &p.mint_a);
        self.ensure_ata(&user.pubkey(), &p.mint_b);
        let sides = damm::Sides {
            pool: *pool,
            mint_a: p.mint_a,
            mint_b: p.mint_b,
            program_a: self.program_of(&p.mint_a),
            program_b: self.program_of(&p.mint_b),
            ours_a: self.ata_of(&user.pubkey(), &p.mint_a),
            ours_b: self.ata_of(&user.pubkey(), &p.mint_b),
            vault_a: p.vault_a,
            vault_b: p.vault_b,
        };
        self.send(&[damm::swap_ix(user.pubkey(), &sides, a_to_b, amount, 0)], &[user])
    }

    /// Buys our token with `amount` of the active quote.
    pub fn buy(&mut self, user: &Keypair, amount: u64) -> Result<(), String> {
        let c = self.config();
        self.mint_to(&user.pubkey(), &c.active_quote, amount);
        self.our_swap(user, &c.active_pool, false, amount)
    }

    /// Sells `amount` of our token into the active pool.
    pub fn sell(&mut self, user: &Keypair, amount: u64) -> Result<(), String> {
        let c = self.config();
        self.our_swap(user, &c.active_pool, true, amount)
    }

    // -----------------------------------------------------------------------------------------
    // Setup

    pub fn new() -> Env {
        Self::new_ordered(None)
    }

    /// `index_first`: Some(true) makes our mint sort before every quote mint (our token is
    /// token A in all its pools), Some(false) after (token B); None leaves it to chance.
    pub fn new_ordered(index_first: Option<bool>) -> Env {
        Self::new_with("../target/deploy-dev/chamelequote.so", index_first, (CLMM_CONFIG, TS_OURS))
    }

    /// `program`: the build to load; `venue`: the pool config and tick spacing to initialize
    /// with (Raydium's for this build; Orca's for the build before the move).
    pub fn new_with(program: &str, index_first: Option<bool>, venue: (Pubkey, u16)) -> Env {
        let mut svm = LiteSVM::new();
        svm.add_program_from_file(PROGRAM_ID, program).unwrap();
        svm.add_program_from_file(TOKEN_METADATA_ID, "fixtures/mpl_token_metadata.so").unwrap();
        svm.add_program_from_file(wp::WHIRLPOOL_ID, "fixtures/whirlpool.so").unwrap();
        svm.add_program_from_file(damm::DAMM_ID, "fixtures/damm_v2.so").unwrap();
        for (file, owner) in [("fixtures/whirlpool_accounts.json", wp::WHIRLPOOL_ID)] {
            let fixtures: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
            for (_, v) in fixtures.as_object().unwrap() {
                let address: Pubkey = v["address"].as_str().unwrap().parse().unwrap();
                let data = base64::engine::general_purpose::STANDARD.decode(v["data"].as_str().unwrap()).unwrap();
                svm.set_account(
                    address,
                    Account { lamports: v["lamports"].as_u64().unwrap(), data, owner, executable: false, rent_epoch: 0 },
                )
                .unwrap();
            }
        }
        let mut c = svm.get_sysvar::<Clock>();
        c.unix_timestamp = 1_790_000_000;
        svm.set_sysvar(&c);

        let admin = Keypair::new();
        svm.airdrop(&admin.pubkey(), 1_000_000_000_000).unwrap();
        let mut env = Env {
            svm,
            admin,
            lp: Keypair::new(),
            fee_recipient: Keypair::new(),
            mint: Pubkey::default(),
            usdc: Pubkey::default(),
            wsol: Pubkey::default(),
            x: Pubkey::default(),
            y: Pubkey::default(),
            hub_pool: Pubkey::default(),
            x_pool: Pubkey::default(),
            y_pool: Pubkey::default(),
            extra_quotes: vec![],
            last_cu: 0,
            last_trace: 0,
            crank_cu: vec![],
            crank_steps: vec![],
        };
        let lp = env.lp.pubkey();
        env.svm.airdrop(&lp, 100_000_000_000).unwrap();

        env.usdc = env.create_mint(6);
        env.wsol = env.create_mint(9);
        env.x = env.create_mint(8);
        env.y = env.create_mint(6);

        // SOL $150: USDC per raw WSOL = 150e6 / 1e9
        let (usdc, wsol, x, y) = (env.usdc, env.wsol, env.x, env.y);
        env.hub_pool = env.init_pool(&wsol, &usdc, TS_ROUTE, pool_sqrt(&wsol, &usdc, 150, 1000));
        // X $400 at 8dp: USDC per raw X = 400e6 / 1e8 = 4
        env.x_pool = env.init_pool(&x, &usdc, TS_ROUTE, pool_sqrt(&x, &usdc, 4, 1));
        // Y $0.001 at 6dp, SOL $150: WSOL per raw Y = (0.001/150) * 1e9/1e6 = 1/150
        env.y_pool = env.init_pool(&y, &wsol, TS_ROUTE, pool_sqrt(&y, &wsol, 1, 150));

        // ~$20M per side in each route pool.
        let deep = |m: &Pubkey, env: &Env| -> u64 {
            if *m == env.usdc {
                20_000_000 * 1_000_000
            } else if *m == env.wsol {
                133_333 * 1_000_000_000
            } else if *m == env.x {
                50_000 * 100_000_000
            } else {
                20_000_000_000 * 1_000_000
            }
        };
        for pool in [env.hub_pool, env.x_pool, env.y_pool] {
            let p = env.pool(&pool);
            let (a, b) = (deep(&p.mint_a, &env), deep(&p.mint_b, &env));
            env.add_lp(&pool, a, b);
        }

        env.initialize(index_first, venue);
        env.list_quote(usdc, None);
        env.list_quote(wsol, Some((usdc, env.hub_pool)));
        env.list_quote(x, Some((usdc, env.x_pool)));
        env.list_quote(y, Some((wsol, env.y_pool)));
        env
    }

    fn initialize(&mut self, index_first: Option<bool>, venue: (Pubkey, u16)) {
        let quotes = [self.usdc, self.wsol, self.x, self.y];
        let mint = loop {
            let k = Keypair::new();
            let ok = match index_first {
                None => true,
                Some(first) => quotes.iter().all(|q| (k.pubkey().to_bytes() < q.to_bytes()) == first),
            };
            if ok {
                break k;
            }
        };
        self.mint = mint.pubkey();
        let admin = self.admin.insecure_clone();
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Initialize {
                admin: admin.pubkey(),
                config: config_pda(),
                authority: authority(),
                mint: mint.pubkey(),
                supply_vault: ata(&authority(), &mint.pubkey()),
                escrow: escrow(),
                metadata: metadata_address(&mint.pubkey()),
                token_metadata_program: TOKEN_METADATA_ID,
                token_program: spl_token::ID,
                associated_token_program: wp::ATA_PROGRAM_ID,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
            data: instruction::Initialize {
                params: InitializeParams {
                    name: "Chameleon".into(),
                    symbol: "CHAM".into(),
                    uri: "https://example.com/cham.json".into(),
                    supply: SUPPLY,
                    burn_amount: BURN,
                    clmm_config: venue.0,
                    tick_spacing: venue.1,
                    usdc: self.usdc,
                    wsol: self.wsol,
                    fee_recipient: self.fee_recipient.pubkey(),
                    fee_share_bps: FEE_SHARE_BPS,
                },
            }
            .data(),
        };
        self.send(&[ix], &[&admin, &mint]).unwrap();
    }

    pub fn list_quote_ix(&self, admin: &Pubkey, mint: Pubkey, hub_route: Option<(Pubkey, Pubkey)>) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::ListQuote {
                admin: *admin,
                config: config_pda(),
                quote_mint: mint,
                quote: quote_pda(&mint),
                hub_entry: hub_route.map(|(h, _)| quote_pda(&h)),
                route_pool: hub_route.map(|(_, r)| r),
                system_program: system_program::ID,
            }
            .to_account_metas(None),
            data: instruction::ListQuote {}.data(),
        }
    }

    pub fn list_quote(&mut self, mint: Pubkey, hub_route: Option<(Pubkey, Pubkey)>) {
        let admin = self.admin.insecure_clone();
        let ix = self.list_quote_ix(&admin.pubkey(), mint, hub_route);
        self.send(&[ix], &[&admin]).unwrap();
    }

    // -----------------------------------------------------------------------------------------
    // Keeper

    pub fn poke_quotes(&mut self) {
        self.poke_all();
    }

    pub fn poke_pool(&mut self) {
        let c = self.config();
        if c.active_pool == Pubkey::default() {
            return;
        }
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::PokePool { config: config_pda(), pool: c.active_pool }.to_account_metas(None),
            data: instruction::PokePool {}.data(),
        };
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin]).unwrap();
    }

    /// Pokes everything every minute for `secs`.
    pub fn keep(&mut self, secs: i64) {
        let mut t = 0;
        while t < secs {
            self.warp(60);
            self.poke_quotes();
            self.poke_pool();
            t += 60;
        }
    }

    // -----------------------------------------------------------------------------------------
    // Program instructions

    pub fn launch(&mut self, quote: Pubkey, index_sqrt: u128) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::Launch { admin: admin.pubkey(), config: config_pda(), quote: quote_pda(&quote) }
                .to_account_metas(None),
            data: instruction::Launch { index_sqrt }.data(),
        };
        self.send(&[ix], &[&admin])?;
        // DAMM moves at least one unit of each token into a new pool, and at launch there is no
        // backing yet: one raw unit of the quote stands in.
        self.mint_to(&authority(), &quote, 1);
        Ok(())
    }

    pub fn request_ix(&self, user: &Pubkey, user_token: Pubkey, target: Pubkey) -> Instruction {
        let mut ix = chamelequote_crank::request_switch_ix(user, &self.config(), &target);
        ix.accounts[3].pubkey = user_token;
        ix
    }

    pub fn request(&mut self, user: &Keypair, target: Pubkey) -> Result<(), String> {
        let ix = self.request_ix(&user.pubkey(), ata(&user.pubkey(), &self.mint), target);
        self.send(&[ix], &[user])
    }

    // Switch steps go through the keeper's own logic (chamelequote-crank), so the tests check
    // exactly what the keeper sends.

    pub fn crank_as(&self, cranker: &Pubkey) -> Crank<'_, Env> {
        Crank::new(self, *cranker)
    }

    pub fn pull_ix(&mut self, cranker: &Pubkey) -> Instruction {
        let c = self.config();
        let fee = self.fee_recipient.pubkey();
        for m in [c.mint, c.active_quote] {
            self.ensure_ata(&fee, &m);
        }
        self.crank_as(cranker).pull_ix()
    }

    /// The whole switch as the keeper sends it (one transaction), creating the token accounts it
    /// needs first.
    pub fn switch_ixs(&mut self, cranker: &Kp) -> Vec<Instruction> {
        let est = self.crank_as(&cranker.pubkey()).estimate().expect("a requested switch");
        let prep = self.crank_as(&cranker.pubkey()).missing_accounts(&est);
        if !prep.is_empty() {
            self.send(&prep, &[cranker]).unwrap();
        }
        self.crank_as(&cranker.pubkey()).switch_tx(&est, false).unwrap()
    }

    pub fn next_via(&self) -> Pubkey {
        self.crank_as(&Pubkey::default()).next_via()
    }

    pub fn hop_ix(&mut self, via: Pubkey) -> Instruction {
        let c = self.config();
        let route = self.quote(&via).route_pool;
        let p = self.pool(&route);
        self.ensure_ata(&authority(), &p.mint_a);
        self.ensure_ata(&authority(), &p.mint_b);
        let _ = c;
        self.crank_as(&Pubkey::default()).hop_ix(via)
    }

    /// The abort instruction alone (its token accounts are created first).
    pub fn abort_ix(&mut self) -> Instruction {
        let admin = self.admin.pubkey();
        let mut ixs = self.crank_as(&admin).abort_ixs();
        let abort = ixs.pop().unwrap();
        let a = self.admin.insecure_clone();
        self.send(&ixs, &[&a]).unwrap();
        abort
    }

    /// Drives the in-flight switch (or launch) to completion with the keeper's logic. Returns the
    /// number of hops taken; `crank_cu` records (step, compute units).
    pub fn crank(&mut self) -> Result<usize, String> {
        self.crank_cu.clear();
        self.crank_steps.clear();
        let cranker = self.funded();
        let mut hops = 0;
        for _ in 0..30 {
            let Some(action) = self.crank_as(&cranker.pubkey()).next() else { return Ok(hops) };
            let mut signers: Vec<&Keypair> = vec![&cranker];
            signers.extend(action.signers.iter());
            self.send(&action.ixs, &signers).map_err(|e| format!("{}: {e}", action.label))?;
            self.crank_cu.push((action.label, self.last_cu));
            eprintln!("  crank {}: {} CU, {} trace, {} accounts", action.label, self.last_cu, self.last_trace, chamelequote_crank::unique_accounts(&action.ixs, &cranker.pubkey()));
            for ix in action.ixs.iter().filter(|ix| ix.program_id == PROGRAM_ID) {
                use anchor_lang::Discriminator;
                let names: [(&[u8], &'static str); 6] = [
                    (instruction::Pull::DISCRIMINATOR, "pull"),
                    (instruction::Hop::DISCRIMINATOR, "hop"),
                    (instruction::Seed::DISCRIMINATOR, "seed"),
                    (instruction::Reprice::DISCRIMINATOR, "reprice"),
                    (instruction::Add::DISCRIMINATOR, "add"),
                    (instruction::Abort::DISCRIMINATOR, "abort"),
                ];
                if let Some((_, name)) = names.iter().find(|(d, _)| ix.data.starts_with(d)) {
                    self.crank_steps.push(name);
                    if *name == "hop" {
                        hops += 1;
                    }
                }
            }
        }
        Err("crank did not finish".into())
    }

    /// Keeper pokes for every listed quote.
    pub fn poke_all(&mut self) {
        let mut mints = vec![self.usdc, self.wsol, self.x, self.y];
        mints.extend(self.extra_quotes.iter().copied());
        let entries: Vec<QuoteEntry> = mints.iter().map(|m| self.quote(m)).collect();
        let admin = self.admin.insecure_clone();
        let actions = self.crank_as(&admin.pubkey()).poke_quotes(&entries);
        for a in actions {
            self.send(&a.ixs, &[&admin]).unwrap();
        }
    }

    /// Value of one raw index unit in raw USDC, from the live pools (spot).
    pub fn index_usd(&self) -> f64 {
        let c = self.config();
        let sqrt = self.crank_as(&Pubkey::default()).our_sqrt(&c.active_pool).unwrap();
        let index_per = f(sqrt); // quote per index
        index_per * self.quote_usd(&c.active_quote)
    }

    /// Raw USDC per raw unit of `mint`, from route pool spot prices.
    pub fn quote_usd(&self, mint: &Pubkey) -> f64 {
        if *mint == self.usdc {
            return 1.0;
        }
        let q = self.quote(mint);
        let p = self.pool(&q.route_pool);
        let hub_per = f(math::flip(p.sqrt_price, p.mint_a != *mint));
        hub_per * self.quote_usd(&q.hub)
    }

    /// Launch at $0.0001 (FDV $100k) against USDC and warm the averages.
    pub fn launched() -> Env {
        Self::launched_ordered(None)
    }

    pub fn launched_ordered(index_first: Option<bool>) -> Env {
        let mut env = Env::new_ordered(index_first);
        env.keep(660);
        let usdc = env.usdc;
        env.launch(usdc, math::sqrt_ratio(1, 10_000).unwrap()).unwrap();
        env.crank().unwrap();
        env
    }
}


impl Ledger for Env {
    fn account(&self, key: &Pubkey) -> Option<(Pubkey, Vec<u8>)> {
        self.svm.get_account(key).filter(|a| a.lamports > 0).map(|a| (a.owner, a.data))
    }
    fn now(&self) -> i64 {
        Env::now(self)
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}
