import { CONFIG } from "./config.js";
import { installCursors } from "./cursors.js";
import { initMenus, initCombo } from "./widgets.js";
import { demoView, DEMO_WALLET } from "./demo.js";
import { QUOTE_META } from "./quotes-meta.js";
import * as chain from "./chain.js";
import { listWallets, rememberWallet, lastWallet } from "./wallet.js";

const $ = (id) => document.getElementById(id);
const DEMO = !CONFIG.MINT || location.hash === "#demo";

let view = null; // what the page shows (see demo.js for the shape)
let conn = null;
let wallet = null; // { conn (from wallet.js), address, balance }

// ---------------------------------------------------------------------------------------------
// Formatting

const num = (n, dp = 0) => Number(n).toLocaleString("en-US", { maximumFractionDigits: dp, minimumFractionDigits: 0 });
const sig = (n) => (n >= 1 ? num(n, 2) : Number(n).toLocaleString("en-US", { maximumSignificantDigits: 4 }));
const usd = (n) => "$" + (n >= 1000 ? num(n) : n >= 1 ? num(n, 2) : sig(n));
const short = (s) => (s.length > 10 ? s.slice(0, 4) + "…" + s.slice(-4) : s);
const ago = (t) => {
  const s = Math.max(0, Date.now() / 1000 - t);
  if (s < 90) return "just now";
  if (s < 5400) return `${Math.round(s / 60)} min ago`;
  if (s < 129600) return `${Math.round(s / 3600)} hr ago`;
  return `${Math.round(s / 86400)} days ago`;
};
const ipfsHttp = (u) => (u?.startsWith("ipfs://") ? CONFIG.IPFS_GATEWAY + u.slice(7) : u);

function say(el, text, kind = "", sig = null) {
  el.textContent = text;
  el.className = "msg" + (kind ? " " + kind : "");
  if (sig) {
    const a = document.createElement("a");
    a.href = CONFIG.EXPLORER_TX + sig;
    a.target = "_blank";
    a.rel = "noopener";
    a.textContent = "View on Solscan";
    el.append(" ", a);
  }
}

// ---------------------------------------------------------------------------------------------
// Rendering

let combo;

function render() {
  const v = view;
  document.title = `${v.coin.name} ($${v.coin.symbol}) · Totality`;
  $("coin-name").textContent = v.coin.name;
  $("coin-symbol").textContent = v.coin.symbol;
  $("coin-image").hidden = !v.coin.image;
  $("coin-blank").hidden = !!v.coin.image;
  if (v.coin.image) $("coin-image").src = v.coin.image;
  $("paired-with").textContent = v.launched ? v.active.symbol : "not launched yet";

  const digits = String(Math.round(v.totalBurned)).padStart(10, "0");
  $("burn-counter").innerHTML = [...digits].map((d) => `<span>${d}</span>`).join("");

  $("marquee-text").textContent =
    `*** Welcome to the home of ${v.coin.name} ($${v.coin.symbol})! ***   ` +
    (v.launched ? `Now paired with ${v.active.symbol} at ${usd(v.price.usd)} a token   ***   ` : "") +
    `${num(v.totalBurned)} tokens burned so far   ***   ` +
    `Burn ${num(v.burnAmount)} to rename it or to change what it trades against   ***`;

  if (v.launched) {
    const qs = v.active.symbol;
    $("st-price").textContent = `${sig(v.price.quote)} ${qs}`;
    $("st-usd").textContent = usd(v.price.usd);
    $("st-mcap").textContent = usd(v.price.usd * v.supply);
    $("st-backing").textContent = `${num(v.backing.quote, 4)} ${qs} (${usd(v.backing.usd)})`;
    $("st-floor").textContent = `${sig(v.floor.quote)} ${qs} (${usd(v.floor.usd)})`;
  }
  $("st-supply").textContent = `${num(v.supply)} (${num(v.inPool ?? 0)} in the pool)`;
  $("st-counts").textContent = `${num(v.renames)} renames, ${num(v.quoteChanges)} quote changes`;
  $("st-ca").textContent = v.coin.mint;

  $("switch-cost").textContent = num(v.burnAmount);
  $("rename-cost").textContent = num(v.burnAmount);
  for (const el of document.querySelectorAll(".coin-sym")) el.textContent = "$" + v.coin.symbol;
  renderBalance();

  const opts = v.quotes
    .filter((q) => q.enabled && (!v.launched || q.mint !== v.active.mint))
    .sort((a, b) => GROUP_ORDER.indexOf(a.group) - GROUP_ORDER.indexOf(b.group) || a.symbol.localeCompare(b.symbol))
    .map((q) => ({ value: q.mint, label: q.symbol, detail: q.name, group: q.group }));
  combo.setOptions(opts, opts.some((o) => o.value === combo.value) ? combo.value : null);

  renderHappenings();
  renderLinks();
  renderSwitch();
  $("last-updated").textContent = `Last updated ${new Date().toLocaleTimeString()}.`;
}

