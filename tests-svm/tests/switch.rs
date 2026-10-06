//! Quote switching end to end against real Raydium (ours) and Orca (route) pools.

use chamelequote::{damm, math, state::Phase};
use chamelequote_tests::*;

fn assert_close(a: f64, b: f64, tol: f64, what: &str) {
    assert!((a / b - 1.0).abs() <= tol, "{what}: {a} vs {b} (tol {tol})");
}

#[test]
fn launch_lays_all_supply_single_sided() {
    for order in ORDERS {
        let env = Env::launched_ordered(order);
        let c = env.config();
        assert_eq!(c.active_quote, env.usdc);
        assert_eq!(c.switch.phase, Phase::Idle);
        // Everything but rounding dust sits in the pool.
        let vault = chamelequote::whirlpool::ata(&authority(), &env.mint, &anchor_spl::token::spl_token::ID);
        // The 1 ppm liquidity margin stays behind as dust and rides along to the next lay-out.
        assert!(env.balance(&vault) < SUPPLY / 100_000, "left in vault: {}", env.balance(&vault));
        assert_close(env.index_usd(), 1e-4, 1e-6, "launch price");
    }
}

/// Both pool orientations: our token as token A everywhere, then as token B everywhere.
const ORDERS: [Option<bool>; 2] = [Some(true), Some(false)];

/// A market with trading on it: users bought $50k of the token with USDC.
fn traded() -> (Env, Kp) {
    traded_ordered(None)
}

fn traded_ordered(index_first: Option<bool>) -> (Env, Kp) {
    let mut env = Env::launched_ordered(index_first);
    let whale = env.funded();
    env.buy(&whale, 50_000 * 1_000_000).unwrap();
    // The pool average lags real moves on purpose (~5 min time constant): after a 2x pump it
    // takes ~25 minutes to come within the 3% bound that lets a switch pull.
    env.keep(1800);
    (env, whale)
}

#[test]
fn switch_to_xstock_keeps_the_index_value() {
    for order in ORDERS {
        let (mut env, whale) = traded_ordered(order);
        let before_usd = env.index_usd();
        let supply = env.supply();
        let x = env.x;

        env.request(&whale, x).unwrap();
        assert_eq!(env.config().switch.phase, Phase::Requested);
        assert_eq!(env.balance(&escrow()), BURN);

        let hops = env.crank().unwrap();
        assert_eq!(hops, 1, "USDC -> X is one hop");

        let c = env.config();
        assert_eq!(c.active_quote, x);
        assert_eq!(c.quote_changes, 1);
        assert_eq!(env.supply(), supply - BURN, "escrowed burn is burned on success");
        assert_eq!(env.balance(&escrow()), 0);
        // Value carried over: the swap costs the route fee (0.3% on a 64-spacing pool) on the backing,
        // which only moves the price a little since most value is unsold supply.
        assert_close(env.index_usd(), before_usd, 0.01, "index USD value across switch");

        // The backing is now X and buyers can trade against it.
        env.buy(&whale, 10 * 100_000_000).unwrap();
    }
}

#[test]
fn switch_via_two_hubs_takes_three_hops_and_back() {
    for order in ORDERS {
        let (mut env, whale) = traded_ordered(order);
        let usd0 = env.index_usd();
        let (x, y, usdc) = (env.x, env.y, env.usdc);

        env.request(&whale, x).unwrap();
        assert_eq!(env.crank().unwrap(), 1);
        env.keep(660);

        // X -> USDC -> WSOL -> Y
        env.request(&whale, y).unwrap();
        assert_eq!(env.crank().unwrap(), 3);
        assert_eq!(env.config().active_quote, y);
        assert_close(env.index_usd(), usd0, 0.02, "after X -> Y");
        env.keep(660);

        // Y -> WSOL -> USDC, landing in the existing (stale, empty) TOKEN/USDC pool: needs repricing.
        env.request(&whale, usdc).unwrap();
        assert_eq!(env.crank().unwrap(), 2);
        assert_eq!(env.config().active_quote, usdc);
        assert_close(env.index_usd(), usd0, 0.03, "after round trip");
        assert_eq!(env.config().quote_changes, 3);
    }
}

