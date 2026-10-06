//! Launch, metadata and quote registry.

use anchor_lang::{prelude::Pubkey, InstructionData, ToAccountMetas};
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_spl::token::spl_token;
use chamelequote::{accounts, instruction, metaplex::{metadata_address, TOKEN_METADATA_ID}, ID as PROGRAM_ID};
use chamelequote_tests::*;

struct Metadata {
    update_authority: Pubkey,
    name: String,
    symbol: String,
    uri: String,
    is_mutable: bool,
}

/// key(1) update_authority(32) mint(32), then borsh strings (Metaplex pads them with NULs).
fn read_metadata(env: &Env) -> Metadata {
    let data = env.svm.get_account(&metadata_address(&env.mint)).unwrap().data;
    let mut o = 65;
    let mut s = || {
        let len = u32::from_le_bytes(data[o..o + 4].try_into().unwrap()) as usize;
        let v = String::from_utf8(data[o + 4..o + 4 + len].to_vec()).unwrap().trim_end_matches('\0').to_string();
        o += 4 + len;
        v
    };
    let (name, symbol, uri) = (s(), s(), s());
    o += 2; // seller fee
    o += if data[o] == 1 { 1 + 4 + 34 * u32::from_le_bytes(data[o + 1..o + 5].try_into().unwrap()) as usize } else { 1 };
    o += 1; // primary_sale_happened
    Metadata { update_authority: Pubkey::try_from(&data[1..33]).unwrap(), name, symbol, uri, is_mutable: data[o] == 1 }
}

fn rename_ix(env: &Env, user: &Pubkey, user_token: Pubkey, name: &str, symbol: &str, uri: &str) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::Rename {
            user: *user,
            config: config_pda(),
            mint: env.mint,
            user_token,
            authority: authority(),
            metadata: metadata_address(&env.mint),
            token_metadata_program: TOKEN_METADATA_ID,
            token_program: spl_token::ID,
        }
        .to_account_metas(None),
        data: instruction::Rename { name: name.into(), symbol: symbol.into(), uri: uri.into() }.data(),
    }
}

#[test]
fn initialize_mints_to_vault_creates_metadata_and_revokes_mint_authority() {
    let env = Env::new();
    let mint = anchor_spl::token::spl_token::state::Mint::unpack_from(&env.svm.get_account(&env.mint).unwrap().data);
    assert!(mint.mint_authority.is_none());
    assert!(mint.freeze_authority.is_none());
    assert_eq!(env.supply(), SUPPLY);
    assert_eq!(env.balance(&chamelequote::whirlpool::ata(&authority(), &env.mint, &spl_token::ID)), SUPPLY);

    let md = read_metadata(&env);
    assert_eq!((md.name.as_str(), md.symbol.as_str(), md.uri.as_str()), ("Chameleon", "CHAM", "https://example.com/cham.json"));
    assert_eq!(md.update_authority, authority(), "program PDA must be the only update authority");
    assert!(md.is_mutable);

    let cfg = env.config();
    assert_eq!(cfg.admin, env.admin.pubkey());
    assert_eq!(cfg.burn_amount, BURN);
    assert_eq!(cfg.active_quote, Pubkey::default());
}

trait UnpackFrom {
    fn unpack_from(d: &[u8]) -> Self;
}
impl UnpackFrom for spl_token::state::Mint {
    fn unpack_from(d: &[u8]) -> Self {
        <Self as anchor_lang::solana_program::program_pack::Pack>::unpack(d).unwrap()
    }
}

#[test]
fn rename_burns_exactly_burn_amount_and_rewrites_metadata() {
    let mut env = Env::new();
    let user = env.funded();
    let ata = env.give(&user.pubkey(), 2 * BURN + 5);

    let uri = "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
    let ix = rename_ix(&env, &user.pubkey(), ata, "Tesla Lizard", "TLIZ", uri);
    env.send(&[ix], &[&user]).unwrap();

    assert_eq!(env.balance(&ata), BURN + 5);
    assert_eq!(env.supply(), SUPPLY - BURN);
    let md = read_metadata(&env);
    assert_eq!((md.name.as_str(), md.symbol.as_str(), md.uri.as_str()), ("Tesla Lizard", "TLIZ", uri));
    assert_eq!(md.update_authority, authority());
    let cfg = env.config();
    assert_eq!((cfg.renames, cfg.total_burned), (1, BURN));

    let ix = rename_ix(&env, &user.pubkey(), ata, "Again", "AGN", "https://a.b/c");
    env.send(&[ix], &[&user]).unwrap();
    assert_eq!(env.balance(&ata), 5);
    assert_eq!(env.config().renames, 2);
}

