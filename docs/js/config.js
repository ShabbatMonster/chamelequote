// Site settings. Leave MINT empty to run the page in demo mode with sample data.

export const CONFIG = {
  // The coin's mint, set after `initialize` runs on mainnet.
  MINT: "TESZBZpTRGUDriZQg72kYre1Vozyp2de7Z6o5JUtdRA",
  PROGRAM_ID: "3ZYVePG4LhBWH9JvhGcExo1ysX6mWAwTGavBvyMgM3Ws",
  // Must accept browser requests: api.mainnet-beta.solana.com answers 403 to them. A paid RPC
  // (Helius, Triton, ...) is better for real traffic.
  RPC_URL: "https://solana-rpc.publicnode.com",
  // Browser-side IPFS uploads for renames (CORS-open, no key needed).
  IPFS_API: "https://api.thegraph.com/ipfs/api/v0/add",
  // ipfs.io no longer serves files directly (service-worker only); Pinata's gateway does.
  IPFS_GATEWAY: "https://gateway.pinata.cloud/ipfs/",
  // Written into the metadata of every rename, so the coin's links survive whoever renames it.
  LINKS: { website: "https://totality.ws", twitter: "https://x.com/totality_ws" },
  // Lists every token account a wallet holds (CORS-open, no key needed).
  HOLDINGS_API: "https://lite-api.jup.ag/ultra/v1/holdings/",
  EXPLORER_TX: "https://solscan.io/tx/",
  EXPLORER_ACCOUNT: "https://solscan.io/account/",
  WEB3_URL: "https://cdn.jsdelivr.net/npm/@solana/web3.js@1.98.0/+esm",
};
