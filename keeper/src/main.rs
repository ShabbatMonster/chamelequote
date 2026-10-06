//! chamelequote keeper.
//!
//! A switch goes through as ONE transaction (see fast.rs): the program only lets the liquidity
//! out of the pool in a transaction that also puts it back, so the coin never stops trading. A
//! switch that can't land is retried until its deadline, then cancelled (burn refunded).
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
//!
//! Launch tooling (signed by --keypair, which becomes the admin):
//!   keeper init   --mint-keypair <path> --name <s> --symbol <s> --uri <url> --supply <raw> --burn <raw>
//!                 --fee-recipient <pubkey> --fee-share-bps <n>
//!   keeper list   --routes data/orca-routes.json [--min-tvl-curated 50000] [--min-tvl-custom 250000]
//!                 [--out docs/quotes.json]   (the listed mints, for the website)
//!   keeper launch --quote <mint> --price-num <n> --price-den <n>   (raw quote per raw token)
//!   keeper request --quote <mint>   burn the keypair's coins to request a switch (testing)
//!   keeper allow  --allowlist data/allowlist.json --routes data/orca-routes.json [--min-tvl 10000]
//!                 [--out docs/quotes.json]   enable exactly the allowlist (listing what's missing)

mod fast;

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
use chamelequote::{instructions::InitializeParams, math};
use chamelequote_crank::{
    config_pda, initialize_ix, launch_ix, list_quote_ix, quote_pda, request_switch_ix, set_quote_enabled_ix,
    Action, Crank, Ledger, QUOTE_ENTRY_DISCRIMINATOR,
};
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
const POKE_EVERY: Duration = Duration::from_secs(30);
const RELIST_EVERY: Duration = Duration::from_secs(600);
const TICK: Duration = Duration::from_secs(1);
// How often a requested switch is retried until it lands (the deadline is 10 minutes).
const RETRY_EVERY: Duration = Duration::from_secs(3);
const CLAIM_EVERY: Duration = Duration::from_secs(900);

// Mainnet defaults for `init`. (The pool config fields are unused since the move to DAMM v2.)
const CLMM_CONFIG: &str = "11111111111111111111111111111111";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const WSOL: &str = "So11111111111111111111111111111111111111112";
const TICK_SPACING: u16 = 120;

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
    /// Everything else, by flag name (for the admin commands).
    extra: std::collections::HashMap<String, String>,
}

impl Opts {
    fn get(&self, k: &str) -> String {
        self.extra.get(k).cloned().unwrap_or_else(|| panic!("--{k} is required"))
    }
    fn get_or(&self, k: &str, d: &str) -> String {
        self.extra.get(k).cloned().unwrap_or_else(|| d.to_string())
    }
    fn pubkey(&self, k: &str) -> Pubkey {
        self.get(k).parse().unwrap_or_else(|_| panic!("--{k} must be a public key"))
    }
    fn num(&self, k: &str) -> u64 {
        self.get(k).parse().unwrap_or_else(|_| panic!("--{k} must be a number"))
    }
}

