//! Fast switches. A one-hop switch (pull, swap, reprice, add) goes in ONE transaction, so it is
//! atomic: the coin is never without liquidity. Longer routes go as two transactions sent back to
//! back: pull + every hop, then (as soon as that lands) reprice + add, so the pool is empty for
//! about a second. Both are simulated before sending. Transactions are v0 and use address lookup
//! tables kept by this keeper (~/.config/chamelequote/alts.json) to stay under 1232 bytes.
//!
//! (Jito bundles were tried first and kept coming back "Invalid" with no reason given; one
//! transaction is atomic by definition and needs no third party.)

use std::{path::PathBuf, thread::sleep, time::Duration};

use anchor_lang::{
    prelude::Pubkey,
    solana_program::instruction::{AccountMeta, Instruction},
};
use chamelequote_crank::{Crank, Ledger};
use solana_commitment_config::CommitmentConfig;
use solana_keypair::Keypair;
use solana_message::{v0, AddressLookupTableAccount, VersionedMessage};
use solana_rpc_client_api::config::{RpcSendTransactionConfig, RpcSimulateTransactionConfig};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

use crate::{first_line, log, send, Rpc};

const ALT_PROGRAM: Pubkey = anchor_lang::prelude::pubkey!("AddressLookupTab1e1111111111111111111111111");
const SYSTEM: Pubkey = anchor_lang::prelude::pubkey!("11111111111111111111111111111111");
const COMPUTE_BUDGET: Pubkey = anchor_lang::prelude::pubkey!("ComputeBudget111111111111111111111111111111");
const ALT_META_SIZE: usize = 56;
const ALT_MAX: usize = 256;
const MAX_TX_BYTES: usize = 1232;
const MAX_ACCOUNTS: usize = 64;
/// Priority fee for switch transactions (micro-lamports per CU): ~0.0003 SOL at 1.4M CU, so they
/// land in the next block or two.
const SWITCH_PRIORITY: u64 = 200_000;

fn alts_file() -> PathBuf {
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("chamelequote").join("alts.json")
}

fn load_alt(rpc: &Rpc, key: &Pubkey) -> Option<AddressLookupTableAccount> {
    let (_, d) = rpc.account(key)?;
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
fn ensure_alts(rpc: &Rpc, payer: &Keypair, needed: &[Pubkey], fee: u64) -> Result<Vec<AddressLookupTableAccount>, String> {
    let mut alts = load_alts(rpc);
    let mut missing: Vec<Pubkey> = needed.iter().filter(|k| !alts.iter().any(|a| a.addresses.contains(k))).copied().collect();
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
                let slot = rpc.0.get_slot().map_err(|e| e.to_string())?.saturating_sub(1);
                let (key, ix) = create_alt_ix(&payer.pubkey(), slot);
                send(rpc, payer, &chamelequote_crank::Action { label: "create lookup table", ixs: vec![ix], signers: vec![] }, fee)?;
                log(&format!("created lookup table {key}"));
                alts.push(AddressLookupTableAccount { key, addresses: vec![] });
                save_alts(&alts);
                alts.len() - 1
            }
        };
        let room = ALT_MAX - alts[idx].addresses.len();
        let chunk: Vec<Pubkey> = missing.drain(..missing.len().min(room).min(20)).collect();
        let ix = extend_alt_ix(&payer.pubkey(), &alts[idx].key, &chunk);
        send(rpc, payer, &chamelequote_crank::Action { label: "extend lookup table", ixs: vec![ix], signers: vec![] }, fee)?;
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

fn budget(units: u32) -> [Instruction; 2] {
    [
        Instruction { program_id: COMPUTE_BUDGET, accounts: vec![], data: [vec![2u8], units.to_le_bytes().to_vec()].concat() },
        Instruction { program_id: COMPUTE_BUDGET, accounts: vec![], data: [vec![3u8], SWITCH_PRIORITY.to_le_bytes().to_vec()].concat() },
    ]
}

fn unique_accounts(ixs: &[Instruction], payer: &Pubkey) -> usize {
    let mut keys: Vec<Pubkey> = ixs.iter().flat_map(|ix| ix.accounts.iter().map(|m| m.pubkey).chain([ix.program_id])).collect();
    keys.push(*payer);
    keys.push(COMPUTE_BUDGET);
    keys.sort();
    keys.dedup();
    keys.len()
}