const GROUP_ORDER = ["Stocks", "Pre-IPO", "Cash", "Solana", "Leveraged", "Backpack", "Collectibles", "Memes & More"];

function renderBalance() {
  // The wallet can connect before the coin data has loaded; render() calls this again afterwards.
  const symbol = view?.coin?.symbol ?? "";
  $("switch-balance").textContent = wallet ? `${num(wallet.balance)} ${symbol}`.trim() : "connect a wallet to see";
}

function renderHappenings() {
  const rows = view.happenings ?? [];
  $("happenings-note").textContent = view.demo ? "Sample entries. Real activity shows up here once the coin is live." : "";
  const body = $("happenings-body");
  body.innerHTML = "";
  if (!rows.length) {
    body.innerHTML = `<tr><td colspan="3">Nothing yet. Be the first!</td></tr>`;
    return;
  }
  for (const h of rows) {
    const tr = document.createElement("tr");
    if (view.demo) tr.className = "example";
    const when = document.createElement("td");
    when.textContent = h.ago;
    const who = document.createElement("td");
    who.innerHTML = "<code></code>";
    who.firstChild.textContent = h.user;
    const what = document.createElement("td");
    const tag = document.createElement("b");
    tag.className = `ev-${h.name}`;
    tag.textContent = (h.launch ? "Launched" : { Renamed: "Renamed", Switched: "Switched", SwitchRequested: "Requested", SwitchAborted: "Cancelled" }[h.name]) + ": ";
    what.append(tag, h.detail);
    if (h.sig) {
      const a = document.createElement("a");
      a.href = CONFIG.EXPLORER_TX + h.sig;
      a.target = "_blank";
      a.rel = "noopener";
      a.textContent = " [tx]";
      what.append(a);
    }
    tr.append(when, who, what);
    body.append(tr);
  }
}

function renderLinks() {
  const m = view.coin.mint;
  const pool = view.pool;
  // The pool changes with every quote switch; these links always point at the live one.
  $("st-pool").textContent = pool ?? "—";
  $("st-pool-pair").textContent = view.launched ? `${view.venue ?? "Meteora DAMM v2"}, ${view.coin.symbol} / ${view.active.symbol}` : "";
  const links = {
    axiom: pool && `https://axiom.trade/meme/${pool}`,
    fomo: `https://fomo.family/tokens/solana/${m}`,
    jup: `https://jup.ag/swap/${view.launched ? view.active.mint : "SOL"}-${m}`,
    pool:
      pool &&
      (view.venue === "Raydium CLMM"
        ? `https://raydium.io/swap/?inputMint=${view.active.mint}&outputMint=${m}`
        : `https://www.meteora.ag/dammv2/${pool}`),
    dex: `https://dexscreener.com/solana/${m}`,
    scan: `https://solscan.io/token/${m}`,
  };
  const set = (id, href) => {
    const a = $(id);
    if (!a) return;
    if (view.demo || !href) {
      a.removeAttribute("href");
      a.setAttribute("aria-disabled", "true");
      a.title = "Available after launch";
    } else {
      a.href = href;
      a.removeAttribute("aria-disabled");
      a.title = "";
    }
  };
  for (const [k, href] of Object.entries(links)) {
    set(`link-${k}`, href);
    set(`tl-${k}`, href);
  }
}

