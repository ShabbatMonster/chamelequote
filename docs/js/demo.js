// Sample state for demo mode (no mint configured). Shapes match what app.js builds from chain.

const q = (mint, symbol, name, group, usd, decimals = 8) => ({ mint, symbol, name, group, usd, decimals, enabled: true });

export const DEMO_QUOTES = [
  q("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC", "USD Coin", "Cash", 1, 6),
  q("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", "USDT", "Tether USD", "Cash", 1, 6),
  q("So11111111111111111111111111111111111111112", "SOL", "Solana", "Solana", 151.42, 9),
  q("XsDoVfqeBukxuZHWhdvWHBhgEHjGNst4MLodqsJHzoB", "TSLAX", "Tesla", "Stocks", 412.66),
  q("Xsc9qvGR1efVDFGLrVsmkzv3qi45LTBjeUKSPmx9qEh", "NVDAX", "NVIDIA", "Stocks", 188.15),
  q("XsoCS1TfEyfFhfvj8EtZ528L3CaKBDBRqRapnBbDF2W", "SPYX", "S&P 500", "Stocks", 671.09),
  q("Xs8S1uUs1zvS2p7iwtsG3b6fkhpvmwz4GYU3gWAmWHZ", "QQQX", "Nasdaq 100", "Stocks", 603.4),
  q("XsCPL9dNWBMvFtTmwcCA5v3xWPSMEBCszbQdiLLq6aN", "GOOGLX", "Google", "Stocks", 245.32),
  q("XsueG8BtpquVJX9LVLLEGuViXUungE6WmK5YZ3p3bd1", "CRCLX", "Circle", "Stocks", 141.77),
  q("Xs7ZdzSHLU9ftNJsii5fCeJhoRWSC32SQGzGQtePxNu", "COINX", "Coinbase", "Stocks", 337.5),
  q("cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij", "cbBTC", "Coinbase Wrapped BTC", "Memes & More", 121930),
  q("2zMMhcVQEXDtdE6vsFS7S7D5oUodfJHE8vd1gnBouauv", "PENGU", "Pudgy Penguins", "Memes & More", 0.031, 6),
  q("9BB6NFEcjBCtnNLFko2FqVQBq8HHM13kCyYcdQbgpump", "Fartcoin", "Fartcoin", "Memes & More", 0.71, 6),
];

const short = (s) => s.slice(0, 4) + "…" + s.slice(-4);

export function demoView() {
  const tsla = DEMO_QUOTES[3];
  const priceUsd = 0.0002241;
  return {
    demo: true,
    coin: {
      mint: "CHAMdemo1111111111111111111111111111111111",
      name: "Tesla Lizard",
      symbol: "TLIZ",
      image: null,
    },
    launched: true,
    active: tsla,
    price: { quote: priceUsd / tsla.usd, usd: priceUsd },
    floor: { quote: 0.0001 / tsla.usd, usd: 0.0001 },
    backing: { quote: 121.4, usd: 121.4 * tsla.usd },
    inPool: 774_112_903,
    supply: 993_000_000,
    burnAmount: 1_000_000,
    totalBurned: 7_000_000,
    renames: 4,
    quoteChanges: 3,
    quotes: DEMO_QUOTES,
    sw: { phase: "Idle" },
    happenings: [
      { name: "Switched", user: short("7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU"), detail: "now paired with TSLAX", ago: "4 min ago" },
      { name: "SwitchRequested", user: short("7xKXtg2CW87d97TXJSDpbD5jBkheTqA83TZRuJosgAsU"), detail: "asked to switch NVDAX → TSLAX", ago: "5 min ago" },
      { name: "Renamed", user: short("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM"), detail: "renamed to Tesla Lizard ($TLIZ)", ago: "22 min ago" },
      { name: "Switched", user: short("HN7cABqLq46Es1jh92dQQisAq662SmxELLLsHHe4YWrH"), detail: "now paired with NVDAX", ago: "1 hr ago" },
      { name: "Renamed", user: short("5ZWj7a1f8tWkjBESHKgrLmXshuXxqeY9SYcfbshpAqPG"), detail: "renamed to Green Machine ($GRN)", ago: "3 hr ago" },
      { name: "SwitchAborted", user: short("HN7cABqLq46Es1jh92dQQisAq662SmxELLLsHHe4YWrH"), detail: "switch to PENGU timed out; burn refunded, landed in SOL", ago: "6 hr ago" },
    ],
  };
}

export const DEMO_WALLET = { address: "DeMoWa11et1111111111111111111111111111111111", balance: 4_200_000 };