#[test]
fn fee_share_goes_to_recipient() {
    for order in ORDERS {
        let (mut env, whale) = traded_ordered(order);
        let fee_ata = chamelequote::whirlpool::ata(&env.fee_recipient.pubkey(), &env.usdc, &anchor_spl::token::spl_token::ID);
        let x = env.x;
        env.request(&whale, x).unwrap();
        env.crank().unwrap();
        // $50k bought through a 1% pool: ~$400 of fees after DAMM's 20% cut, 10% to the
        // recipient (claimed right before the pull).
        let got = env.balance(&fee_ata) as f64 / 1e6;
        assert!((35.0..=60.0).contains(&got), "fee share: ${got}");
    }
}

#[test]
fn request_needs_warm_averages_and_valid_target() {
    let (mut env, whale) = traded();
    let (x, usdc) = (env.x, env.usdc);
    assert!(env.request(&whale, usdc).unwrap_err().contains("SameQuote"));

    // Averages go stale without the keeper.
    env.warp(600);
    assert!(env.request(&whale, x).unwrap_err().contains("StalePrice"));
    // One poke restarts them but they must warm up again.
    env.poke_quotes();
    assert!(env.request(&whale, x).unwrap_err().contains("StalePrice"));
    env.keep(660);
    env.request(&whale, x).unwrap();
    // Only one switch at a time.
    let y = env.y;
    assert!(env.request(&whale, y).unwrap_err().contains("WrongPhase"));
}

#[test]
fn pull_refuses_a_manipulated_pool() {
    let (mut env, whale) = traded();
    let x = env.x;
    env.request(&whale, x).unwrap();
    // Someone pumps the pool hard right before the pull.
    let pumper = env.funded();
    env.buy(&pumper, 200_000 * 1_000_000).unwrap();
    let cranker = env.funded();
    let ix = env.pull_ix(&cranker.pubkey());
    let err = env.send(&[ix], &[&cranker]).unwrap_err();
    assert!(err.contains("PoolManipulated"), "{err}");
}

#[test]
fn hop_refuses_a_manipulated_route_pool() {
    let (mut env, whale) = traded();
    let x = env.x;
    env.request(&whale, x).unwrap();
    let cranker = env.funded();
    let ix = env.pull_ix(&cranker.pubkey());
    env.send(&[ix], &[&cranker]).unwrap();

    // Push X/USDC 10% off its average, then try the hop.
    let pusher = env.funded();
    let pool = env.x_pool;
    let usdc_is_a = env.pool(&pool).mint_a == env.usdc;
    let usdc = env.usdc;
    env.mint_to(&pusher.pubkey(), &usdc, 2_000_000 * 1_000_000);
    env.user_swap(&pusher, &pool, usdc_is_a, 2_000_000 * 1_000_000).unwrap();
    let ix = env.hop_ix(x);
    let err = env.send(&[ix], &[&cranker]).unwrap_err();
    assert!(err.contains("RouteOffAverage"), "{err}");
}

#[test]
fn hop_rejects_the_wrong_route() {
    let (mut env, whale) = traded();
    let x = env.x;
    env.request(&whale, x).unwrap();
    let cranker = env.funded();
    let ix = env.pull_ix(&cranker.pubkey());
    env.send(&[ix], &[&cranker]).unwrap();
    // Holding USDC, target X: going through Y's pool is not a step toward X.
    let y = env.y;
    let ix = env.hop_ix(y);
    let err = env.send(&[ix], &[&cranker]).unwrap_err();
    assert!(err.contains("WrongHop"), "{err}");
}