/// Masks `api-key=...` (and similar query secrets) so RPC errors never print keys.
fn redact(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(i) = rest.find("api-key=").or_else(|| rest.find("api_key=")) {
        out.push_str(&rest[..i + 8]);
        out.push_str("***");
        rest = &rest[i + 8..];
        let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn log(msg: &str) {
    let msg = &redact(msg);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    println!("[{:02}:{:02}:{:02}Z] {msg}", (t / 3600) % 24, (t / 60) % 60, t % 60);
}

fn cu_limit(label: &str) -> u32 {
    match label {
        "poke quotes" | "poke pool" | "claim fees" | "list" | "launch" => 300_000,
        _ => 1_400_000,
    }
}

fn send(rpc: &Rpc, payer: &Keypair, action: &Action, priority_fee: u64) -> Result<String, String> {
    let mut ixs = vec![Instruction {
        program_id: COMPUTE_BUDGET,
        accounts: Vec::<AccountMeta>::new(),
        // Priority fees are charged per requested compute unit, so ask only for what a step needs.
        data: [vec![2u8], cu_limit(action.label).to_le_bytes().to_vec()].concat(),
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
    if bincode::serialize(&tx).map(|b| b.len()).unwrap_or(usize::MAX) > fast::MAX_TX_BYTES {
        // Too big for a legacy transaction (a switch step names ~30 accounts): v0 with lookup tables.
        let keys: Vec<Pubkey> = action.ixs.iter().flat_map(|ix| ix.accounts.iter().map(|m| m.pubkey)).collect();
        let alts = fast::ensure_alts(rpc, payer, &keys, priority_fee)?;
        let extra: Vec<&Keypair> = action.signers.iter().collect();
        return fast::send_v0(
            rpc,
            payer,
            &extra,
            action.label,
            &action.ixs,
            &alts,
            cu_limit(action.label),
            priority_fee,
            CommitmentConfig::confirmed(),
        );
    }
    // Simulate (so program errors come back with their logs), then rebroadcast every 2 s until
    // confirmed or 30 s pass: a dropped send costs seconds instead of a minute.
    let sim = rpc
        .0
        .simulate_transaction_with_config(
            &tx,
            // Simulate on the node's own blockhash: ours may be newer than what that node has seen.
            solana_rpc_client_api::config::RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: true,
                ..Default::default()
            },
        )
        .map_err(|e| format!("{e:?}"))?
        .value;
    if let Some(err) = sim.err {
        return Err(format!("simulation failed: {err:?} logs: Some([{}])", sim.logs.unwrap_or_default().join(", ")));
    }
    let sig = tx.signatures[0];
    let started = Instant::now();
    loop {
        let _ = rpc.0.send_transaction_with_config(
            &tx,
            solana_rpc_client_api::config::RpcSendTransactionConfig { skip_preflight: true, max_retries: Some(0), ..Default::default() },
        );
        for _ in 0..4 {
            sleep(Duration::from_millis(500));
            if let Ok(st) = rpc.0.get_signature_statuses(&[sig]) {
                if let Some(Some(s)) = st.value.first() {
                    if let Some(err) = &s.err {
                        return Err(format!("failed on-chain: {err:?}"));
                    }
                    if s.satisfies_commitment(CommitmentConfig::confirmed()) {
                        return Ok(sig.to_string());
                    }
                }
            }
        }
        if started.elapsed() > Duration::from_secs(30) {
            return Err("unable to confirm transaction within 30 s".into());
        }
    }
}

/// Pokes every listed quote and the coin's pool.
/// Sends small independent transactions all at once and waits for them together (pokes): a
/// round takes seconds instead of a confirmation wait per transaction. Each result is the
/// signature, or why it failed (simulation or on-chain).
fn send_many(rpc: &Rpc, payer: &Keypair, actions: &[&Action], priority_fee: u64) -> Vec<Result<String, String>> {
    let blockhash = match rpc.0.get_latest_blockhash() {
        Ok(b) => b,
        Err(e) => return actions.iter().map(|_| Err(e.to_string())).collect(),
    };
    let mut out: Vec<Option<Result<String, String>>> = vec![None; actions.len()];
    let mut txs = vec![];
    for (k, action) in actions.iter().enumerate() {
        let mut ixs = vec![Instruction {
            program_id: COMPUTE_BUDGET,
            accounts: vec![],
            data: [vec![2u8], cu_limit(action.label).to_le_bytes().to_vec()].concat(),
        }];
        if priority_fee > 0 {
            ixs.push(Instruction {
                program_id: COMPUTE_BUDGET,
                accounts: vec![],
                data: [vec![3u8], priority_fee.to_le_bytes().to_vec()].concat(),
            });
        }
        ixs.extend(action.ixs.iter().cloned());
        let tx = Transaction::new_signed_with_payer(&ixs, Some(&payer.pubkey()), &[payer], blockhash);
        // Preflight catches program errors (a bad quote) right away. Paced, and retried when the
        // RPC says it's getting too many requests.
        let mut sent = rpc.0.send_transaction(&tx);
        for wait in [1000, 2000, 4000] {
            match &sent {
                Err(e) if e.to_string().contains("429") => {
                    sleep(Duration::from_millis(wait));
                    sent = rpc.0.send_transaction(&tx);
                }
                _ => break,
            }
        }
        match sent {
            Ok(_) => txs.push((k, tx)),
            Err(e) => out[k] = Some(Err(format!("simulation failed: {e}"))),
        }
        sleep(Duration::from_millis(150));
    }
    let started = Instant::now();
    while !txs.is_empty() && started.elapsed() < Duration::from_secs(30) {
        sleep(Duration::from_millis(1500));
        let sigs: Vec<_> = txs.iter().map(|(_, t)| t.signatures[0]).collect();
        let statuses = rpc.0.get_signature_statuses(&sigs).map(|r| r.value).unwrap_or_default();
        let mut pending = vec![];
        for (n, (k, tx)) in txs.into_iter().enumerate() {
            match statuses.get(n).cloned().flatten() {
                Some(st) if st.err.is_some() => out[k] = Some(Err(format!("failed on-chain: {:?}", st.err))),
                Some(st) if st.satisfies_commitment(CommitmentConfig::confirmed()) => out[k] = Some(Ok(tx.signatures[0].to_string())),
                _ => {
                    // Rebroadcast what hasn't landed yet.
                    let _ = rpc.0.send_transaction_with_config(
                        &tx,
                        solana_rpc_client_api::config::RpcSendTransactionConfig { skip_preflight: true, max_retries: Some(0), ..Default::default() },
                    );
                    pending.push((k, tx));
                }
            }
        }
        txs = pending;
    }
    out.into_iter().map(|r| r.unwrap_or_else(|| Err("unable to confirm transaction within 30 s".into()))).collect()
}

fn poke_round(rpc: &Rpc, payer: &Keypair, quotes: &[QuoteEntry], fee: u64) {
    let crank = Crank::new(rpc, payer.pubkey());
    let c = crank.config();
    let needed = |q: &QuoteEntry| {
        q.enabled || q.mint == c.active_quote || q.mint == c.wsol || q.mint == c.switch.holding || q.mint == c.switch.target
    };
    let live: Vec<&QuoteEntry> = quotes.iter().filter(|q| !q.is_root() && needed(q)).collect();
    let mut actions: Vec<(Action, Vec<&QuoteEntry>)> = live
        .chunks(12)
        .map(|chunk| {
            let entries: Vec<QuoteEntry> = chunk.iter().map(|q| (*q).clone()).collect();
            (crank.poke_quotes(&entries).remove(0), chunk.to_vec())
        })
        .collect();
    if let Some(p) = crank.poke_pool() {
        actions.insert(0, (p, vec![]));
    }
    let (mut ok, mut failed) = (0, 0);
    let results = send_many(rpc, payer, &actions.iter().map(|a| &a.0).collect::<Vec<_>>(), fee);
    // One bad quote fails its whole batch: retry those singly so the rest stay fresh, and name it.
    let mut singles: Vec<(Action, Pubkey)> = vec![];
    for ((a, chunk), r) in actions.iter().zip(results) {
        match r {
            Ok(_) => ok += 1,
            Err(e) if chunk.len() > 1 => {
                log(&format!("poke batch failed, retrying singly: {}", first_line(&e)));
                singles.extend(chunk.iter().map(|q| (crank.poke_quotes(&[(*q).clone()]).remove(0), q.mint)));
            }
            Err(e) => {
                failed += 1;
                log(&format!("{} failed: {}", a.label, first_line(&e)));
            }
        }
    }
    if !singles.is_empty() {
        let results = send_many(rpc, payer, &singles.iter().map(|s| &s.0).collect::<Vec<_>>(), fee);
        for ((_, mint), r) in singles.iter().zip(results) {
            match r {
                Ok(_) => ok += 1,
                Err(e) => {
                    failed += 1;
                    log(&format!("poke {mint} failed: {}", first_line(&e)));
                }
            }
        }
    }
    if failed > 0 || ok > 0 {
        log(&format!("poked {} quotes + pool ({ok} tx ok, {failed} failed)", live.len()));
    }
}

/// Keeps every average fresh on its own clock, so switch retries never delay it: an average
/// poked more than two minutes apart restarts and needs ten minutes to warm up again.
fn poke_loop(url: String, payer: Keypair, fee: u64) {
    let rpc = Rpc(RpcClient::new_with_commitment(url, CommitmentConfig::confirmed()));
    let mut quotes = rpc.quote_entries();
    let mut last_list = Instant::now();
    loop {
        if last_list.elapsed() >= RELIST_EVERY {
            let fresh = rpc.quote_entries();
            if !fresh.is_empty() {
                quotes = fresh;
            }
            last_list = Instant::now();
        }
        let started = Instant::now();
        let round = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poke_round(&rpc, &payer, &quotes, fee)));
        if round.is_err() {
            log("poke round failed to read state; retrying");
        }
        sleep(POKE_EVERY.saturating_sub(started.elapsed()));
    }
}

/// Pushes an in-flight switch forward until idle or a step fails. Returns steps sent.
/// A fresh request takes the fast path once; anything else, or a request whose fast attempt
/// failed, goes step by step.
fn crank_round(rpc: &Rpc, payer: &Keypair, fee: u64, last_try: &mut Option<Instant>) -> usize {
    // A request goes through as ONE transaction (the program won't let the liquidity out
    // otherwise), retried every few seconds until it lands or its deadline passes; it is never
    // sent step by step.
    let state = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Crank::new(rpc, payer.pubkey()).config()));
    if let Ok(c) = state {
        if c.switch.phase == chamelequote::state::Phase::Requested && rpc.now() <= c.switch.deadline {
            if last_try.is_some_and(|t| t.elapsed() < RETRY_EVERY) {
                return 0;
            }
            *last_try = Some(Instant::now());
            return match fast::try_fast(rpc, payer, fee) {
                Ok(true) => {
                    log("switch done in one transaction");
                    1
                }
                Ok(false) => 0,
                Err(e) => {
                    log(&format!("switch didn't land, will retry: {}", first_line(&e)));
                    0
                }
            };
        }
    }
    // Everything else: seeding a new pool's sentinel, a launch, cancelling an expired request.
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
    if e.contains("unable to confirm") {
        return "dropped before confirmation (will retry)".into();
    }
    // Simulation failures: the transaction error and the last program log lines say why.
    if let Some(i) = e.find("err: Some(") {
        let err: String = e[i + 10..].chars().take_while(|c| *c != ')').collect();
        let logs = e.find("logs: Some([").map(|j| e[j..].chars().take(600).collect::<String>()).unwrap_or_default();
        return format!("simulation failed: {err}) {logs}");
    }
    e.chars().take(300).collect()
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
    println!("active pool   {} (Meteora DAMM v2)", c.active_pool);
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
    let mut o = Opts {
        rpc: std::env::var("RPC_URL").unwrap_or_else(|_| "https://api.devnet.solana.com".into()),
        keypair: std::env::var("KEEPER_KEYPAIR").ok(),
        priority_fee: 0,
        extra: Default::default(),
    };
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--rpc" => o.rpc = val(),
            "--keypair" => o.keypair = Some(val()),
            "--priority-fee" => o.priority_fee = val().parse().expect("--priority-fee takes a number"),
            other if other.starts_with("--") => {
                let v = val();
                o.extra.insert(other[2..].to_string(), v);
            }
            other => panic!("unknown argument {other}"),
        }
    }
    (cmd, o)
}