/// Builds, checks limits, simulates and sends one v0 transaction; waits for confirmation.
fn send_v0(rpc: &Rpc, payer: &Keypair, label: &str, ixs: &[Instruction], alts: &[AddressLookupTableAccount]) -> Result<String, String> {
    let n = unique_accounts(ixs, &payer.pubkey());
    if n > MAX_ACCOUNTS {
        return Err(format!("{label}: {n} accounts, over the {MAX_ACCOUNTS} limit"));
    }
    let mut all = budget(1_400_000).to_vec();
    all.extend(ixs.iter().cloned());
    let blockhash = rpc.0.get_latest_blockhash().map_err(|e| e.to_string())?;
    let msg = v0::Message::try_compile(&payer.pubkey(), &all, alts, blockhash).map_err(|e| e.to_string())?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[payer]).map_err(|e| e.to_string())?;
    let size = bincode::serialize(&tx).map_err(|e| e.to_string())?.len();
    if size > MAX_TX_BYTES {
        return Err(format!("{label}: {size} bytes, over the {MAX_TX_BYTES} limit"));
    }
    let sim = rpc
        .0
        .simulate_transaction_with_config(
            &tx,
            RpcSimulateTransactionConfig { sig_verify: false, commitment: Some(CommitmentConfig::processed()), ..Default::default() },
        )
        .map_err(|e| format!("{label} simulation: {e}"))?
        .value;
    if let Some(err) = sim.err {
        let logs = sim.logs.unwrap_or_default();
        let why: Vec<&String> = logs.iter().filter(|l| l.contains("Error") || l.contains("failed")).collect();
        return Err(format!("{label} would fail: {err:?} {why:?}"));
    }
    // Rebroadcast every 2 s until confirmed (or 30 s): a dropped send costs seconds, not the
    // minute the RPC client's own confirm loop waits.
    let sig = tx.signatures[0];
    let started = std::time::Instant::now();
    'outer: loop {
        let _ = rpc.0.send_transaction_with_config(
            &tx,
            RpcSendTransactionConfig { skip_preflight: true, max_retries: Some(0), ..Default::default() },
        );
        for _ in 0..4 {
            sleep(Duration::from_millis(500));
            if let Ok(st) = rpc.0.get_signature_statuses(&[sig]) {
                if let Some(Some(s)) = st.value.first() {
                    if let Some(err) = &s.err {
                        return Err(format!("{label} failed on-chain: {err:?}"));
                    }
                    if s.satisfies_commitment(CommitmentConfig::confirmed()) {
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
    let crank = Crank::new(rpc, payer.pubkey());
    let Some(est) = crank.estimate() else { return Ok(false) };

    for a in crank.bundle_prep(&est) {
        if a.ixs.is_empty() {
            continue;
        }
        send(rpc, payer, &a, fee).map_err(|e| format!("{}: {}", a.label, first_line(&e)))?;
    }

    let crank = Crank::new(rpc, payer.pubkey());
    let compact = crank.switch_steps(&est, true);
    let full = crank.switch_steps(&est, false);
    let me = payer.pubkey();
    let mut needed: Vec<Pubkey> =
        compact.iter().chain(full.iter()).flatten().flat_map(|ix| ix.accounts.iter().map(|m| m.pubkey)).filter(|k| *k != me).collect();
    // The second transaction (reprice + add) is built only after the first lands, at the exact
    // target. Put every tick array it could touch in the lookup tables now, so nothing has to be
    // added (and waited on) while the pool is empty.
    let ts = crank.config().tick_spacing as i32;
    let span = ts * chamelequote::math::TICK_ARRAY_SIZE;
    let start = chamelequote::math::tick_array_start(chamelequote::math::tick_at_sqrt_price(est.pool_sqrt), ts);
    for k in -2..=2 {
        needed.push(chamelequote::whirlpool::tick_array_address(&est.target_pool, start + k * span));
    }
    for p in est.positions {
        for t in [p.0, p.1] {
            for k in -1..=1 {
                needed.push(chamelequote::whirlpool::tick_array_address(&est.target_pool, chamelequote::math::tick_array_start(t, ts) + k * span));
            }
        }
    }
    let alts = ensure_alts(rpc, payer, &needed, fee)?;

    // Try compact tick arrays first (fewer accounts); the full set if a swap leaves its array.
    let attempt = |steps: &Vec<Vec<Instruction>>, label: &str| -> Result<(), String> {
        if est.hops.len() <= 1 {
            let one: Vec<Instruction> = steps.iter().flatten().cloned().collect();
            send_v0(rpc, payer, label, &one, &alts).map(|_| ())
        } else {
            let first: Vec<Instruction> = steps[..steps.len() - 1].iter().flatten().cloned().collect();
            send_v0(rpc, payer, label, &first, &alts).map(|_| ())
        }
    };
    let label = if est.hops.len() <= 1 { "switch (one transaction)" } else { "pull + hops" };
    if let Err(e1) = attempt(&compact, label) {
        log(&format!("compact attempt: {}", first_line(&e1)));
        attempt(&full, label)?;
    }
    if est.hops.len() <= 1 {
        return Ok(true);
    }

    // Second transaction: reprice + add at the exact target, as soon as the first is confirmed.
    let finish = Crank::new(rpc, me).finish_ixs().ok_or("not in the repricing phase after the hops")?;
    let finish_keys: Vec<Pubkey> = finish.iter().flat_map(|ix| ix.accounts.iter().map(|m| m.pubkey)).filter(|k| *k != me).collect();
    let alts = ensure_alts(rpc, payer, &finish_keys, fee)?;
    send_v0(rpc, payer, "reprice + add", &finish, &alts)?;
    Ok(true)
}