#[test]
fn crank_cannot_redirect_funds() {
    let (mut env, whale) = traded();
    let x = env.x;
    env.request(&whale, x).unwrap();
    let thief = env.funded();
    let mut ix = env.pull_ix(&thief.pubkey());
    // Swap the program's USDC account (ours_b or ours_a) for the thief's own.
    let usdc = env.usdc;
    let thief_usdc = env.ensure_ata(&thief.pubkey(), &usdc);
    let ours_usdc = chamelequote::whirlpool::ata(&authority(), &usdc, &anchor_spl::token::spl_token::ID);
    for m in ix.accounts.iter_mut() {
        if m.pubkey == ours_usdc {
            m.pubkey = thief_usdc;
        }
    }
    let err = env.send(&[ix], &[&thief]).unwrap_err();
    assert!(err.contains("WrongTokenAccount"), "{err}");
}

#[test]
fn abort_refunds_and_lands_where_the_backing_is() {
    for order in ORDERS {
        let (mut env, whale) = traded_ordered(order);
        let whale_token = chamelequote::whirlpool::ata(&whale.pubkey(), &env.mint, &anchor_spl::token::spl_token::ID);
        let usd0 = env.index_usd();
        let y = env.y;

        // Request -> abort before anything happened.
        let bal = env.balance(&whale_token);
        env.request(&whale, y).unwrap();
        let ix = env.abort_ix();
        assert!(env.send(&[ix], &[&env.admin.insecure_clone()]).unwrap_err().contains("NotExpired"));
        env.warp(601);
        let ix = env.abort_ix();
        let admin = env.admin.insecure_clone();
        env.send(&[ix], &[&admin]).unwrap();
        assert_eq!(env.balance(&whale_token), bal, "refunded");
        assert_eq!(env.config().switch.phase, Phase::Idle);

        // Request -> pull -> one hop (USDC -> WSOL) -> stall -> abort: backing lands in WSOL.
        env.keep(660);
        env.request(&whale, y).unwrap();
        let cranker = env.funded();
        let ix = env.pull_ix(&cranker.pubkey());
        env.send(&[ix], &[&cranker]).unwrap();
        env.poke_quotes();
        let via = env.next_via();
        let ix = env.hop_ix(via);
        env.send(&[ix], &[&cranker]).unwrap();
        assert_eq!(env.config().switch.holding, env.wsol);
        env.warp(601);
        let ix = env.abort_ix();
        env.send(&[ix], &[&admin]).unwrap();
        assert_eq!(env.balance(&whale_token), bal, "refunded again");
        env.crank().unwrap();
        let c = env.config();
        assert_eq!(c.active_quote, env.wsol);
        assert_eq!(c.quote_changes, 0, "aborted switches burn nothing");
        // Supply untouched: launch-time supply minus nothing.
        assert_eq!(env.supply(), SUPPLY);
        // Value roughly kept (keep() pokes moved time, not prices).
        let usd = env.index_usd();
        assert!((usd / usd0 - 1.0).abs() < 0.02, "{usd} vs {usd0}");
    }
}

#[test]
fn a_pool_someone_else_created_is_refused_before_anything_burns() {
    let (mut env, whale) = traded();
    let (x, mint) = (env.x, env.mint);
    // Griefer pre-creates the TOKEN/X DAMM pool (the one address our X pool would have).
    let griefer = env.funded();
    env.mint_to(&griefer.pubkey(), &x, 1_000_000);
    env.buy(&griefer, 1_000_000).unwrap();
    let pool = chamelequote::instructions::expected_pool(&env.config(), &x);
    let sides = damm::Sides {
        pool,
        mint_a: mint,
        mint_b: x,
        program_a: anchor_spl::token::spl_token::ID,
        program_b: anchor_spl::token::spl_token::ID,
        ours_a: chamelequote::whirlpool::ata(&griefer.pubkey(), &mint, &anchor_spl::token::spl_token::ID),
        ours_b: chamelequote::whirlpool::ata(&griefer.pubkey(), &x, &anchor_spl::token::spl_token::ID),
        vault_a: damm::vault_address(&pool, &mint),
        vault_b: damm::vault_address(&pool, &x),
    };
    let nft = Kp::new();
    let p = math::sqrt_ratio(1_000, 1).unwrap();
    let ix = damm::create_pool_ix(griefer.pubkey(), nft.pubkey(), &sides, p / 2, p, 1u128 << 70);
    env.send(&[ix], &[&griefer, &nft]).unwrap();
    let whale_token = chamelequote::whirlpool::ata(&whale.pubkey(), &mint, &anchor_spl::token::spl_token::ID);
    let before = env.balance(&whale_token);
    let err = env.request(&whale, x).unwrap_err();
    assert!(err.contains("ForeignPool"), "{err}");
    assert_eq!(env.balance(&whale_token), before, "nothing burned");
}