// ---------------------------------------------------------------------------------------------
// Switch progress

const STEP_DONE = { Requested: 1, Swapping: 2, Repricing: 3 };
let watching = null; // { target } while a switch we care about is in flight

function renderSwitch() {
  const sw = view.sw;
  const box = $("switch-progress");
  const inFlight = sw.phase !== "Idle";
  const done = inFlight ? STEP_DONE[sw.phase] : watching?.finished ? 5 : null;
  box.hidden = done == null;
  updateCooldown();
  if (done == null) return;
  $("switch-fill").style.width = `${(done / 5) * 100}%`;
  [...$("switch-steps").children].forEach((li, i) => {
    li.className = i < done ? "done" : i === done ? "now" : "";
  });
  if (inFlight && !watching) {
    const t = view.quotes.find((q) => q.mint === sw.target);
    say($("switch-msg"), `A switch to ${t?.symbol ?? short(sw.target)} is in progress. One at a time, please.`);
  }
}

// After a switch the coin's price average needs a few minutes of updates before the program
// allows the next one (it refuses requests until then, before anything is burned).
function updateCooldown() {
  if (!view) return;
  const inFlight = view.sw.phase !== "Idle";
  const left = view.opensAt ? Math.ceil(view.opensAt - Date.now() / 1000) : 0;
  const cooling = !inFlight && view.launched && left > 0;
  $("switch-go").disabled = inFlight || !view.launched || cooling;
  const note = $("switch-cooldown");
  note.hidden = !cooling;
  if (cooling) {
    const m = Math.floor(left / 60), s = String(left % 60).padStart(2, "0");
    note.textContent = `Next switch possible in ${m}:${s} (the price settles for a few minutes after every switch).`;
  }
}
setInterval(updateCooldown, 1000);

// ---------------------------------------------------------------------------------------------
// Wallet

function walletSay(text, err = false) {
  const el = $("wallet-msg");
  el.textContent = text;
  el.className = "wallet-msg" + (err ? " err" : "");
}

/** Rebuilt every time the menu opens, so wallets that load late still show up. */
function renderWalletMenu() {
  const menu = $("menu-wallet");
  menu.innerHTML = "";
  const item = (label, fn, icon) => {
    const b = document.createElement("button");
    b.type = "button";
    b.setAttribute("role", "menuitem");
    b.className = "wallet-item";
    if (icon) {
      const img = document.createElement("img");
      img.src = icon;
      img.alt = "";
      b.append(img);
    }
    b.append(label);
    b.addEventListener("click", fn);
    menu.append(b);
  };
  if (wallet) {
    item("Copy My Address", () => copy(wallet.address));
    item("Disconnect", () => {
      wallet.conn?.disconnect?.();
      wallet = null;
      rememberWallet(null);
      $("wallet-btn").textContent = "Connect Wallet";
      walletSay("");
      renderBalance();
    });
    return;
  }
  if (DEMO) {
    item("Demo Wallet", () => connectWallet(null));
    return;
  }
  const found = listWallets();
  if (!found.length) {
    const p = document.createElement("p");
    p.className = "menu-empty";
    p.textContent = "No Solana wallet found. Install Phantom, Solflare or Backpack, or open this page in your wallet app's browser.";
    menu.append(p);
    return;
  }
  for (const w of found) item(w.name, () => connectWallet(w), w.icon);
}

