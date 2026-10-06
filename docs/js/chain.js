// Everything that talks to Solana: account decoding, the two user instructions, recent activity.
// @solana/web3.js is loaded lazily so demo mode works without the network.

import { CONFIG } from "./config.js";

let web3;
export async function loadWeb3() {
  web3 ??= await import(CONFIG.WEB3_URL);
  return web3;
}

const TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ATA_PROGRAM = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const METADATA_PROGRAM = "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s";

// Anchor discriminators: sha256("global:<ix>" / "account:<T>" / "event:<E>")[0..8]
const IX_RENAME = [98, 63, 129, 146, 24, 170, 185, 45];
const IX_REQUEST_SWITCH = [227, 11, 252, 67, 174, 8, 46, 204];
const EVENTS = {
  "148,232,32,179,231,9,232,103": "Renamed",
  "7,223,168,139,165,147,157,170": "Switched",
  "230,210,43,21,34,12,248,207": "SwitchRequested",
  "249,26,148,197,141,190,145,33": "SwitchAborted",
};

export const PHASES = ["Idle", "Requested", "Swapping", "Repricing"];
export const Q64 = 2 ** 64;

// ---------------------------------------------------------------------------------------------
// Byte reader (no Buffer dependency)

class Reader {
  constructor(bytes, offset = 0) {
    this.b = bytes;
    this.o = offset;
    this.v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  }
  u8() { return this.b[this.o++]; }
  u16() { const x = this.v.getUint16(this.o, true); this.o += 2; return x; }
  u64() { const x = this.v.getBigUint64(this.o, true); this.o += 8; return x; }
  i64() { const x = this.v.getBigInt64(this.o, true); this.o += 8; return x; }
  u128() { const lo = this.u64(); const hi = this.u64(); return (hi << 64n) | lo; }
  key() { const k = new web3.PublicKey(this.b.slice(this.o, this.o + 32)); this.o += 32; return k; }
  str() { const n = this.v.getUint32(this.o, true); this.o += 4; const s = new TextDecoder().decode(this.b.slice(this.o, this.o + n)); this.o += n; return s.replace(/\0+$/, ""); }
  ema() { return { sqrt: this.u128(), lastTs: this.i64(), streakStart: this.i64() }; }
}

// ---------------------------------------------------------------------------------------------
// PDAs

const pk = (s) => new web3.PublicKey(s);
const programId = () => pk(CONFIG.PROGRAM_ID);
const pda = (seeds, program = programId()) => web3.PublicKey.findProgramAddressSync(seeds, program)[0];
const enc = (s) => new TextEncoder().encode(s);

export const configPda = () => pda([enc("config")]);
export const authorityPda = () => pda([enc("authority")]);
export const escrowPda = () => pda([enc("escrow")]);
export const quotePda = (mint) => pda([enc("quote"), mint.toBytes()]);
export const ata = (owner, mint) => pda([owner.toBytes(), pk(TOKEN_PROGRAM).toBytes(), mint.toBytes()], pk(ATA_PROGRAM));
export const metadataPda = (mint) => pda([enc("metadata"), pk(METADATA_PROGRAM).toBytes(), mint.toBytes()], pk(METADATA_PROGRAM));

// ---------------------------------------------------------------------------------------------
// Decoders (layouts follow programs/chamelequote/src/state.rs)

export function decodeConfig(data) {
  const r = new Reader(data, 8);
  const c = {
    admin: r.key(), mint: r.key(), burnAmount: r.u64(), renames: r.u64(), quoteChanges: r.u64(), totalBurned: r.u64(),
    bump: r.u8(), authorityBump: r.u8(), escrowBump: r.u8(),
    clmmConfig: r.key(), tickSpacing: r.u16(), usdc: r.key(), wsol: r.key(), feeRecipient: r.key(), feeShareBps: r.u16(),
    maxPriceMoveBps: r.u16(), maxRouteDeviationBps: r.u16(), maxSlippageBps: r.u16(),
    activeQuote: r.key(), activePool: r.key(), floorSqrt: r.u128(), poolEma: r.ema(),
  };
  c.switch = {
    phase: PHASES[r.u8()], requester: r.key(), target: r.key(), holding: r.key(), deadline: r.i64(), escrowed: r.u64(),
    startAmount: r.u64(), realisedMin: r.u64(), emaRateSqrt: r.u128(), oldIndexSqrt: r.u128(),
    targetIndexSqrt: r.u128(), targetFloorSqrt: r.u128(),
  };
  return c;
}