/// Prints value carried across each switch and compute used per crank step; fails if any step
/// gets within 30% of the 1.4M compute limit.
#[test]
fn report_value_and_compute() {
    let (mut env, whale) = traded();
    let (x, y, usdc) = (env.x, env.y, env.usdc);
    let usd0 = env.index_usd();
    for (name, target) in [("USDC->X", x), ("X->Y", y), ("Y->USDC", usdc)] {
        let before = env.index_usd();
        env.request(&whale, target).unwrap();
        env.crank().unwrap();
        let after = env.index_usd();
        eprintln!(
            "{name}: index ${before:.3e} -> ${after:.3e} ({:+.3}%), steps {:?}",
            (after / before - 1.0) * 100.0,
            env.crank_cu
        );
        for (_, cu) in &env.crank_cu {
            assert!(*cu < 1_000_000, "step too close to the compute limit: {cu}");
        }
        env.keep(660);
    }
    eprintln!("round trip: {:+.3}%", (env.index_usd() / usd0 - 1.0) * 100.0);
}

#[test]
fn keeper_cancels_a_switch_left_past_its_deadline() {
    let (mut env, whale) = traded();
    let whale_token = chamelequote::whirlpool::ata(&whale.pubkey(), &env.mint, &anchor_spl::token::spl_token::ID);
    let bal = env.balance(&whale_token);
    let (x, usdc) = (env.x, env.usdc);
    env.request(&whale, x).unwrap();
    env.warp(601); // keeper was down
    env.crank().unwrap();
    assert_eq!(env.crank_cu.first().map(|s| s.0), Some("abort"));
    let c = env.config();
    assert_eq!(c.switch.phase, Phase::Idle);
    assert_eq!(c.active_quote, usdc, "nothing was pulled, so the coin stays put");
    assert_eq!(env.balance(&whale_token), bal, "burn refunded");
}

#[test]
fn abort_refund_cannot_be_redirected() {
    let (mut env, whale) = traded();
    let x = env.x;
    env.request(&whale, x).unwrap();
    env.warp(601);
    let thief = env.funded();
    let mint = env.mint;
    let thief_token = env.ensure_ata(&thief.pubkey(), &mint);
    let whale_token = chamelequote::whirlpool::ata(&whale.pubkey(), &env.mint, &anchor_spl::token::spl_token::ID);
    let mut ix = env.abort_ix();
    for m in ix.accounts.iter_mut() {
        if m.pubkey == whale_token {
            m.pubkey = thief_token;
        }
    }
    let err = env.send(&[ix], &[&thief]).unwrap_err();
    assert!(err.contains("WrongTokenAccount"), "{err}");
    assert_eq!(env.balance(&thief_token), 0);
}

#[test]
fn claim_fees_pays_the_recipient_without_touching_liquidity() {
    let (mut env, _whale) = traded();
    let fee_ata = chamelequote::whirlpool::ata(&env.fee_recipient.pubkey(), &env.usdc, &anchor_spl::token::spl_token::ID);
    let pool = env.config().active_pool;
    let (liq, price) = (env.our_pool(&pool).liquidity, env.our_pool(&pool).sqrt_price);
    let cranker = env.funded();
    let action = env.crank_as(&cranker.pubkey()).claim_fees().unwrap();
    env.send(&action.ixs, &[&cranker]).unwrap();
    // $50k bought through a 1% pool: ~$400 of fees after DAMM's 20% cut, 10% share here.
    let got = env.balance(&fee_ata) as f64 / 1e6;
    assert!((35.0..=60.0).contains(&got), "fee share: ${got}");
    assert_eq!((env.our_pool(&pool).liquidity, env.our_pool(&pool).sqrt_price), (liq, price), "liquidity untouched");
    // Claiming again right away pays nothing new.
    let action = env.crank_as(&cranker.pubkey()).claim_fees().unwrap();
    env.send(&action.ixs, &[&cranker]).unwrap();
    assert_eq!(env.balance(&fee_ata) as f64 / 1e6, got);
}