fn main() {
    let (cmd, o) = parse();
    let rpc = Rpc(RpcClient::new_with_commitment(o.rpc.clone(), CommitmentConfig::confirmed()));
    match cmd.as_str() {
        "status" => return status(&rpc),
        "run" | "once" | "init" | "list" | "launch" | "allow" | "request" => {}
        _ => {
            println!("usage: keeper run|once|status|init|list|launch --rpc <url> --keypair <path> ... (see source header)");
            return;
        }
    }
    let path = o.keypair.clone().expect("--keypair (or KEEPER_KEYPAIR) is required");
    let payer = read_keypair_file(&path).unwrap_or_else(|e| panic!("could not read keypair {path}: {e}"));
    let shown = o.rpc.split('?').next().unwrap_or(&o.rpc); // drop any ?api-key=... from logs
    log(&format!("keeper {} on {shown} (program {PROGRAM_ID})", payer.pubkey()));
    match rpc.0.get_balance(&payer.pubkey()) {
        Ok(b) => log(&format!("balance {:.4} SOL", b as f64 / 1e9)),
        Err(e) => log(&format!("could not read balance: {e}")),
    }
    match cmd.as_str() {
        "init" => return admin_init(&rpc, &payer, &o),
        "list" => return admin_list(&rpc, &payer, &o),
        "launch" => return admin_launch(&rpc, &payer, &o),
        "allow" => return admin_allow(&rpc, &payer, &o),
        "request" => {
            // Burns the keypair's own coins: a holder's switch request, for testing and ops.
            let c = Crank::new(&rpc, payer.pubkey()).config();
            let ix = request_switch_ix(&payer.pubkey(), &c, &o.pubkey("quote"));
            return send_ixs(&rpc, &payer, "request switch", vec![ix], vec![], o.priority_fee);
        }

        _ => {}
    }
    if rpc.account(&config_pda()).is_none() {
        log("program is not initialized on this cluster yet; nothing to do");
        return;
    }

    let mut last_claim = None::<Instant>;
    let mut last_try = None::<Instant>;
    if cmd == "once" {
        poke_round(&rpc, &payer, &rpc.quote_entries(), o.priority_fee);
    } else {
        let (url, poker, fee) = (o.rpc.clone(), payer.insecure_clone(), o.priority_fee);
        std::thread::spawn(move || poke_loop(url, poker, fee));
    }
    loop {
        crank_round(&rpc, &payer, o.priority_fee, &mut last_try);
        if last_claim.is_none_or(|t| t.elapsed() >= CLAIM_EVERY) {
            if let Some(a) = Crank::new(&rpc, payer.pubkey()).claim_fees() {
                match send(&rpc, &payer, &a, o.priority_fee) {
                    Ok(sig) => log(&format!("claimed fees: {sig}")),
                    Err(e) => log(&format!("claim fees failed: {}", first_line(&e))),
                }
            }
            last_claim = Some(Instant::now());
        }
        if cmd == "once" {
            return;
        }
        sleep(TICK);
    }
}

