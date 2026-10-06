//! chamelequote keeper.
//!
//! Every ~50 s it pokes the price averages of every listed quote and of the coin's own pool (a
//! switch refuses to trade on an average older than two minutes). Every few seconds it checks
//! for an in-flight switch and pushes it through pull / hop / reprice / add, cancelling it if it
//! is past its deadline. All decisions come from `chamelequote-crank`, the same code the LiteSVM
//! tests drive.
//!
//!   keeper run    --rpc <url> --keypair <path> [--priority-fee <micro-lamports per CU>]
//!   keeper once   ...          one poke round + crank until idle, then exit
//!   keeper status --rpc <url>  print the switch state and how fresh the averages are

use std::{
    thread::sleep,
    time::{Duration, Instant},
};

use anchor_lang::{
    prelude::Pubkey,
    solana_program::instruction::{AccountMeta, Instruction},
    AccountDeserialize,
};
use chamelequote::{state::QuoteEntry, ID as PROGRAM_ID};
use chamelequote_crank::{config_pda, Action, Crank, Ledger, QUOTE_ENTRY_DISCRIMINATOR};
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_commitment_config::CommitmentConfig;
use solana_keypair::{read_keypair_file, Keypair};
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::{
    config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    filter::{Memcmp, RpcFilterType},
};
use solana_signer::Signer;
use solana_transaction::Transaction;

const COMPUTE_BUDGET: Pubkey = anchor_lang::prelude::pubkey!("ComputeBudget111111111111111111111111111111");
const CLOCK_SYSVAR: Pubkey = anchor_lang::prelude::pubkey!("SysvarC1ock11111111111111111111111111111111");
const POKE_EVERY: Duration = Duration::from_secs(50);
const RELIST_EVERY: Duration = Duration::from_secs(600);
const TICK: Duration = Duration::from_secs(4);

struct Rpc(RpcClient);

impl Ledger for Rpc {
    fn account(&self, key: &Pubkey) -> Option<(Pubkey, Vec<u8>)> {
        self.0
            .get_account_with_commitment(key, CommitmentConfig::confirmed())
            .ok()?
            .value
            .map(|a| (a.owner, a.data))
    }

    fn now(&self) -> i64 {
        // Clock sysvar: slot, epoch_start_timestamp, epoch, leader_schedule_epoch, unix_timestamp
        self.account(&CLOCK_SYSVAR)
            .map(|(_, d)| i64::from_le_bytes(d[32..40].try_into().unwrap()))
            .unwrap_or(0)
    }
}

impl Rpc {
    fn quote_entries(&self) -> Vec<QuoteEntry> {
        let cfg = RpcProgramAccountsConfig {
            filters: Some(vec![RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, QUOTE_ENTRY_DISCRIMINATOR.to_vec()))]),
            account_config: RpcAccountInfoConfig { encoding: Some(UiAccountEncoding::Base64), ..Default::default() },
            ..Default::default()
        };
        match self.0.get_program_ui_accounts_with_config(&PROGRAM_ID, cfg) {
            Ok(list) => list
                .into_iter()
                .filter_map(|(_, a)| a.data.decode())
                .filter_map(|d| QuoteEntry::try_deserialize(&mut &d[..]).ok())
                .collect(),
            Err(e) => {
                log(&format!("could not list quotes: {e}"));
                vec![]
            }
        }
    }
}

struct Opts {
    rpc: String,
    keypair: Option<String>,
    priority_fee: u64,
}

fn log(msg: &str) {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    println!("[{:02}:{:02}:{:02}Z] {msg}", (t / 3600) % 24, (t / 60) % 60, t % 60);
}

fn send(rpc: &Rpc, payer: &Keypair, action: &Action, priority_fee: u64) -> Result<String, String> {
    let mut ixs = vec![Instruction {
        program_id: COMPUTE_BUDGET,
        accounts: Vec::<AccountMeta>::new(),
        data: [vec![2u8], 1_400_000u32.to_le_bytes().to_vec()].concat(),
    }];
    if priority_fee > 0 {
        ixs.push(Instruction {
            program_id: COMPUTE_BUDGET,
            accounts: vec![],
            data: [vec![3u8], priority_fee.to_le_bytes().to_vec()].concat(),
        });
    }
    ixs.extend(action.ixs.iter().cloned());
    let blockhash = rpc.0.get_latest_blockhash().map_err(|e| e.to_string())?;
    let mut signers: Vec<&Keypair> = vec![payer];
    signers.extend(action.signers.iter());
    let tx = Transaction::new_signed_with_payer(&ixs, Some(&payer.pubkey()), &signers, blockhash);
    rpc.0.send_and_confirm_transaction(&tx).map(|s| s.to_string()).map_err(|e| e.to_string())
}