async function connectWallet(w, silent = false) {
  try {
    if (!w) {
      wallet = { conn: null, address: DEMO_WALLET.address, balance: DEMO_WALLET.balance };
    } else {
      if (!silent) walletSay(`Waiting for ${w.name}…`);
      const conn = await w.connect(silent);
      wallet = { conn, address: conn.address, balance: 0 };
      rememberWallet(w.name);
    }
    // Show the connection right away; the balance follows.
    $("wallet-btn").textContent = short(wallet.address);
    walletSay("");
    renderBalance();
    if (!DEMO) {
      walletSay("Loading balance…");
      refresh()
        .then(() => walletSay(""))
        .catch((e) => walletSay(`Connected, but loading the balance failed: ${String(e?.message ?? e).slice(0, 80)}`, true));
    }
  } catch (e) {
    if (silent) return; // not approved before: stay quiet
    const msg = String(e?.message ?? e);
    walletSay(/reject|denied|cancel/i.test(msg) ? "Connection cancelled." : `Could not connect: ${msg.slice(0, 80)}`, true);
  }
}

/** Reconnects the last wallet without a popup, if the user approved this site before. */
function autoReconnect() {
  const name = lastWallet();
  if (!name || DEMO) return;
  const tryIt = () => {
    const w = listWallets().find((x) => x.name === name);
    if (w && !wallet) connectWallet(w, true);
    return !!w;
  };
  if (!tryIt()) setTimeout(tryIt, 800); // some wallets announce themselves a beat after load
}

async function copy(text, fallbackId = "st-ca") {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Clipboard blocked: select the text so the user can copy it by hand.
    const r = document.createRange();
    r.selectNodeContents($(fallbackId));
    getSelection().removeAllRanges();
    getSelection().addRange(r);
  }
}

// ---------------------------------------------------------------------------------------------
// Actions

function busy(on) {
  document.body.classList.toggle("busy", on);
}

function needWallet(msgEl) {
  if (wallet) return true;
  say(msgEl, "Connect a wallet first (top right).", "err");
  return false;
}

function needTokens(msgEl) {
  if (wallet.balance >= view.burnAmount) return true;
  say(msgEl, `You need ${num(view.burnAmount)} ${view.coin.symbol} to do this. You have ${num(wallet.balance)}.`, "err");
  return false;
}

async function onSwitch() {
  const msg = $("switch-msg");
  if (!view) return say(msg, "Still loading the coin. Try again in a second.", "err");
  const target = view.quotes.find((q) => q.mint === combo.value);
  if (!target) return say(msg, "Pick a quote token from the list first.", "err");
  if (!needWallet(msg) || !needTokens(msg)) return;

  busy(true);
  $("switch-go").disabled = true;
  try {
    if (DEMO) {
      watching = { target: target.mint };
      say(msg, `Burning ${num(view.burnAmount)} and switching to ${target.symbol}…`);
      for (const phase of ["Requested", "Swapping", "Repricing"]) {
        view.sw = { phase, target: target.mint };
        renderSwitch();
        await new Promise((r) => setTimeout(r, 1100));
      }
      const before = view.active;
      view.sw = { phase: "Idle" };
      view.price.quote = (view.price.quote * before.usd) / target.usd;
      view.floor.quote = (view.floor.quote * before.usd) / target.usd;
      view.backing = { quote: view.backing.usd / target.usd, usd: view.backing.usd };
      view.active = target;
      view.totalBurned += view.burnAmount;
      view.supply -= view.burnAmount;
      view.quoteChanges += 1;
      wallet.balance -= view.burnAmount;
      view.happenings.unshift({ name: "Switched", user: short(wallet.address), detail: `now paired with ${target.symbol}`, ago: "just now" });
      watching.finished = true;
      render();
      say(msg, `Done. ${view.coin.symbol} now trades against ${target.symbol}.`, "ok");
    } else {
      say(msg, "Waiting for your wallet…");
      const { PublicKey } = await chain.loadWeb3();
      const ix = chain.requestSwitchIx(new PublicKey(wallet.address), view.raw, new PublicKey(target.mint));
      const sigTx = await chain.sendIx(conn, wallet.conn, ix, (sig) =>
        say(msg, "Sent! Waiting for the network to confirm…", "", sig),
      );
      watching = { target: target.mint, sig: sigTx };
      say(msg, `Burn received. Switching to ${target.symbol}; this takes about a minute.`, "", sigTx);
      await refresh().catch(() => {}); // the next poll catches up if the RPC is busy
      pollSwitch();
    }
  } catch (e) {
    say(msg, friendly(e), "err");
    $("switch-go").disabled = false;
  } finally {
    busy(false);
  }
}