#[test]
fn rename_rejects_bad_metadata_without_burning() {
    let mut env = Env::new();
    let user = env.funded();
    let ata = env.give(&user.pubkey(), BURN);
    let long_name = "x".repeat(33);
    let long_uri = format!("https://{}", "a".repeat(200));
    let cases: [(&str, &str, &str, &str); 8] = [
        ("", "OK", "https://x.y", "BadName"),
        (&long_name, "OK", "https://x.y", "BadName"),
        ("Ok", "", "https://x.y", "BadSymbol"),
        ("Ok", "TOOLONGTICK", "https://x.y", "BadSymbol"),
        ("Ok", "OK", "http://insecure.example", "BadUri"),
        ("Ok", "OK", "javascript:alert(1)", "BadUri"),
        ("Ok", "OK", &long_uri, "BadUri"),
        ("Ok", "A\nB", "https://x.y", "ControlChars"),
    ];
    for (name, symbol, uri, expected) in cases {
        let ix = rename_ix(&env, &user.pubkey(), ata, name, symbol, uri);
        let err = env.send(&[ix], &[&user]).unwrap_err();
        assert!(err.contains(expected), "{name:?}/{symbol:?}/{uri:?}: expected {expected}, got:\n{err}");
    }
    assert_eq!(env.balance(&ata), BURN);
    assert_eq!(env.config().renames, 0);
}

#[test]
fn rename_fails_below_burn_amount() {
    let mut env = Env::new();
    let user = env.funded();
    let ata = env.give(&user.pubkey(), BURN - 1);
    let ix = rename_ix(&env, &user.pubkey(), ata, "X", "X", "https://x.y");
    let err = env.send(&[ix], &[&user]).unwrap_err();
    assert!(err.to_lowercase().contains("insufficient funds"), "{err}");
    assert_eq!(read_metadata(&env).name, "Chameleon");
}

#[test]
fn rename_cannot_burn_someone_elses_tokens() {
    let mut env = Env::new();
    let victim = env.funded();
    let victim_ata = env.give(&victim.pubkey(), BURN);
    let thief = env.funded();
    let ix = rename_ix(&env, &thief.pubkey(), victim_ata, "X", "X", "https://x.y");
    let err = env.send(&[ix], &[&thief]).unwrap_err();
    assert!(err.contains("ConstraintTokenOwner"), "{err}");
    assert_eq!(env.balance(&victim_ata), BURN);
}

#[test]
fn metadata_cannot_be_updated_except_through_the_program() {
    // UpdateMetadataAccountV2 signed by the admin (not the PDA) must fail: nobody holds a key for it.
    let mut env = Env::new();
    let admin = env.admin.insecure_clone();
    let mut data = vec![15u8, 1];
    for s in ["Rug", "RUG", "https://evil.example"] {
        data.extend((s.len() as u32).to_le_bytes());
        data.extend(s.as_bytes());
    }
    data.extend([0u8; 8]); // fee (2), creators, collection, uses, then 3x None
    let ix = Instruction {
        program_id: TOKEN_METADATA_ID,
        accounts: vec![AccountMeta::new(metadata_address(&env.mint), false), AccountMeta::new_readonly(admin.pubkey(), true)],
        data,
    };
    let err = env.send(&[ix], &[&admin]).unwrap_err();
    assert!(err.contains("Update Authority given does not match"), "{err}");
    assert_eq!(read_metadata(&env).name, "Chameleon");
}

#[test]
fn quote_registry_rules() {
    let mut env = Env::new();
    let admin = env.admin.insecure_clone();
    let x = env.quote(&env.x);
    assert_eq!((x.hub, x.route_pool, x.decimals, x.enabled), (env.usdc, env.x_pool, 8, true));
    let usdc = env.quote(&env.usdc);
    assert!(usdc.is_root());

    // Route pool must pair the quote with its hub.
    let z = env.create_mint(6);
    let (usdc_m, x_pool, y_pool) = (env.usdc, env.x_pool, env.y_pool);
    let ix = env.list_quote_ix(&admin.pubkey(), z, Some((usdc_m, x_pool)));
    assert!(env.send(&[ix], &[&admin]).unwrap_err().contains("BadRoute"));
    // Hub must be USDC or WSOL.
    let x_m = env.x;
    let ix = env.list_quote_ix(&admin.pubkey(), z, Some((x_m, y_pool)));
    assert!(env.send(&[ix], &[&admin]).unwrap_err().contains("BadHub"));
    // Our own token cannot be a quote.
    let mint = env.mint;
    let ix = env.list_quote_ix(&admin.pubkey(), mint, Some((usdc_m, x_pool)));
    assert!(env.send(&[ix], &[&admin]).unwrap_err().contains("QuoteIsSelf"));

    // Only the admin manages the list; renouncing freezes it.
    let set_enabled = |admin: Pubkey, enabled: bool| Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::AdminQuote { admin, config: config_pda(), quote: quote_pda(&x_m) }.to_account_metas(None),
        data: instruction::SetQuoteEnabled { enabled }.data(),
    };
    let rando = env.funded();
    assert!(env.send(&[set_enabled(rando.pubkey(), false)], &[&rando]).unwrap_err().contains("NotAdmin"));
    env.send(&[set_enabled(admin.pubkey(), false)], &[&admin]).unwrap();
    assert!(!env.quote(&x_m).enabled);
    let renounce = Instruction {
        program_id: PROGRAM_ID,
        accounts: accounts::AdminConfig { admin: admin.pubkey(), config: config_pda() }.to_account_metas(None),
        data: instruction::SetAdmin { new_admin: Pubkey::default() }.data(),
    };
    env.send(&[renounce], &[&admin]).unwrap();
    assert!(env.send(&[set_enabled(admin.pubkey(), true)], &[&admin]).unwrap_err().contains("NotAdmin"));
}