// ---------------------------------------------------------------------------------------------
// Launch tooling

fn send_ixs(rpc: &Rpc, payer: &Keypair, label: &'static str, ixs: Vec<Instruction>, signers: Vec<Keypair>, fee: u64) {
    let action = Action { label, ixs, signers };
    match send(rpc, payer, &action, fee) {
        Ok(sig) => log(&format!("{label}: {sig}")),
        Err(e) => {
            log(&format!("{label} FAILED: {}", first_line(&e)));
            std::process::exit(1);
        }
    }
}

fn admin_init(rpc: &Rpc, payer: &Keypair, o: &Opts) {
    let mint_kp = read_keypair_file(o.get("mint-keypair")).expect("could not read --mint-keypair");
    let params = InitializeParams {
        name: o.get("name"),
        symbol: o.get("symbol"),
        uri: o.get("uri"),
        supply: o.num("supply"),
        burn_amount: o.num("burn"),
        clmm_config: o.get_or("clmm-config", CLMM_CONFIG).parse().unwrap(),
        tick_spacing: o.get_or("tick-spacing", &TICK_SPACING.to_string()).parse().unwrap(),
        usdc: o.get_or("usdc", USDC).parse().unwrap(),
        wsol: o.get_or("wsol", WSOL).parse().unwrap(),
        fee_recipient: o.pubkey("fee-recipient"),
        fee_share_bps: o.num("fee-share-bps") as u16,
    };
    log(&format!("initializing mint {}", mint_kp.pubkey()));
    let ix = initialize_ix(&payer.pubkey(), &mint_kp.pubkey(), params);
    send_ixs(rpc, payer, "initialize", vec![ix], vec![mint_kp], o.priority_fee);
}