async function pollSwitch() {
  const msg = $("switch-msg");
  for (let i = 0; i < 200 && watching; i++) {
    await new Promise((r) => setTimeout(r, 3000));
    try {
      await refresh();
    } catch {
      continue; // a busy public RPC; try again next time round
    }
    if (view.sw.phase === "Idle") {
      const landed = view.active;
      watching.finished = true;
      renderSwitch();
      if (landed.mint === watching.target) say(msg, `Done. ${view.coin.symbol} now trades against ${landed.symbol}.`, "ok");
      else say(msg, `The switch timed out and was cancelled. Your burn was refunded; the coin landed on ${landed.symbol}.`, "err");
      return;
    }
  }
}

function utf8Len(s) {
  return new TextEncoder().encode(s).length;
}

function checkRename(name, symbol) {
  if (!name || utf8Len(name) > 32) return "Name must be 1 to 32 characters.";
  if (!symbol || utf8Len(symbol) > 10) return "Ticker must be 1 to 10 characters.";
  if (/[\u0000-\u001f\u007f-\u009f]/.test(name + symbol)) return "Name and ticker can't contain control characters.";
  return null;
}

async function onRename(e) {
  e.preventDefault();
  const msg = $("rename-msg");
  const name = $("rn-name").value.trim();
  const symbol = $("rn-symbol").value.trim();
  const description = $("rn-desc").value.trim();
  const file = $("rn-image").files[0];
  const bad = checkRename(name, symbol);
  if (bad) return say(msg, bad, "err");
  if (!view) return say(msg, "Still loading the coin. Try again in a second.", "err");
  if (name === view.coin.name && symbol === view.coin.symbol && !file && !description) {
    return say(msg, "That's already the coin's name and ticker. Nothing was burned.", "err");
  }
  if (file && file.size > 2_000_000) return say(msg, "Pictures must be under 2 MB.", "err");
  if (!needWallet(msg) || !needTokens(msg)) return;

  busy(true);
  $("rename-go").disabled = true;
  try {
    if (DEMO) {
      say(msg, "Uploading picture…");
      await new Promise((r) => setTimeout(r, 900));
      say(msg, `Burning ${num(view.burnAmount)}…`);
      await new Promise((r) => setTimeout(r, 900));
      view.coin = { ...view.coin, name, symbol, image: file ? URL.createObjectURL(file) : view.coin.image };
      view.totalBurned += view.burnAmount;
      view.supply -= view.burnAmount;
      view.renames += 1;
      wallet.balance -= view.burnAmount;
      view.happenings.unshift({ name: "Renamed", user: short(wallet.address), detail: `renamed to ${name} ($${symbol})`, ago: "just now" });
      render();
    } else {
      let image = view.coin.image ?? "";
      if (file) {
        say(msg, "Uploading picture to IPFS…");
        image = await chain.ipfsUpload(file, file.name);
      }
      say(msg, "Uploading details to IPFS…");
      const links = CONFIG.LINKS;
      const json = new Blob(
        [JSON.stringify({ name, symbol, description, image, external_url: links.website, ...links, extensions: links })],
        { type: "application/json" },
      );
      const uri = await chain.ipfsUpload(json, "metadata.json");
      say(msg, "Waiting for your wallet…");
      const { PublicKey } = await chain.loadWeb3();
      const sig = await chain.sendIx(conn, wallet.conn, chain.renameIx(new PublicKey(wallet.address), name, symbol, uri), (s2) =>
        say(msg, "Sent! Waiting for the network to confirm…", "", s2),
      );
      happeningsCache.at = 0; // show the rename in Recent Happenings right away
      await refresh().catch(() => {}); // the next poll catches up if the RPC is busy
      say(msg, `Done. Say hello to ${name} ($${symbol}). The page header shows it now; wallets and Solscan may take a while to catch up.`, "ok", sig);
      $("rename-form").reset();
      updatePreview();
      return;
    }
    say(msg, `Done. Say hello to ${name} ($${symbol}).`, "ok");
    $("rename-form").reset();
    updatePreview();
  } catch (e2) {
    say(msg, friendly(e2), "err");
  } finally {
    busy(false);
    $("rename-go").disabled = false;
  }
}

