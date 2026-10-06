//! Switches. Every step (pull, hops, then seed/reprice and add on the target pool) goes into ONE
//! transaction, so a switch lands whole or not at all and the coin never stops trading; the
//! program refuses a pull without its add. The 64-account and 64-entry trace limits hold for the
//! longest route (three hops into a new pool). Transactions are v0 and use address lookup tables kept
//! by this keeper (~/.config/chamelequote/alts.json) to stay under 1232 bytes.
//!
//! (Jito bundles were tried first and kept coming back "Invalid" with no reason given; one
//! transaction is atomic by definition and needs no third party.)

use std::{path::PathBuf, thread::sleep, time::Duration};

use anchor_lang::{
    prelude::Pubkey,
    solana_program::instruction::{AccountMeta, Instruction},
};
use chamelequote_crank::{unique_accounts, Crank, MAX_TX_ACCOUNTS};
use solana_commitment_config::CommitmentConfig;
use solana_keypair::Keypair;
use solana_message::{v0, AddressLookupTableAccount, VersionedMessage};
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_rpc_client_api::config::{RpcSendTransactionConfig, RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

use crate::{first_line, log, send, Rpc};

const ALT_PROGRAM: Pubkey = anchor_lang::prelude::pubkey!("AddressLookupTab1e1111111111111111111111111");
const SYSTEM: Pubkey = anchor_lang::prelude::pubkey!("11111111111111111111111111111111");
const COMPUTE_BUDGET: Pubkey = anchor_lang::prelude::pubkey!("ComputeBudget111111111111111111111111111111");
const ALT_META_SIZE: usize = 56;
const ALT_MAX: usize = 256;
pub const MAX_TX_BYTES: usize = 1232;
/// Priority fee for switch transactions (micro-lamports per CU): ~0.0003 SOL at 1.4M CU, so they
/// land in the next block or two.
const SWITCH_PRIORITY: u64 = 200_000;

fn alts_file() -> PathBuf {
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("chamelequote").join("alts.json")
}

fn load_alt(rpc: &Rpc, key: &Pubkey) -> Option<AddressLookupTableAccount> {
    let (_, d) = chamelequote_crank::Ledger::account(rpc, key)?;
    let addresses = d.get(ALT_META_SIZE..)?.chunks_exact(32).map(|c| Pubkey::try_from(c).unwrap()).collect();
    Some(AddressLookupTableAccount { key: *key, addresses })
}

fn load_alts(rpc: &Rpc) -> Vec<AddressLookupTableAccount> {
    let keys: Vec<String> = std::fs::read_to_string(alts_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    keys.iter().filter_map(|k| k.parse().ok()).filter_map(|k| load_alt(rpc, &k)).collect()
}

fn save_alts(alts: &[AddressLookupTableAccount]) {
    let keys: Vec<String> = alts.iter().map(|a| a.key.to_string()).collect();
    let _ = std::fs::write(alts_file(), serde_json::to_string_pretty(&keys).unwrap());
}

fn create_alt_ix(authority: &Pubkey, slot: u64) -> (Pubkey, Instruction) {
    let (key, bump) = Pubkey::find_program_address(&[authority.as_ref(), &slot.to_le_bytes()], &ALT_PROGRAM);
    let mut data = 0u32.to_le_bytes().to_vec();
    data.extend(slot.to_le_bytes());
    data.push(bump);
    let ix = Instruction {
        program_id: ALT_PROGRAM,
        accounts: vec![
            AccountMeta::new(key, false),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(*authority, true),
            AccountMeta::new_readonly(SYSTEM, false),
        ],
        data,
    };
    (key, ix)
}

fn extend_alt_ix(authority: &Pubkey, table: &Pubkey, keys: &[Pubkey]) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend((keys.len() as u64).to_le_bytes());
    for k in keys {
        data.extend(k.to_bytes());
    }
    Instruction {
        program_id: ALT_PROGRAM,
        accounts: vec![
            AccountMeta::new(*table, false),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(*authority, true),
            AccountMeta::new_readonly(SYSTEM, false),
        ],
        data,
    }
}

/// Makes sure every address in `needed` is in one of our lookup tables, creating or extending
/// tables as needed, and waits until new entries are usable (a slot later).
pub fn ensure_alts(rpc: &Rpc, payer: &Keypair, needed: &[Pubkey], fee: u64) -> Result<Vec<AddressLookupTableAccount>, String> {
    let mut alts = load_alts(rpc);
    let me = payer.pubkey();
    let mut missing: Vec<Pubkey> =
        needed.iter().filter(|k| **k != me && !alts.iter().any(|a| a.addresses.contains(k))).copied().collect();
    missing.sort();
    missing.dedup();
    if missing.is_empty() {
        return Ok(alts);
    }
    let mut last_slot = 0;
    while !missing.is_empty() {
        let idx = match alts.iter().position(|a| a.addresses.len() < ALT_MAX) {
            Some(i) => i,
            None => {
                // A slot every node has seen: the one checking it may be a few blocks behind ours
                // (a slot it doesn't know is rejected as invalid instruction data). Once more,
                // further back, if that still happens.
                let now = rpc.0.get_slot().map_err(|e| e.to_string())?;
                let mut created = Err(String::new());
                for back in [20, 150] {
                    let (key, ix) = create_alt_ix(&me, now.saturating_sub(back));
                    let action = chamelequote_crank::Action { label: "create lookup table", ixs: vec![ix], signers: vec![] };
                    created = send(rpc, payer, &action, fee).map(|_| key).map_err(|e| format!("create lookup table: {e}"));
                    if created.is_ok() {
                        break;
                    }
                }
                let key = created?;
                log(&format!("created lookup table {key}"));
                alts.push(AddressLookupTableAccount { key, addresses: vec![] });
                save_alts(&alts);
                alts.len() - 1
            }
        };
        let room = ALT_MAX - alts[idx].addresses.len();
        let chunk: Vec<Pubkey> = missing.drain(..missing.len().min(room).min(20)).collect();
        let ix = extend_alt_ix(&me, &alts[idx].key, &chunk);
        send(rpc, payer, &chamelequote_crank::Action { label: "extend lookup table", ixs: vec![ix], signers: vec![] }, fee)
            .map_err(|e| format!("extend lookup table: {e}"))?;
        alts[idx].addresses.extend(chunk);
        last_slot = rpc.0.get_slot().map_err(|e| e.to_string())?;
    }
    for _ in 0..20 {
        if rpc.0.get_slot().map_err(|e| e.to_string())? > last_slot + 1 {
            break;
        }
        sleep(Duration::from_millis(400));
    }
    Ok(alts)
}

fn budget(units: u32, price: u64) -> Vec<Instruction> {
    let mut out = vec![Instruction { program_id: COMPUTE_BUDGET, accounts: vec![], data: [vec![2u8], units.to_le_bytes().to_vec()].concat() }];
    if price > 0 {
        out.push(Instruction { program_id: COMPUTE_BUDGET, accounts: vec![], data: [vec![3u8], price.to_le_bytes().to_vec()].concat() });
    }
    out
}

/// Checks a simulation before sending: given token accounts to watch, it receives their balances
/// after the simulated transaction and may veto it.
pub type Verify<'a> = (&'a [Pubkey], &'a dyn Fn(&[u64]) -> Result<(), String>);

/// Builds, checks limits, simulates (and lets `verify` look at the result) and sends one v0
/// transaction (`signers` after the payer), then waits until it reaches `commitment`.
#[allow(clippy::too_many_arguments)]
pub fn send_v0(
    rpc: &Rpc,
    payer: &Keypair,
    signers: &[&Keypair],
    label: &str,
    ixs: &[Instruction],
    alts: &[AddressLookupTableAccount],
    units: u32,
    price: u64,
    commitment: CommitmentConfig,
    verify: Option<Verify>,
) -> Result<String, String> {
    let n = unique_accounts(ixs, &payer.pubkey());
    if n > MAX_TX_ACCOUNTS {
        return Err(format!("{label}: {n} accounts, over the {MAX_TX_ACCOUNTS} limit"));
    }
    let mut all = budget(units, price);
    all.extend(ixs.iter().cloned());
    let blockhash = rpc.0.get_latest_blockhash().map_err(|e| e.to_string())?;
    let msg = v0::Message::try_compile(&payer.pubkey(), &all, alts, blockhash).map_err(|e| e.to_string())?;
    let mut keys: Vec<&Keypair> = vec![payer];
    keys.extend(signers.iter().copied());
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &keys).map_err(|e| e.to_string())?;
    let size = bincode::serialize(&tx).map_err(|e| e.to_string())?.len();
    if size > MAX_TX_BYTES {
        return Err(format!("{label}: {size} bytes, over the {MAX_TX_BYTES} limit"));
    }
    let sim = rpc
        .0
        .simulate_transaction_with_config(
            &tx,
            // The RPC may answer from a node that hasn't seen our (fresh) blockhash yet; let it use
            // its own for the simulation instead of failing with BlockhashNotFound.
            RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: true,
                commitment: Some(CommitmentConfig::processed()),
                accounts: verify.map(|(watch, _)| RpcSimulateTransactionAccountsConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    addresses: watch.iter().map(|k| k.to_string()).collect(),
                }),
                ..Default::default()
            },
        )
        .map_err(|e| format!("{label} simulation: {e}"))?
        .value;
    if let Some(err) = sim.err {
        let logs = sim.logs.unwrap_or_default();
        let why: Vec<&String> = logs.iter().filter(|l| l.contains("Error") || l.contains("failed")).collect();
        return Err(format!("{label} would fail: {err:?} {why:?}"));
    }
    if let Some((_, check)) = verify {
        // Token amount (offset 64) of each watched account after the simulation; 0 if absent.
        let after: Vec<u64> = sim
            .accounts
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|a| {
                a.and_then(|a| a.data.decode())
                    .filter(|d| d.len() >= 72)
                    .map(|d| u64::from_le_bytes(d[64..72].try_into().unwrap()))
                    .unwrap_or(0)
            })
            .collect();
        check(&after).map_err(|e| format!("{label}: {e}"))?;
    }
    // Rebroadcast every 2 s until it lands (or 30 s): a dropped send costs seconds, not the
    // minute the RPC client's own confirm loop waits.
    let sig = tx.signatures[0];
    let started = std::time::Instant::now();
    'outer: loop {
        let _ = rpc.0.send_transaction_with_config(
            &tx,
            RpcSendTransactionConfig { skip_preflight: true, max_retries: Some(0), ..Default::default() },
        );
        for _ in 0..10 {
            sleep(Duration::from_millis(200));
            if let Ok(st) = rpc.0.get_signature_statuses(&[sig]) {
                if let Some(Some(s)) = st.value.first() {
                    if let Some(err) = &s.err {
                        return Err(format!("{label} failed on-chain: {err:?}"));
                    }
                    if s.satisfies_commitment(commitment) {
                        break 'outer;
                    }
                }
            }
        }
        if started.elapsed() > Duration::from_secs(30) {
            return Err(format!("{label}: not confirmed within 30 s"));
        }
    }
    log(&format!("{label}: {sig} ({n} accounts, {size} bytes, {} CU)", sim.units_consumed.unwrap_or(0)));
    Ok(sig.to_string())
}