/// Lists USDC, then WSOL (via the SOL/USDC hub pool), then every route in the file that clears
/// the liquidity bar. Already-listed quotes are skipped, so it can be re-run.
fn admin_list(rpc: &Rpc, payer: &Keypair, o: &Opts) {
    let routes: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(o.get("routes")).expect("could not read --routes")).expect("bad routes json");
    let min_curated: f64 = o.get_or("min-tvl-curated", "50000").parse().unwrap();
    let min_custom: f64 = o.get_or("min-tvl-custom", "250000").parse().unwrap();
    let c = Crank::new(rpc, payer.pubkey()).config();
    let hub_pool: Pubkey = routes["hubPool"]["pool"].as_str().expect("hubPool.pool").parse().unwrap();

    let mut todo: Vec<(Pubkey, Option<(Pubkey, Pubkey)>, String)> =
        vec![(c.usdc, None, "USDC".into()), (c.wsol, Some((c.usdc, hub_pool)), "SOL".into())];
    for r in routes["routes"].as_array().expect("routes") {
        let mint: Pubkey = r["mint"].as_str().unwrap().parse().unwrap();
        if mint == c.usdc || mint == c.wsol || mint == c.mint {
            continue;
        }
        let tvl = r["tvlUsd"].as_f64().or_else(|| r["tvlUsd"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0);
        let bar = if r["category"] == "custom" { min_custom } else { min_curated };
        if tvl < bar {
            continue;
        }
        let hub = if r["hub"] == "USDC" { c.usdc } else { c.wsol };
        let pool: Pubkey = r["pool"].as_str().unwrap().parse().unwrap();
        todo.push((mint, Some((hub, pool)), format!("{} (${:.0}k)", r["symbol"].as_str().unwrap_or("?"), tvl / 1000.0)));
    }
    log(&format!("{} quotes pass the bar", todo.len()));
    for (mint, hub_route, label) in todo {
        if rpc.account(&quote_pda(&mint)).is_some() {
            continue;
        }
        let action = Action { label: "list", ixs: vec![list_quote_ix(&payer.pubkey(), &mint, hub_route)], signers: vec![] };
        match send(rpc, payer, &action, o.priority_fee) {
            Ok(_) => log(&format!("listed {label}")),
            Err(e) => log(&format!("could not list {label}: {}", first_line(&e))),
        }
    }
    write_quotes_file(rpc, o);
}