// The program's error names, in order: wallets often report only "custom program error: 0x…",
// numbered from 6000.
const PROGRAM_ERRORS = ["NotAdmin", "BadName", "BadSymbol", "BadUri", "ControlChars", "BadBurnAmount", "QuoteDisabled", "QuoteIsSelf", "InvalidParam", "NotAWhirlpool", "BadPosition", "MissingAccount", "BadRoute", "BadHub", "StalePrice", "AlreadyLaunched", "NotLaunched", "WrongPhase", "SameQuote", "WrongPool", "WrongTokenAccount", "PoolManipulated", "RouteOffAverage", "SlippageExceeded", "WrongHop", "NotAtTarget", "NothingToReprice", "NotExpired", "WrongQuote", "MathOverflow", "ForeignPool", "BelowPoolFloor", "NotAtomic", "CoolingDown"];

function friendly(e) {
  let s = String(e?.message ?? e);
  const code = /custom program error: 0x([0-9a-f]+)/i.exec(s);
  if (code) s += " " + (PROGRAM_ERRORS[parseInt(code[1], 16) - 6000] ?? "");
  if (/reject|denied|cancel/i.test(s)) return "You cancelled it in your wallet. Nothing was burned.";
  if (/CoolingDown/.test(s)) return "The last switch was moments ago. The next one opens a few minutes after it. Nothing was burned.";
  if (/StalePrice/.test(s)) return "Prices are still settling after a big move. Try again in a few minutes.";
  if (/BelowPoolFloor/.test(s)) return "The coin would land below that quote's pool floor right now. Pick another quote. Nothing was burned.";
  if (/ForeignPool/.test(s)) return "That quote's pool was set up by someone else, so the coin can't use it. Nothing was burned.";
  if (/WrongPhase/.test(s)) return "Another switch is already running. Wait for it to finish.";
  if (/QuoteDisabled/.test(s)) return "That quote isn't available any more. Pick another. Nothing was burned.";
  if (/SameQuote/.test(s)) return "That's already the coin's quote. Nothing was burned.";
  if (/insufficient/i.test(s)) return "Not enough tokens or SOL for this.";
  return `Something went wrong: ${s.slice(0, 160)}`;
}

let previewUrl = null;
function updatePreview() {
  $("rn-preview-name").textContent = $("rn-name").value.trim() || "Your Name Here";
  $("rn-preview-symbol").textContent = "$" + ($("rn-symbol").value.trim() || "TICKER");
  const f = $("rn-image").files[0];
  if (previewUrl) URL.revokeObjectURL(previewUrl);
  previewUrl = f ? URL.createObjectURL(f) : null;
  $("rn-preview-img").hidden = !previewUrl;
  $("rn-preview-blank").hidden = !!previewUrl;
  if (previewUrl) $("rn-preview-img").src = previewUrl;
}

// ---------------------------------------------------------------------------------------------
// Live data