export function decodeQuote(data) {
  const r = new Reader(data, 8);
  return {
    mint: r.key(), tokenProgram: r.key(), decimals: r.u8(), enabled: r.u8() === 1, bump: r.u8(),
    hub: r.key(), routePool: r.key(), ema: r.ema(),
  };
}

const ORCA_PROGRAM = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

/** The coin's pool: Raydium CLMM, or the Orca Whirlpool it lived in before the move. Both store
 *  sqrt(token B / token A) with the mints sorted, so A/B mean token 0/1 on Raydium. */
export function decodePool(acc) {
  const r = new Reader(acc.data);
  if (acc.owner.toBase58() === ORCA_PROGRAM) {
    r.o = 49;
    const liquidity = r.u128();
    const sqrtPrice = r.u128();
    r.o = 101;
    const mintA = r.key(), vaultA = r.key();
    r.o = 181;
    const mintB = r.key(), vaultB = r.key();
    return { venue: "Orca Whirlpool", liquidity, sqrtPrice, mintA, vaultA, mintB, vaultB };
  }
  r.o = 73;
  const mintA = r.key(), mintB = r.key(), vaultA = r.key(), vaultB = r.key();
  r.o = 237;
  const liquidity = r.u128();
  const sqrtPrice = r.u128();
  return { venue: "Raydium CLMM", liquidity, sqrtPrice, mintA, vaultA, mintB, vaultB };
}

/** Metaplex metadata: key(1) update_authority(32) mint(32) name symbol uri. */
export function decodeMetadata(data) {
  const r = new Reader(data, 65);
  return { name: r.str(), symbol: r.str(), uri: r.str() };
}

const tokenAmount = (data) => new DataView(data.buffer, data.byteOffset).getBigUint64(64, true);

/** (x / 2^64)^2 for a Q64.64 sqrt price, as a float. */
export const priceOf = (sqrt) => { const s = Number(sqrt) / Q64; return s * s; };

// ---------------------------------------------------------------------------------------------
// Reads

export async function connect() {
  await loadWeb3();
  return new web3.Connection(CONFIG.RPC_URL, "confirmed");
}

// Free RPC tiers block large batches (publicnode: under 20 accounts), so fetch 10 at a time.
async function many(conn, keys) {
  const chunks = [];
  for (let i = 0; i < keys.length; i += 10) chunks.push(keys.slice(i, i + 10));
  return (await Promise.all(chunks.map((c) => conn.getMultipleAccountsInfo(c)))).flat();
}

