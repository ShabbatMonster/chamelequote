//! Moving the live coin from its Orca pool to Raydium. Starts from the program exactly as it is
//! deployed on mainnet (fixtures/chamelequote_mainnet_v1.so, laid out in an Orca pool), upgrades
//! it in place to this build, points new pools at Raydium, and switches.

use anchor_lang::{
    prelude::Pubkey,
    solana_program::instruction::{AccountMeta, Instruction},
    InstructionData, ToAccountMetas,
};
use chamelequote::{accounts, instruction, math, raydium as ray, state::Phase, whirlpool as wp, ID as PROGRAM_ID};
use chamelequote_tests::*;

const V1: &str = "fixtures/chamelequote_mainnet_v1.so";
const V2: &str = "../target/deploy-dev/chamelequote.so";
const TS_ORCA: u16 = 128;

/// The v1 `add` (Orca positions), with tick arrays for the launch lay-out: everything single
/// sided above the price (the backing range is empty at launch).
fn v1_launch_add(env: &mut Env, pool: Pubkey) {
    let c = env.config();
    let p = env.pool(&pool);
    let ts = p.tick_spacing as i32;
    let max_t = math::max_usable_tick(ts);
    let below = math::align_down(p.tick_current, ts);
    let index_is_a = c.index_is_a(&c.switch.target);
    let index_range = if index_is_a { (below + ts, max_t) } else { (-max_t, below) };
    let starts = [math::tick_array_start(index_range.0, ts), math::tick_array_start(index_range.1, ts)];
    env.init_tick_arrays(&pool, &starts);
    let slot = |i: u8, (lo, hi): (i32, i32)| {
        let mint = chamelequote::instructions::position_mint_address(&pool, i).0;
        (
            mint,
            wp::position_address(&mint),
            wp::ata(&authority(), &mint, &wp::TOKEN_2022_ID),
            wp::tick_array_address(&pool, math::tick_array_start(lo, ts)),
            wp::tick_array_address(&pool, math::tick_array_start(hi, ts)),
        )
    };
    let (s0, s1) = (slot(0, index_range), slot(1, index_range));
    let admin = env.admin.insecure_clone();
    let s = env.sides_for(&pool, &authority());
    let mut metas = vec![
        AccountMeta::new(admin.pubkey(), true),
        AccountMeta::new(config_pda(), false),
        AccountMeta::new_readonly(authority(), false),
    ];
    metas.extend(
        accounts::PoolSides {
            whirlpool: pool,
            mint_a: s.mint_a,
            mint_b: s.mint_b,
            token_program_a: s.program_a,
            token_program_b: s.program_b,
            ours_a: s.owner_a,
            ours_b: s.owner_b,
            vault_a: s.vault_a,
            vault_b: s.vault_b,
        }
        .to_account_metas(None),
    );
    metas.extend(
        accounts::OrcaPositions {
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
        .to_account_metas(None),
    );
    for (k, w) in [
        (escrow(), true),
        (env.mint, true),
        (anchor_spl::token::spl_token::ID, false),
        (wp::TOKEN_2022_ID, false),
        (anchor_lang::solana_program::system_program::ID, false),
        (wp::ATA_PROGRAM_ID, false),
        (wp::MEMO_ID, false),
        (wp::NFT_UPDATE_AUTH, false),
        (wp::WHIRLPOOL_ID, false),
    ] {
        metas.push(if w { AccountMeta::new(k, false) } else { AccountMeta::new_readonly(k, false) });
    }
    let ix = Instruction { program_id: PROGRAM_ID, accounts: metas, data: instruction::Add {}.data() };
    env.send(&[ix], &[&admin]).unwrap();
}

#[test]
fn upgrade_moves_the_liquidity_from_orca_to_raydium() {
    for order in [Some(true), Some(false)] {
        // v1, launched into an Orca TOKEN/USDC pool the way it was on mainnet.
        let mut env = Env::new_with(V1, order, (WP_CONFIG, TS_ORCA));
        env.keep(660);
        let (usdc, x) = (env.usdc, env.x);
        let index_sqrt = math::sqrt_ratio(1, 10_000).unwrap();
        env.launch(usdc, index_sqrt).unwrap();
        let index_is_a = env.config().index_is_a(&usdc);
        let mint = env.mint;
        let orca_pool = env.init_pool(&mint, &usdc, TS_ORCA, math::flip(index_sqrt, !index_is_a));
        env.ensure_ata(&authority(), &usdc);
        v1_launch_add(&mut env, orca_pool);
        let c = env.config();
        assert_eq!((c.active_pool, c.switch.phase), (orca_pool, Phase::Idle));

        // Trading on Orca.
        let whale = env.funded();
        env.mint_to(&whale.pubkey(), &usdc, 50_000 * 1_000_000);
        env.user_swap(&whale, &orca_pool, !index_is_a, 50_000 * 1_000_000).unwrap();
        env.keep(1800);
        let usd0 = env.index_usd();
        let supply = env.supply();

        // Upgrade in place; pools from now on are Raydium 1% pools.
        env.svm.add_program_from_file(PROGRAM_ID, V2).unwrap();
        let admin = env.admin.insecure_clone();
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: accounts::AdminConfig { admin: admin.pubkey(), config: config_pda() }.to_account_metas(None),
            data: instruction::SetPoolVenue { clmm_config: CLMM_CONFIG, tick_spacing: TS_OURS }.data(),
        };
        env.send(&[ix], &[&admin]).unwrap();
        // The keeper keeps poking the Orca pool until the move.
        env.keep(120);

        // First switch: pulled from Orca (fees paid out as before), laid into Raydium.
        let fee_usdc = wp::ata(&env.fee_recipient.pubkey(), &usdc, &anchor_spl::token::spl_token::ID);
        env.request(&whale, x).unwrap();
        if order == Some(true) {
            env.crank().unwrap();
        } else {
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
        }
        let c = env.config();
        assert_eq!(c.active_quote, x);
        assert_eq!(c.active_pool, chamelequote::instructions::expected_pool(&c, &x));
        assert_eq!(env.svm.get_account(&c.active_pool).unwrap().owner, ray::CLMM_ID);
        assert_eq!(env.pool(&orca_pool).liquidity, 0, "nothing left on Orca");
        assert!(env.balance(&fee_usdc) > 0, "Orca fees paid out at the pull");
        assert_eq!(env.supply(), supply - BURN);
        assert!((env.index_usd() / usd0 - 1.0).abs() < 0.01, "value carried: {} vs {usd0}", env.index_usd());

        // And on from there, Raydium to Raydium (a new USDC pool, not the Orca one).
        env.keep(660);
        env.request(&whale, usdc).unwrap();
        env.crank().unwrap();
        let c = env.config();
        assert_eq!(c.active_quote, usdc);
        assert_ne!(c.active_pool, orca_pool);
        assert!((env.index_usd() / usd0 - 1.0).abs() < 0.02, "round trip: {} vs {usd0}", env.index_usd());
        env.buy(&whale, 1_000 * 1_000_000).unwrap();
    }
}