function toView(s, happenings, image) {
  const quoteView = (q) => {
    const m = QUOTE_META.get(q.mint.toBase58());
    return {
      mint: q.mint.toBase58(),
      symbol: m?.symbol ?? short(q.mint.toBase58()),
      name: m?.name ?? "",
      group: m?.group ?? "Memes & More",
      decimals: q.decimals,
      enabled: q.enabled,
      usd: s.usdOf(q),
    };
  };
  const symbolOf = (k) => QUOTE_META.get(k.toBase58())?.symbol ?? short(k.toBase58());
  return {
    demo: false,
    raw: s,
    coin: { mint: CONFIG.MINT, name: s.metadata.name, symbol: s.metadata.symbol, image },
    launched: s.launched,
    active: s.launched ? quoteView(s.active) : null,
    pool: s.config.activePool.toBase58(),
    venue: s.venue,
    price: s.price,
    floor: s.floor,
    backing: s.backing,
    inPool: s.inPool,
    supply: s.supply,
    burnAmount: Number(s.config.burnAmount) / 1e6,
    totalBurned: Number(s.config.totalBurned) / 1e6,
    renames: Number(s.config.renames),
    quoteChanges: Number(s.config.quoteChanges),
    quotes: s.quotes.map(quoteView),
    sw: { phase: s.config.switch.phase, target: s.config.switch.target.toBase58() },
    // When the coin's price average has warmed up enough for the next switch (the program's rule).
    opensAt: Number(s.config.poolEma.streakStart) + 600,
    happenings: happenings.map((h) => {
      // The launch lays the first pool through the same path as a switch, with no requester.
      const launch = h.name === "Switched" && /^1+$/.test(h.user.toBase58());
      return {
      name: h.name,
      launch,
      user: launch ? "launch" : short(h.user.toBase58()),
      ago: h.time ? ago(h.time) : "",
      sig: h.sig,
      detail:
        h.name === "Renamed" ? `renamed to ${h.newName} ($${h.symbol})`
        : h.name === "Switched" ? `now paired with ${symbolOf(h.quote)}`
        : h.name === "SwitchRequested" ? `asked to switch ${symbolOf(h.from)} → ${symbolOf(h.to)}`
        : `switch to ${symbolOf(h.wanted)} timed out; burn refunded, landed in ${symbolOf(h.landed)}`,
      };
    }),
  };
}

let imageCache = { uri: null, image: null };
let happeningsCache = { at: 0, rows: [] };

async function refresh() {
  if (DEMO) return render();
  conn ??= await chain.connect();
  const owner = wallet ? new (await chain.loadWeb3()).PublicKey(wallet.address) : null;
  const s = await chain.loadState(conn, owner);
  if (s.metadata.uri !== imageCache.uri) {
    imageCache = { uri: s.metadata.uri, image: null };
    try {
      const j = await (await fetch(ipfsHttp(s.metadata.uri))).json();
      imageCache.image = ipfsHttp(j.image) ?? null;
    } catch {
      /* no picture */
    }
  }
  if (Date.now() - happeningsCache.at > 30_000) {
    happeningsCache = { at: Date.now(), rows: await chain.loadHappenings(conn).catch(() => happeningsCache.rows) };
  }
  view = toView(s, happeningsCache.rows, imageCache.image);
  if (wallet) wallet.balance = s.balance ?? 0;
  render();
}

// ---------------------------------------------------------------------------------------------
// Boot

installCursors();
// Before initMenus: rebuild the wallet list first, then the menu opens with it.
$("wallet-btn").addEventListener("click", renderWalletMenu);
initMenus(document.querySelector(".menubar"));
combo = initCombo($("quote-combo"), { placeholder: "Choose…" });
renderWalletMenu();
autoReconnect();
$("switch-go").addEventListener("click", onSwitch);
$("rename-form").addEventListener("submit", onRename);
for (const id of ["rn-name", "rn-symbol", "rn-image"]) $(id).addEventListener("input", updatePreview);
$("copy-ca").addEventListener("click", () => copy(view.coin.mint));
$("copy-ca-menu").addEventListener("click", () => copy(view.coin.mint));
$("copy-pool").addEventListener("click", () => view.pool && copy(view.pool, "st-pool"));

if (DEMO) {
  $("demo-note").hidden = false;
  view = demoView();
  render();
} else {
  refresh().catch((e) => {
    $("coin-name").textContent = "Could not reach Solana";
    say($("switch-msg"), friendly(e), "err");
  });
  setInterval(() => refresh().catch(() => {}), 20_000);
}
