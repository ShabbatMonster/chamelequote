//! Quote switching end to end against real Orca pools.

use chamelequote::{math, state::Phase};
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
        // $50k bought through a 1% pool: ~$500 of fees, 10% of it to the recipient.
        let got = env.balance(&fee_ata) as f64 / 1e6;
        assert!((40.0..=60.0).contains(&got), "fee share: ${got}");
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
fn reprice_moves_a_preexisting_pool_created_at_a_silly_price() {
    let mut most_reprices = 0;
    for order in ORDERS {
        let (mut env, whale) = traded_ordered(order);
        let x = env.x;
        let usd0 = env.index_usd();
        // Griefer pre-creates TOKEN/X at a silly price (1000 X per token), with no liquidity.
        let mint = env.mint;
        let silly = math::sqrt_ratio(1_000, 1).unwrap();
        let griefed = env.init_pool(&mint, &x, TS_OURS, silly);
        env.request(&whale, x).unwrap();
        env.crank().unwrap();
        assert_eq!(env.config().active_pool, griefed);
        let reprices = env.crank_cu.iter().filter(|s| s.0 == "reprice").count();
        most_reprices = most_reprices.max(reprices);
        assert_close(env.index_usd(), usd0, 0.01, "repriced to fair");
    }
    // Moving the price down through empty ticks stops at each three-array window, so at least one
    // orientation must have needed several calls (Orca crosses empty space upward more freely).
    assert!(most_reprices >= 3, "windowed reprice not exercised: {most_reprices}");
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
