// Wallet discovery and signing. Supports both Wallet Standard wallets (how current Phantom,
// Solflare, Backpack, Jupiter and others announce themselves) and the older injected providers
// (window.phantom.solana, window.solflare, window.backpack) as a fallback.

const CHAIN = "solana:mainnet";
const standard = new Map(); // name -> Wallet Standard wallet

function register(...wallets) {
  for (const w of wallets) {
    const solana = w.chains?.some((c) => c.startsWith("solana:"));
    if (solana && w.features?.["standard:connect"] && w.features?.["solana:signTransaction"]) standard.set(w.name, w);
  }
  return () => {};
}

// Wallet Standard handshake: tell wallets we're ready, and accept wallets that load later.
const api = Object.freeze({ register });
window.addEventListener("wallet-standard:register-wallet", (e) => e.detail?.(api));
window.dispatchEvent(new CustomEvent("wallet-standard:app-ready", { detail: api }));

const LEGACY = [
  { name: "Phantom", get: () => window.phantom?.solana ?? (window.solana?.isPhantom ? window.solana : null) },
  { name: "Solflare", get: () => (window.solflare?.isSolflare ? window.solflare : null) },
  { name: "Backpack", get: () => window.backpack?.solana ?? null },
];

/** Every wallet currently available, Wallet Standard first. [{ name, icon, connect(silent) }] */
export function listWallets() {
  const out = [...standard.values()].map((w) => ({ name: w.name, icon: w.icon, connect: (silent) => connectStandard(w, silent) }));
  for (const l of LEGACY) {
    const p = l.get();
    if (p && !out.some((o) => o.name === l.name)) out.push({ name: l.name, icon: null, connect: (silent) => connectLegacy(l.name, p, silent) });
  }
  return out;
}

/**
 * A connected wallet: { name, address (base58), sign(web3 Transaction) -> Uint8Array, disconnect() }.
 * `silent` only reconnects a wallet the user already approved (no popup); it throws otherwise.
 */
async function connectStandard(w, silent) {
  const { accounts } = await w.features["standard:connect"].connect(silent ? { silent: true } : undefined);
  const account = accounts?.[0] ?? w.accounts?.[0];
  if (!account) throw new Error(`${w.name} did not share an account`);
  return {
    name: w.name,
    address: account.address,
    async sign(tx) {
      const bytes = tx.serialize({ requireAllSignatures: false, verifySignatures: false });
      const [res] = await w.features["solana:signTransaction"].signTransaction({ transaction: bytes, account, chain: CHAIN });
      return res.signedTransaction;
    },
    disconnect: () => w.features["standard:disconnect"]?.disconnect(),
  };
}

async function connectLegacy(name, p, silent) {
  const res = await p.connect(silent ? { onlyIfTrusted: true } : undefined);
  const key = res?.publicKey ?? p.publicKey;
  if (!key) throw new Error(`${name} did not share an account`);
  return {
    name,
    address: key.toString(),
    async sign(tx) {
      return (await p.signTransaction(tx)).serialize();
    },
    disconnect: () => p.disconnect?.(),
  };
}

const LAST = "cq.wallet";
export function rememberWallet(name) {
  try {
    name ? localStorage.setItem(LAST, name) : localStorage.removeItem(LAST);
  } catch {
    /* storage blocked: no auto-reconnect */
  }
}
export function lastWallet() {
  try {
    return localStorage.getItem(LAST);
  } catch {
    return null;
  }
}