/// The keeper's fast path: prep (pokes, accounts, fee claim), then every step packed into as
/// few transactions as the 64-account and 64-entry trace limits allow, sent back to back.
/// Returns the number of transactions.
fn fast_switch(env: &mut Env, target: anchor_lang::prelude::Pubkey) -> usize {
    let cranker = env.funded();
    let est = env.crank_as(&cranker.pubkey()).estimate().unwrap();
    for a in env.crank_as(&cranker.pubkey()).fast_prep(&est) {
        let mut s: Vec<&Kp> = vec![&cranker];
        s.extend(a.signers.iter());
        env.send(&a.ixs, &s).unwrap_or_else(|e| panic!("{}: {e}", a.label));
    }
    let steps = env.crank_as(&cranker.pubkey()).fast_steps(&est, true);
    let txs = chamelequote_crank::pack(steps, &cranker.pubkey());
    for (i, (labels, ixs)) in txs.iter().enumerate() {
        let k = chamelequote_crank::unique_accounts(ixs, &cranker.pubkey());
        env.send(ixs, &[&cranker]).unwrap_or_else(|e| panic!("fast tx {i} {labels:?}: {e}"));
        eprintln!("  tx {i} {labels:?}: {k} accounts, {} trace, {} CU", env.last_trace, env.last_cu);
        assert!(k <= 64, "{k} accounts");
        assert!(env.last_trace <= 64, "trace {}", env.last_trace);
        assert!(env.last_cu < 1_400_000);
    }
    let c = env.config();
    assert_eq!(c.switch.phase, Phase::Idle);
    assert_eq!(c.active_quote, target);
    txs.len()
}

#[test]
fn fast_switches_pack_into_few_transactions() {
    for order in ORDERS {
        let (mut env, whale) = traded_ordered(order);
        let (x, wsol, y, usdc) = (env.x, env.wsol, env.y, env.usdc);
        let usd0 = env.index_usd();
        let plan = [
            ("USDC->X (new pool)", x, 1),
            ("X->SOL (new pool, 2 hops)", wsol, 1),
            ("SOL->Y (new pool)", y, 1),
            ("Y->X (revisit, 3 hops)", x, 1),
            ("X->USDC (revisit, no sentinel yet)", usdc, 1),
            ("USDC->X (revisit, 1 hop)", x, 1),
        ];
        for (name, target, txs) in plan {
            env.request(&whale, target).unwrap();
            eprintln!("{name}:");
            let n = fast_switch(&mut env, target);
            assert!(n <= txs, "{name}: {n} transactions");
            env.keep(660);
        }
        assert_close(env.index_usd(), usd0, 0.05, "after six fast switches");
    }
}

#[test]
fn revisits_reprice_through_the_sentinel() {
    for order in ORDERS {
        let (mut env, whale) = traded_ordered(order);
        let (x, usdc) = (env.x, env.usdc);
        // USDC -> X: a new pool, seeded with a sentinel.
        env.request(&whale, x).unwrap();
        env.crank().unwrap();
        let x_pool = env.config().active_pool;
        env.keep(660);
        // X -> USDC: back to the launch pool, which had no sentinel (no backing at launch).
        env.request(&whale, usdc).unwrap();
        env.crank().unwrap();
        let labels: Vec<_> = env.crank_cu.iter().map(|s| s.0).collect();
        assert!(labels.contains(&"seed") && labels.contains(&"reprice"), "{labels:?}");
        // The X pool keeps only its sentinel, so it stays tradeable.
        assert!(env.our_pool(&x_pool).liquidity > 0, "sentinel left in the X pool");
        // Move X so the way back lands at a different price, then go back.
        let pusher = env.funded();
        let pool = env.x_pool;
        let usdc_is_a = env.pool(&pool).mint_a == usdc;
        env.mint_to(&pusher.pubkey(), &usdc, 2_000_000 * 1_000_000);
        env.user_swap(&pusher, &pool, usdc_is_a, 2_000_000 * 1_000_000).unwrap();
        env.keep(1800);
        let usd0 = env.index_usd();
        env.request(&whale, x).unwrap();
        env.crank().unwrap();
        let labels: Vec<_> = env.crank_cu.iter().map(|s| s.0).collect();
        assert!(labels.contains(&"reprice") && !labels.contains(&"seed"), "{labels:?}");
        assert_eq!(env.config().active_pool, x_pool);
        assert_close(env.index_usd(), usd0, 0.02, "back in the X pool at the new price");
        env.buy(&whale, 1_000_000).unwrap();
    }
}