/** Everything the page shows, in display units. */
export async function loadState(conn, wallet) {
  const mint = pk(CONFIG.MINT);
  const [cfgAcc, mdAcc, mintAcc] = await many(conn, [configPda(), metadataPda(mint), mint]);
  const config = decodeConfig(cfgAcc.data);
  const metadata = decodeMetadata(mdAcc.data);
  const supply = Number(new DataView(mintAcc.data.buffer, mintAcc.data.byteOffset).getBigUint64(36, true)) / 1e6;

  // Free RPCs refuse getProgramAccounts from browsers, so the listed mints come from quotes.json
  // (written by `keeper list --out`) and their entries are fetched directly.
  const { quotes: mints } = await (await fetch("quotes.json", { cache: "no-cache" })).json();
  const entryAccs = await many(conn, mints.map((m) => quotePda(pk(m))));
  const quotes = entryAccs.filter(Boolean).map((a) => decodeQuote(a.data));
  const byMint = new Map(quotes.map((q) => [q.mint.toBase58(), q]));

  // USD per whole token, from the on-chain averages.
  const usdPerRaw = (q) => {
    if (q.hub.equals(q.mint)) return 1;
    const own = priceOf(q.ema.sqrt);
    const hub = byMint.get(q.hub.toBase58());
    return own * (hub && !hub.hub.equals(hub.mint) ? priceOf(hub.ema.sqrt) : 1);
  };
  const usdOf = (q) => usdPerRaw(q) * 10 ** (q.decimals - 6);

  const state = { config, metadata, supply, quotes, byMint, usdOf, launched: !config.activeQuote.equals(web3.PublicKey.default) };
  if (state.launched) {
    const active = byMint.get(config.activeQuote.toBase58());
    const [poolAcc] = await many(conn, [config.activePool]);
    const pool = decodePool(poolAcc);
    state.venue = pool.venue;
    const indexIsA = pool.mintA.equals(mint);
    const [va, vb] = await many(conn, [pool.vaultA, pool.vaultB]);
    const [indexVault, quoteVault] = indexIsA ? [va, vb] : [vb, va];
    // quote per index, whole units
    let p = priceOf(pool.sqrtPrice);
    if (!indexIsA) p = 1 / p;
    const priceQuote = p * 10 ** (6 - active.decimals);
    const quoteUsd = usdOf(active);
    state.active = active;
    state.price = { quote: priceQuote, usd: priceQuote * quoteUsd };
    state.floor = { quote: priceOf(config.floorSqrt) * 10 ** (6 - active.decimals) };
    state.floor.usd = state.floor.quote * quoteUsd;
    state.backing = { quote: Number(tokenAmount(quoteVault.data)) / 10 ** active.decimals };
    state.backing.usd = state.backing.quote * quoteUsd;
    state.inPool = Number(tokenAmount(indexVault.data)) / 1e6;
  }
  if (wallet) {
    const [acc] = await many(conn, [ata(wallet, mint)]);
    state.balance = acc ? Number(tokenAmount(acc.data)) / 1e6 : 0;
  }
  return state;
}

/**
 * Renames, switches and aborts from recent transactions. Renames always touch the metadata
 * account and switch steps that matter touch the escrow, so those two histories cover everything
 * without wading through the keeper's price updates.
 */
export async function loadHappenings(conn, limit = 15) {
  const mint = pk(CONFIG.MINT);
  const lists = await Promise.all(
    [escrowPda(), metadataPda(mint)].map((a) => conn.getSignaturesForAddress(a, { limit }).catch(() => [])),
  );
  const seen = new Set();
  const sigs = lists
    .flat()
    .filter((s) => !s.err && !seen.has(s.signature) && seen.add(s.signature))
    .sort((a, b) => (b.blockTime ?? 0) - (a.blockTime ?? 0))
    .slice(0, limit);
  const out = [];
  for (const s of sigs) {
    const tx = await conn.getTransaction(s.signature, { maxSupportedTransactionVersion: 0 });
    for (const line of tx?.meta?.logMessages ?? []) {
      if (!line.startsWith("Program data: ")) continue;
      const bytes = Uint8Array.from(atob(line.slice(14)), (c) => c.charCodeAt(0));
      const name = EVENTS[Array.from(bytes.slice(0, 8)).join(",")];
      if (!name) continue;
      const r = new Reader(bytes, 8);
      const ev = { name, sig: s.signature, time: s.blockTime };
      if (name === "Renamed") Object.assign(ev, { user: r.key(), newName: r.str(), symbol: r.str(), uri: r.str() });
      if (name === "Switched") Object.assign(ev, { user: r.key(), quote: r.key() });
      if (name === "SwitchRequested") Object.assign(ev, { user: r.key(), from: r.key(), to: r.key() });
      if (name === "SwitchAborted") Object.assign(ev, { user: r.key(), wanted: r.key(), landed: r.key() });
      out.push(ev);
    }
  }
  return out;
}

