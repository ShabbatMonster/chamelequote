//! Moving the live coin from its Raydium pool to Meteora DAMM v2. Starts from the program exactly
//! as deployed before the move (fixtures/chamelequote_mainnet_v2.so), launched and traded in a
//! Raydium pool by the crank of that build, then upgrades it in place to this build and switches.

use anchor_lang::prelude::Pubkey;
use chamelequote::{damm, math, raydium as ray, state::Phase, whirlpool as wp, ID as PROGRAM_ID};
use chamelequote_tests::*;

const V2: &str = "fixtures/chamelequote_mainnet_v2.so";
const V3: &str = "../target/deploy-dev/chamelequote.so";

/// The deployed build's crank reads accounts through its own Ledger trait.
struct Old<'a>(&'a Env);

impl crank_raydium::Ledger for Old<'_> {
    fn account(&self, key: &Pubkey) -> Option<(Pubkey, Vec<u8>)> {
        chamelequote_crank::Ledger::account(self.0, key)
    }
    fn now(&self) -> i64 {
        self.0.now()
    }
}

/// Runs the deployed build's crank until the switch (or launch) in flight is done.
fn old_crank(env: &mut Env) {
    let cranker = env.funded();
    for _ in 0..20 {
        let Some(a) = crank_raydium::Crank::new(&Old(env), cranker.pubkey()).next() else { return };
        let mut signers: Vec<&Kp> = vec![&cranker];
        signers.extend(a.signers.iter());
        env.send(&a.ixs, &signers).unwrap_or_else(|e| panic!("old {}: {e}", a.label));
    }
    panic!("old crank did not finish");
}

#[test]
fn upgrade_moves_the_liquidity_from_raydium_to_damm() {
    for (order, fast) in [(Some(true), false), (Some(false), true)] {
        // The deployed build, launched into a Raydium TOKEN/USDC pool and traded on.
        let mut env = Env::new_with(V2, order, (CLMM_CONFIG, 120));
        env.keep(660);
        let (usdc, x) = (env.usdc, env.x);
        env.launch(usdc, math::sqrt_ratio(1, 10_000).unwrap()).unwrap();
        old_crank(&mut env);
        let ray_pool = env.config().active_pool;
        assert_eq!(env.svm.get_account(&ray_pool).unwrap().owner, ray::CLMM_ID);
        let whale = env.funded();
        env.buy(&whale, 50_000 * 1_000_000).unwrap();
        env.keep(1800);
        let usd0 = env.index_usd();
        let supply = env.supply();

        // Upgrade in place. The keeper keeps poking the Raydium pool until the move.
        env.svm.add_program_from_file(PROGRAM_ID, V3).unwrap();
        env.keep(120);

        // First switch: pulled from Raydium (all three positions, fees paid out), laid into DAMM.
        let fee_usdc = wp::ata(&env.fee_recipient.pubkey(), &usdc, &anchor_spl::token::spl_token::ID);
        env.request(&whale, x).unwrap();
        if fast {
            // The keeper's fast path, as it will run on mainnet.
            let cranker = env.funded();
            let est = env.crank_as(&cranker.pubkey()).estimate().unwrap();
            for a in env.crank_as(&cranker.pubkey()).fast_prep(&est) {
                env.send(&a.ixs, &[&cranker]).unwrap_or_else(|e| panic!("{}: {e}", a.label));
            }
            let steps = env.crank_as(&cranker.pubkey()).fast_steps(&est, true);
            for (labels, ixs) in chamelequote_crank::pack(steps, &cranker.pubkey()) {
                let k = chamelequote_crank::unique_accounts(&ixs, &cranker.pubkey());
                env.send(&ixs, &[&cranker]).unwrap_or_else(|e| panic!("{labels:?}: {e}"));
                eprintln!("  {labels:?}: {k} accounts, {} trace, {} CU", env.last_trace, env.last_cu);
                assert!(k <= 64 && env.last_trace <= 64);
            }
        } else {
            env.crank().unwrap();
        }
        let c = env.config();
        assert_eq!((c.active_quote, c.switch.phase), (x, Phase::Idle));
        assert_eq!(env.svm.get_account(&c.active_pool).unwrap().owner, damm::DAMM_ID);
        assert_eq!(env.ray_pool(&ray_pool).liquidity, 0, "nothing of ours left on Raydium");
        assert!(env.balance(&fee_usdc) > 0, "Raydium fees paid out at the pull");
        assert_eq!(env.supply(), supply - BURN);
        assert!((env.index_usd() / usd0 - 1.0).abs() < 0.01, "value carried: {} vs {usd0}", env.index_usd());
        assert!(env.crank_as(&Pubkey::default()).has_sentinel(&c.active_pool));

        // And on from there, DAMM to DAMM (a new USDC pool, not the Raydium one).
        env.keep(660);
        env.request(&whale, usdc).unwrap();
        env.crank().unwrap();
        let c = env.config();
        assert_eq!(c.active_quote, usdc);
        assert_eq!(env.svm.get_account(&c.active_pool).unwrap().owner, damm::DAMM_ID);
        assert!((env.index_usd() / usd0 - 1.0).abs() < 0.02, "round trip: {} vs {usd0}", env.index_usd());
        env.buy(&whale, 1_000 * 1_000_000).unwrap();
        env.sell(&whale, 1_000_000 * 1_000_000).unwrap();
    }
}

#[test]
fn a_new_pool_costs_the_requester_its_rent_and_a_known_one_costs_nothing() {
    let mut env = Env::launched();
    let whale = env.funded();
    env.buy(&whale, 50_000 * 1_000_000).unwrap();
    env.keep(1800);
    let (x, usdc) = (env.x, env.usdc);
    let fee = env.fee_recipient.pubkey();
    let lamports = |env: &Env, k: &Pubkey| env.svm.get_account(k).map(|a| a.lamports).unwrap_or(0);

    let (w0, f0) = (lamports(&env, &whale.pubkey()), lamports(&env, &fee));
    env.request(&whale, x).unwrap();
    assert_eq!(lamports(&env, &fee) - f0, chamelequote::state::NEW_POOL_FEE_LAMPORTS);
    assert!(w0 - lamports(&env, &whale.pubkey()) >= chamelequote::state::NEW_POOL_FEE_LAMPORTS);
    env.crank().unwrap();
    env.keep(660);

    // Back to the USDC pool, which exists: no fee.
    let f1 = lamports(&env, &fee);
    env.request(&whale, usdc).unwrap();
    assert_eq!(lamports(&env, &fee), f1);
}