/// Pokes every listed quote and the coin's pool.
fn poke_round(rpc: &Rpc, payer: &Keypair, quotes: &[QuoteEntry], fee: u64) {
    let crank = Crank::new(rpc, payer.pubkey());
    let mut actions = crank.poke_quotes(quotes);
    actions.extend(crank.poke_pool());
    let (mut ok, mut failed) = (0, 0);
    for a in &actions {
        match send(rpc, payer, a, fee) {
            Ok(_) => ok += 1,
            Err(e) => {
                failed += 1;
                log(&format!("{} failed: {}", a.label, first_line(&e)));
            }
        }
    }
    if failed > 0 || ok > 0 {
        log(&format!("poked {} quotes + pool ({ok} tx ok, {failed} failed)", quotes.iter().filter(|q| !q.is_root()).count()));
    }
}

/// Pushes an in-flight switch forward until idle or a step fails. Returns steps sent.
fn crank_round(rpc: &Rpc, payer: &Keypair, fee: u64) -> usize {
    let mut sent = 0;
    for _ in 0..24 {
        let next = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Crank::new(rpc, payer.pubkey()).next()));
        let action = match next {
            Ok(Some(a)) => a,
            Ok(None) => return sent,
            Err(_) => {
                log("could not read the switch state (missing account?); retrying next tick");
                return sent;
            }
        };
        match send(rpc, payer, &action, fee) {
            Ok(sig) => {
                sent += 1;
                log(&format!("{}: {sig}", action.label));
            }
            Err(e) => {
                log(&format!("{} failed, will retry: {}", action.label, first_line(&e)));
                return sent;
            }
        }
    }
    sent
}

fn first_line(e: &str) -> String {
    // Program errors carry their name in the logs; surface it instead of the whole dump.
    if let Some(i) = e.find("Error Code: ") {
        return e[i..].split('.').next().unwrap_or(e).to_string();
    }
    e.lines().next().unwrap_or(e).chars().take(200).collect()
}

fn status(rpc: &Rpc) {
    let crank = Crank::new(rpc, Pubkey::default());
    if rpc.account(&config_pda()).is_none() {
        println!("No config account at {}: the program is not initialized on this cluster.", config_pda());
        return;
    }
    let c = crank.config();
    let now = rpc.now();
    println!("mint          {}", c.mint);
    println!("active quote  {}", c.active_quote);
    println!("active pool   {}", c.active_pool);
    println!("switch phase  {:?} (target {}, holding {}, deadline in {}s)", c.switch.phase, c.switch.target, c.switch.holding, c.switch.deadline - now);
    println!("pool average  last poke {}s ago, warm: {}", now - c.pool_ema.last_ts, c.pool_ema.is_valid(now));
    for q in rpc.quote_entries() {
        let age = if q.is_root() { "root".into() } else { format!("{}s ago, warm: {}", now - q.ema.last_ts, q.ema.is_valid(now)) };
        println!("quote {} enabled={} poke {}", q.mint, q.enabled, age);
    }
}

fn parse() -> (String, Opts) {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "help".into());
    let mut o = Opts { rpc: std::env::var("RPC_URL").unwrap_or_else(|_| "https://api.devnet.solana.com".into()), keypair: std::env::var("KEEPER_KEYPAIR").ok(), priority_fee: 0 };
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--rpc" => o.rpc = val(),
            "--keypair" => o.keypair = Some(val()),
            "--priority-fee" => o.priority_fee = val().parse().expect("--priority-fee takes a number"),
            other => panic!("unknown option {other}"),
        }
    }
    (cmd, o)
}

fn main() {
    let (cmd, o) = parse();
    let rpc = Rpc(RpcClient::new_with_commitment(o.rpc.clone(), CommitmentConfig::confirmed()));
    match cmd.as_str() {
        "status" => return status(&rpc),
        "run" | "once" => {}
        _ => {
            println!("usage: keeper run|once|status --rpc <url> --keypair <path> [--priority-fee <micro-lamports>]");
            return;
        }
    }
    let path = o.keypair.expect("--keypair (or KEEPER_KEYPAIR) is required");
    let payer = read_keypair_file(&path).unwrap_or_else(|e| panic!("could not read keypair {path}: {e}"));
    log(&format!("keeper {} on {} (program {PROGRAM_ID})", payer.pubkey(), o.rpc));
    match rpc.0.get_balance(&payer.pubkey()) {
        Ok(b) => log(&format!("balance {:.4} SOL", b as f64 / 1e9)),
        Err(e) => log(&format!("could not read balance: {e}")),
    }
    if rpc.account(&config_pda()).is_none() {
        log("program is not initialized on this cluster yet; nothing to do");
        return;
    }

    let mut quotes = rpc.quote_entries();
    let (mut last_poke, mut last_list) = (None::<Instant>, Instant::now());
    loop {
        if last_list.elapsed() >= RELIST_EVERY {
            quotes = rpc.quote_entries();
            last_list = Instant::now();
        }
        if last_poke.is_none_or(|t| t.elapsed() >= POKE_EVERY) {
            poke_round(&rpc, &payer, &quotes, o.priority_fee);
            last_poke = Some(Instant::now());
        }
        crank_round(&rpc, &payer, o.priority_fee);
        if cmd == "once" {
            return;
        }
        sleep(TICK);
    }
}