#[test]
fn pool_has_liquidity_at_its_price_after_a_switch() {
    for order in ORDERS {
        let (mut env, whale) = traded_ordered(order);
        let x = env.x;
        env.request(&whale, x).unwrap();
        env.crank().unwrap();
        let p = env.our_pool(&env.config().active_pool);
        // More than the sentinel: the backing (or index) position is in range.
        assert!(p.liquidity > 10_000_000, "in-range liquidity {}", p.liquidity);
        // A small sell and a small buy both go through right away.
        env.sell(&whale, 1_000 * 1_000_000).unwrap();
        env.buy(&whale, 100_000).unwrap();
    }
}

#[test]
fn dust_backing_still_seeds_and_comes_back() {
    for order in ORDERS {
        // Tokens to burn handed out before launch, then a single $0.01 buy: the backing is dust.
        let mut env = Env::new_ordered(order);
        let (x, usdc) = (env.x, env.usdc);
        let whale = env.funded();
        env.give(&whale.pubkey(), BURN * 2);
        env.keep(660);
        env.launch(usdc, math::sqrt_ratio(1, 10_000).unwrap()).unwrap();
        env.crank().unwrap();
        env.buy(&whale, 10_000).unwrap();
        env.keep(660);
        env.request(&whale, x).unwrap();
        env.crank().unwrap();
        let x_pool = env.config().active_pool;
        assert!(env.crank_as(&authority()).has_sentinel(&x_pool), "dust backing still funds a sentinel");
        env.keep(660);
        // Back to the launch pool, which has no sentinel (nothing to fund one at launch).
        env.request(&whale, usdc).unwrap();
        env.crank().unwrap();
        assert_eq!(env.config().active_quote, usdc);
    }
}

#[test]
fn switches_into_and_out_of_a_token_2022_stock() {
    let (mut env, whale) = traded();
    let usd0 = env.index_usd();
    let usdc = env.usdc;
    // A $400 stock on token-2022 with xStock-style extensions, routed via a deep Orca pool.
    let xt = env.create_xstock_mint(8);
    let route = env.init_pool(&xt, &usdc, TS_ROUTE, pool_sqrt(&xt, &usdc, 4, 1));
    let (stock, dollars) = (50_000 * 100_000_000u64, 20_000_000 * 1_000_000u64);
    let (a, b) = if env.pool(&route).mint_a == xt { (stock, dollars) } else { (dollars, stock) };
    env.add_lp(&route, a, b);
    env.list_quote(xt, Some((usdc, route)));
    env.extra_quotes.push(xt);
    env.keep(660);

    env.request(&whale, xt).unwrap();
    env.crank().unwrap();
    let c = env.config();
    assert_eq!(c.active_quote, xt);
    assert_eq!(env.program_of(&xt), anchor_spl::token_2022::ID);
    assert_close(env.index_usd(), usd0, 0.01, "into the token-2022 stock");
    env.buy(&whale, 10 * 100_000_000).unwrap();
    env.sell(&whale, 1_000_000 * 1_000_000).unwrap();

    env.keep(1800);
    let usd1 = env.index_usd();
    env.request(&whale, usdc).unwrap();
    env.crank().unwrap();
    assert_eq!(env.config().active_quote, usdc);
    assert_close(env.index_usd(), usd1, 0.02, "and back");
}
