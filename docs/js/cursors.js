// Classic 1998 desktop cursors, drawn pixel by pixel. X = black, o = white, . = clear.
// Installed as CSS custom properties: --cur-arrow, --cur-hand, --cur-wait, --cur-text.

const ARROW = [
  "X...........",
  "XX..........",
  "XoX.........",
  "XooX........",
  "XoooX.......",
  "XooooX......",
  "XoooooX.....",
  "XooooooX....",
  "XoooooooX...",
  "XooooooooX..",
  "XoooooXXXXX.",
  "XooXooX.....",
  "XoX.XooX....",
  "XX..XooX....",
  "X....XooX...",
  ".....XooX...",
  "......XooX..",
  "......XooX..",
  ".......XX...",
];

const HAND = [
  ".....XX.........",
  "....XooX........",
  "....XooX........",
  "....XooX........",
  "....XooX........",
  "....XooXXX......",
  "....XooXooXXX...",
  "....XooXooXooXX.",
  ".XX.XooXooXooXoX",
  "XooXXooooooooXoX",
  "XoooXooooooooooX",
  ".XooXooooooooooX",
  "..XoXooooooooooX",
  "..XooooooooooooX",
  "...XoooooooooooX",
  "...XooooooooooX.",
  "....XoooooooooX.",
  "....XoooooooooX.",
  ".....XooooooooX.",
  ".....XXXXXXXXXX.",
];

const WAIT = [
  "XXXXXXXXXXX",
  "XoooooooooX",
  ".XXXXXXXXX.",
  ".XoooooooX.",
  ".XoXoXoXoX.",
  "..XoXoXoX..",
  "...XoXoX...",
  "....XoX....",
  "....XoX....",
  "...XoooX...",
  "..XooXooX..",
  ".XoooXoooX.",
  ".XoXoXoXoX.",
  ".XXXXXXXXX.",
  "XoooooooooX",
  "XXXXXXXXXXX",
];

const TEXT = [
  "XXX.XXX",
  "...X...",
  "...X...",
  "...X...",
  "...X...",
  "...X...",
  "...X...",
  "...X...",
  "...X...",
  "...X...",
  "...X...",
  "...X...",
  "...X...",
  "...X...",
  "XXX.XXX",
];

function svg(rows) {
  const w = Math.max(...rows.map((r) => r.length));
  let rects = "";
  rows.forEach((row, y) => {
    [...row].forEach((c, x) => {
      if (c === "X" || c === "o") rects += `<rect x="${x}" y="${y}" width="1" height="1" fill="${c === "X" ? "#000" : "#fff"}"/>`;
    });
  });
  const s = `<svg xmlns="http://www.w3.org/2000/svg" width="${w}" height="${rows.length}" shape-rendering="crispEdges">${rects}</svg>`;
  return `url("data:image/svg+xml,${encodeURIComponent(s)}")`;
}

export function installCursors() {
  const root = document.documentElement.style;
  root.setProperty("--cur-arrow", `${svg(ARROW)} 0 0`);
  root.setProperty("--cur-hand", `${svg(HAND)} 5 0`);
  root.setProperty("--cur-wait", `${svg(WAIT)} 5 7`);
  root.setProperty("--cur-text", `${svg(TEXT)} 3 7`);
}
