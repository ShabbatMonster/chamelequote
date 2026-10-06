// Site settings. Leave MINT empty to run the page in demo mode with sample data.

export const CONFIG = {
  // The coin's mint, set after `initialize` runs on mainnet.
  MINT: "",
  PROGRAM_ID: "3ZYVePG4LhBWH9JvhGcExo1ysX6mWAwTGavBvyMgM3Ws",
  // Use a paid RPC in production; the public endpoint rate-limits hard.
  RPC_URL: "https://api.mainnet-beta.solana.com",
  // Browser-side IPFS uploads for renames (CORS-open, no key needed).
  IPFS_API: "https://api.thegraph.com/ipfs/api/v0/add",
  IPFS_GATEWAY: "https://ipfs.io/ipfs/",
  EXPLORER_TX: "https://solscan.io/tx/",
  EXPLORER_ACCOUNT: "https://solscan.io/account/",
  WEB3_URL: "https://cdn.jsdelivr.net/npm/@solana/web3.js@1.98.0/+esm",
};
