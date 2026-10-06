//! Independent checks against Jupiter's public API (it sees every Solana DEX):
//!
//! - Before a switch is sent, the keeper simulates it and compares what the backing would turn
//!   into with Jupiter's best quote for the same amount. A route that comes out much worse (a
//!   drained or high-fee route pool, or one being gamed) is not sent; the request then times out
//!   and the burn is refunded.
//! - Every few minutes it values each enabled quote's route pool and compares its price with
//!   Jupiter's, and disables quotes whose pool has gone thin or off-market (the admin can turn
//!   them back on with `keeper allow` once they recover).
//!
//! The program's own checks (price averages, slippage bounds) still apply on-chain; these sit on
//! top, using information the program can't see.

use std::time::Duration;

use anchor_lang::prelude::Pubkey;
use chamelequote::{math, state::QuoteEntry};
use chamelequote_crank::{set_quote_enabled_ix, Action, Crank};
use solana_keypair::Keypair;
use solana_signer::Signer;

use crate::{first_line, log, send, Rpc};

const API: &str = "https://lite-api.jup.ag";
/// A switch whose route delivers less than Jupiter's quote by more than this is not sent.
pub const MAX_ROUTE_SHORTFALL: f64 = 0.03;
/// Route pools below this much liquidity, or priced further than this from Jupiter, get their
/// quote disabled.
const MIN_ROUTE_TVL_USD: f64 = 10_000.0;
const MAX_ROUTE_PRICE_GAP: f64 = 0.05;

fn get(url: &str) -> Result<serde_json::Value, String> {
    ureq::get(url)
        .timeout(Duration::from_secs(8))
        .call()
        .map_err(|e| format!("Jupiter: {e}"))?
        .into_json()
        .map_err(|e| format!("Jupiter: {e}"))
}

/// Jupiter's best output for `amount` of `input`, in raw units of `output`.
pub fn quote(input: &Pubkey, output: &Pubkey, amount: u64) -> Result<u64, String> {
    let v = get(&format!("{API}/swap/v1/quote?inputMint={input}&outputMint={output}&amount={amount}&slippageBps=50"))?;
    v["outAmount"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| format!("Jupiter: no route ({})", v["error"]))
}

/// (USD price, decimals) per mint, for up to 50 mints.
fn prices(mints: &[Pubkey]) -> Result<std::collections::HashMap<Pubkey, (f64, u32)>, String> {
    let mut out = std::collections::HashMap::new();
    for chunk in mints.chunks(50) {
        let ids: Vec<String> = chunk.iter().map(|m| m.to_string()).collect();
        let v = get(&format!("{API}/price/v3?ids={}", ids.join(",")))?;
        for m in chunk {
            let e = &v[m.to_string()];
            if let (Some(p), Some(d)) = (e["usdPrice"].as_f64(), e["decimals"].as_u64()) {
                out.insert(*m, (p, d as u32));
            }
        }
    }
    Ok(out)
}

/// Compares a simulated switch with Jupiter: `pulled` raw units of `from` came out of the old
/// pool and `delivered` raw units of `to` went into the new one.
pub fn check_switch(from: &Pubkey, to: &Pubkey, pulled: u64, delivered: u64) -> Result<(), String> {
    if pulled == 0 {
        return Ok(());
    }
    let best = match quote(from, to, pulled) {
        Ok(b) => b,
        Err(e) => {
            // No independent reference: the on-chain checks still guard the swap.
            log(&format!("switch check skipped: {}", first_line(&e)));
            return Ok(());
        }
    };
    // Dust: a handful of raw units can't be compared meaningfully.
    if best < 1_000 {
        return Ok(());
    }
    let ratio = delivered as f64 / best as f64;
    if ratio < 1.0 - MAX_ROUTE_SHORTFALL {
        return Err(format!(
            "route would deliver {delivered} where Jupiter gives {best} ({:.1}% short); not sending",
            (1.0 - ratio) * 100.0
        ));
    }
    log(&format!("switch check: route delivers {:.2}% of Jupiter's best", ratio * 100.0));
    Ok(())
}

/// Values each enabled quote's route pool with Jupiter's prices and disables the quotes whose
/// pool has gone thin or off-market (never the USDC/SOL hubs).
/// With `dry`, only reports (the `keeper routes` command).
pub fn check_routes(rpc: &Rpc, payer: &Keypair, quotes: &[QuoteEntry], fee: u64, dry: bool) {
    let crank = Crank::new(rpc, payer.pubkey());
    let c = crank.config();
    let live: Vec<&QuoteEntry> = quotes.iter().filter(|q| q.enabled && !q.is_root() && q.mint != c.wsol).collect();
    let mut mints: Vec<Pubkey> = live.iter().map(|q| q.mint).collect();
    mints.extend([c.usdc, c.wsol]);
    let px = match prices(&mints) {
        Ok(p) => p,
        Err(e) => return log(&format!("route check skipped: {}", first_line(&e))),
    };
    for q in live {
        let Some(pool) = crank.pool(&q.route_pool) else { continue };
        let (Some(&(ua, da)), Some(&(ub, db))) = (px.get(&pool.mint_a), px.get(&pool.mint_b)) else {
            log(&format!("route check: no Jupiter price for {}", q.mint));
            continue;
        };
        let tvl = crank.balance(&pool.vault_a) as f64 / 10f64.powi(da as i32) * ua
            + crank.balance(&pool.vault_b) as f64 / 10f64.powi(db as i32) * ub;
        // Pool price of A in USD (B per A, raw) against Jupiter's.
        let s = pool.sqrt_price as f64 / math::Q64 as f64;
        let implied_a = s * s * 10f64.powi(da as i32 - db as i32) * ub;
        let gap = (implied_a / ua - 1.0).abs();
        let healthy = tvl >= MIN_ROUTE_TVL_USD && gap <= MAX_ROUTE_PRICE_GAP;
        if dry {
            println!("{} route {} liquidity ${tvl:>12.0} price gap {:>5.2}% {}", q.mint, q.route_pool, gap * 100.0, if healthy { "ok" } else { "WOULD DISABLE" });
        }
        if healthy || dry {
            continue;
        }
        let why = format!("route pool {} has ${tvl:.0} of liquidity, price {:.1}% off Jupiter", q.route_pool, gap * 100.0);
        if c.switch.target == q.mint || c.switch.holding == q.mint {
            log(&format!("route check: {} looks unhealthy ({why}) but a switch involves it; leaving it", q.mint));
            continue;
        }
        let action = Action { label: "disable quote", ixs: vec![set_quote_enabled_ix(&payer.pubkey(), &q.mint, false)], signers: vec![] };
        match send(rpc, payer, &action, fee) {
            Ok(_) => log(&format!("disabled quote {}: {why}", q.mint)),
            Err(e) => log(&format!("could not disable quote {}: {}", q.mint, first_line(&e))),
        }
    }
}