/// The website can't list program accounts through free RPCs, so it reads this file instead.
/// It holds every listed mint (disabled ones too: the active quote may be disabled).
fn write_quotes_file(rpc: &Rpc, o: &Opts) {
    if let Some(out) = o.extra.get("out") {
        let mut mints: Vec<String> = rpc.quote_entries().iter().map(|q| q.mint.to_string()).collect();
        mints.sort();
        std::fs::write(out, serde_json::to_string_pretty(&serde_json::json!({ "quotes": mints })).unwrap()).expect("could not write --out");
        log(&format!("wrote {} listed quotes to {out}", mints.len()));
    }
}

/// Makes the enabled set exactly the allowlist entries that have an Orca route with at least
/// --min-tvl of liquidity: lists missing ones, enables listed ones, disables everything else.
/// USDC and WSOL stay listed regardless (they are the routing hubs).
fn admin_allow(rpc: &Rpc, payer: &Keypair, o: &Opts) {
    let allow: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(o.get("allowlist")).expect("could not read --allowlist")).expect("bad allowlist");
    let routes: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(o.get("routes")).expect("could not read --routes")).expect("bad routes json");
    let min_tvl: f64 = o.get_or("min-tvl", "10000").parse().unwrap();
    let c = Crank::new(rpc, payer.pubkey()).config();
    let hub_pool: Pubkey = routes["hubPool"]["pool"].as_str().expect("hubPool.pool").parse().unwrap();
    let route_of = |mint: &str| routes["routes"].as_array().unwrap().iter().find(|r| r["mint"] == mint).cloned();
    let tvl_of = |r: &serde_json::Value| r["tvlUsd"].as_f64().or_else(|| r["tvlUsd"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0);

    let mut wanted: Vec<(Pubkey, String)> = vec![];
    for e in allow["quotes"].as_array().expect("quotes") {
        let (mint_s, sym) = (e["mint"].as_str().unwrap(), e["symbol"].as_str().unwrap_or("?").to_string());
        let mint: Pubkey = mint_s.parse().unwrap();
        if mint == c.usdc || mint == c.wsol {
            wanted.push((mint, sym));
            continue;
        }
        match route_of(mint_s) {
            Some(r) if tvl_of(&r) >= min_tvl => wanted.push((mint, sym)),
            Some(r) => log(&format!("skip {sym}: Orca route has only ${:.0} of liquidity", tvl_of(&r))),
            None => log(&format!("skip {sym}: no Orca route against USDC or SOL")),
        }
    }

    let listed = rpc.quote_entries();
    let send_one = |label: String, ix: Instruction| {
        let action = Action { label: "allow", ixs: vec![ix], signers: vec![] };
        match send(rpc, payer, &action, o.priority_fee) {
            Ok(_) => log(&label),
            Err(e) => log(&format!("FAILED {label}: {}", first_line(&e))),
        }
    };
    for (mint, sym) in &wanted {
        match listed.iter().find(|q| q.mint == *mint) {
            Some(q) if q.enabled => {}
            Some(_) => send_one(format!("enabled {sym}"), set_quote_enabled_ix(&payer.pubkey(), mint, true)),
            None => {
                let hub_route = if *mint == c.usdc {
                    None
                } else if *mint == c.wsol {
                    Some((c.usdc, hub_pool))
                } else {
                    let r = route_of(&mint.to_string()).unwrap();
                    let hub = if r["hub"] == "USDC" { c.usdc } else { c.wsol };
                    Some((hub, r["pool"].as_str().unwrap().parse().unwrap()))
                };
                send_one(format!("listed {sym}"), list_quote_ix(&payer.pubkey(), mint, hub_route));
            }
        }
    }
    for q in listed.iter().filter(|q| q.enabled && !wanted.iter().any(|(m, _)| *m == q.mint)) {
        send_one(format!("disabled {}", q.mint), set_quote_enabled_ix(&payer.pubkey(), &q.mint, false));
    }
    log(&format!("{} quotes allowed", wanted.len()));
    write_quotes_file(rpc, o);
}

fn admin_launch(rpc: &Rpc, payer: &Keypair, o: &Opts) {
    let quote = o.pubkey("quote");
    let sqrt = math::sqrt_ratio(o.num("price-num") as u128, o.num("price-den") as u128).expect("bad price");
    log(&format!("launching against {quote} at sqrt price {sqrt}"));
    send_ixs(rpc, payer, "launch", vec![launch_ix(&payer.pubkey(), &quote, sqrt)], vec![], o.priority_fee);
}