/// One fast attempt at the request in flight. Ok(true) when the switch completed.
pub fn try_fast(rpc: &Rpc, payer: &Keypair, fee: u64) -> Result<bool, String> {
    let me = payer.pubkey();
    let crank = Crank::new(rpc, me);
    let Some(est) = crank.estimate() else { return Ok(false) };

    // Only token accounts that don't exist yet: the regular poke round keeps every average fresh,
    // and the pull claims the fees itself, so a retry every few seconds costs one transaction.
    let prep = crank.missing_accounts(&est);
    if !prep.is_empty() {
        let a = chamelequote_crank::Action { label: "prep accounts", ixs: prep, signers: vec![] };
        send(rpc, payer, &a, fee).map_err(|e| format!("{}: {}", a.label, first_line(&e)))?;
    }

    // Built after the prep so it sees fresh state; every account goes in the lookup tables
    // first. Compact tick arrays for the hops first (fewer accounts), the full sets if a hop
    // leaves its array.
    let crank = Crank::new(rpc, me);
    let compact = crank.switch_tx(&est, true);
    let full = crank.switch_tx(&est, false);
    let needed: Vec<Pubkey> =
        compact.iter().chain(full.iter()).flatten().flat_map(|ix| ix.accounts.iter().map(|m| m.pubkey)).collect();
    if needed.is_empty() {
        return Err(compact.err().unwrap_or_default());
    }
    let alts = ensure_alts(rpc, payer, &needed, fee)?;

    // Cross-check with Jupiter before anything is sent: how much of the old quote comes out of
    // the active pool (its quote vault) against how much of the new one lands in the target pool
    // (its quote vault, plus whatever the program keeps aside), from the simulation.
    let c = crank.config();
    let old_vault = crank.our_pool(&c.active_pool).map(|p| p.vault_b).ok_or("active pool missing")?;
    let new_vault = crank.our_pool(&est.target_pool).map(|p| p.vault_b).unwrap_or_else(|| {
        chamelequote::damm::vault_address(&est.target_pool, &c.switch.target)
    });
    let ours_new = crank.ours(&c.switch.target);
    let watch = [old_vault, new_vault, ours_new];
    let before: Vec<u64> = watch.iter().map(|k| crank.balance(k)).collect();
    let (from, to) = (c.active_quote, c.switch.target);
    let check = move |after: &[u64]| {
        let pulled = before[0].saturating_sub(after[0]);
        let delivered = (after[1] + after[2]).saturating_sub(before[1] + before[2]);
        crate::jupiter::check_switch(&from, &to, pulled, delivered)
    };

    let confirmed = CommitmentConfig::confirmed();
    let mut last = String::new();
    for ixs in [compact, full].into_iter().flatten() {
        match send_v0(rpc, payer, &[], "switch", &ixs, &alts, 1_400_000, SWITCH_PRIORITY, confirmed, Some((&watch, &check))) {
            Ok(_) => return Ok(true),
            Err(e) => {
                log(&format!("switch attempt: {}", first_line(&e)));
                last = e;
            }
        }
    }
    Err(last)
}