// ---------------------------------------------------------------------------------------------
// Writes

const meta = (pubkey, isSigner, isWritable) => ({ pubkey, isSigner, isWritable });
const borshStr = (s) => {
  const b = enc(s);
  const out = new Uint8Array(4 + b.length);
  new DataView(out.buffer).setUint32(0, b.length, true);
  out.set(b, 4);
  return out;
};
const concat = (...parts) => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) { out.set(p, o); o += p.length; }
  return out;
};

export function renameIx(user, name, symbol, uri) {
  const mint = pk(CONFIG.MINT);
  return new web3.TransactionInstruction({
    programId: programId(),
    keys: [
      meta(user, true, false),
      meta(configPda(), false, true),
      meta(mint, false, true),
      meta(ata(user, mint), false, true),
      meta(authorityPda(), false, false),
      meta(metadataPda(mint), false, true),
      meta(pk(METADATA_PROGRAM), false, false),
      meta(pk(TOKEN_PROGRAM), false, false),
    ],
    data: concat(Uint8Array.from(IX_RENAME), borshStr(name), borshStr(symbol), borshStr(uri)),
  });
}

export function requestSwitchIx(user, state, target) {
  const mint = pk(CONFIG.MINT);
  return new web3.TransactionInstruction({
    programId: programId(),
    keys: [
      meta(user, true, false),
      meta(configPda(), false, true),
      meta(mint, false, false),
      meta(ata(user, mint), false, true),
      meta(escrowPda(), false, true),
      meta(quotePda(state.config.activeQuote), false, false),
      meta(quotePda(target), false, false),
      meta(quotePda(state.config.wsol), false, false),
      meta(pk(TOKEN_PROGRAM), false, false),
    ],
    data: Uint8Array.from(IX_REQUEST_SWITCH),
  });
}

/**
 * Signs with the connected wallet (see wallet.js), sends, and waits for confirmation by polling
 * over HTTP (free RPCs often don't serve the websocket web3.js would otherwise use, which leaves
 * the page hanging after the transaction has already landed). `onSent(signature)` fires as soon
 * as the transaction is submitted. Returns the signature.
 */
export async function sendIx(conn, wallet, ix, onSent = () => {}) {
  const tx = new web3.Transaction().add(web3.ComputeBudgetProgram.setComputeUnitLimit({ units: 200_000 }), ix);
  tx.feePayer = new web3.PublicKey(wallet.address);
  const { blockhash, lastValidBlockHeight } = await conn.getLatestBlockhash();
  tx.recentBlockhash = blockhash;
  const signed = await wallet.sign(tx);
  const sig = await conn.sendRawTransaction(signed, { maxRetries: 5 });
  onSent(sig);
  for (;;) {
    const st = (await conn.getSignatureStatuses([sig])).value[0];
    if (st?.err) throw new Error(`Transaction failed on-chain: ${JSON.stringify(st.err)}`);
    if (st?.confirmationStatus === "confirmed" || st?.confirmationStatus === "finalized") return sig;
    if ((await conn.getBlockHeight()) > lastValidBlockHeight) {
      throw new Error("The transaction expired without landing, so nothing was burned. Try again.");
    }
    await new Promise((r) => setTimeout(r, 1500));
  }
}

/** Uploads bytes to IPFS; returns the gateway URL. */
export async function ipfsUpload(blob, filename) {
  const form = new FormData();
  form.append("file", blob, filename);
  const res = await fetch(`${CONFIG.IPFS_API}?pin=true&cid-version=0`, { method: "POST", body: form });
  if (!res.ok) throw new Error(`IPFS upload failed (${res.status})`);
  const { Hash } = await res.json();
  return CONFIG.IPFS_GATEWAY + Hash;
}

