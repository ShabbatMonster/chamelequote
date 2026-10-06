// Docs page: cursors, menus, and the few live values (approved quotes, fee share, mint).

import { CONFIG } from "./config.js";
import { installCursors } from "./cursors.js";
import { initMenus } from "./widgets.js";
import { DEMO_QUOTES } from "./demo.js";
import { QUOTE_META } from "./quotes-meta.js";

const DEMO = !CONFIG.MINT || location.hash === "#demo";
const $ = (id) => document.getElementById(id);
const GROUP_ORDER = ["Stocks", "Pre-IPO", "Cash", "Solana", "Leveraged", "Backpack", "Collectibles", "Memes & More"];
const price = (n) => "$" + (n >= 1000 ? Math.round(n).toLocaleString("en-US") : n >= 1 ? n.toFixed(2) : n.toLocaleString("en-US", { maximumSignificantDigits: 4 }));

function renderQuotes(quotes, note) {
  const body = $("quote-table").tBodies[0];
  body.innerHTML = "";
  quotes
    .filter((q) => q.enabled)
    .sort((a, b) => GROUP_ORDER.indexOf(a.group) - GROUP_ORDER.indexOf(b.group) || a.symbol.localeCompare(b.symbol))
    .forEach((q) => {
      const tr = body.insertRow();
      for (const text of [q.symbol, q.name, q.group, q.usd ? price(q.usd) : "—"]) tr.insertCell().textContent = text;
      tr.cells[0].style.fontWeight = "bold";
    });
  $("quote-note").textContent = note;
}

async function live() {
  const chain = await import("./chain.js");
  const conn = await chain.connect();
  const s = await chain.loadState(conn);
  $("ref-mint").textContent = CONFIG.MINT;
  $("fee-share").textContent = `${(s.config.feeShareBps / 100).toFixed(0)}%`;
  renderQuotes(
    s.quotes.map((q) => {
      const m = QUOTE_META.get(q.mint.toBase58());
      return { symbol: m?.symbol ?? q.mint.toBase58().slice(0, 6), name: m?.name ?? "", group: m?.group ?? "Memes & More", enabled: q.enabled, usd: s.usdOf(q) };
    }),
    "Live from the program's quote list. Prices are its on-chain averages.",
  );
}

installCursors();
initMenus(document.querySelector(".menubar"));
if (DEMO) {
  renderQuotes(DEMO_QUOTES, "Sample list. The real list appears here once the coin is live.");
} else {
  live().catch(() => renderQuotes([], "Could not reach Solana to load the list. Try again in a minute."));
}
