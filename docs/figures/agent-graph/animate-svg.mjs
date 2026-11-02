// animate-svg.mjs — turn a moving Glyph kit diagram into one self-contained animated SVG.
//
// Glyph kit plays its moving diagrams only in HTML. GitHub READMEs show SVG images but run no
// scripts, so this takes the diagram's frames (one per 120 ms step, for one full cycle), draws the
// most common glyph of each cell once, and per frame draws only the cells that differ, as layers
// that CSS keyframes show in turn. Reduced motion shows the diagram's still frame.
//
// The diagram is a module: named exports `title`, `intent`, `source`, and a default async
// function that takes the kit loader and returns { frame(step) → Grid, period, still, alt }.
//
// Usage: node animate-svg.mjs <glyph-kit dir> <diagram.mjs> <out.svg> [theme]
import { writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { join, resolve } from "node:path";

const [kit, diagramPath, outPath, theme = "paper"] = process.argv.slice(2);
const load = (rel) => import(pathToFileURL(join(kit, "scripts", rel)).href);
const { figureSvg } = await load("render/svg.mjs");
const { resolveTheme, SPACING } = await load("render/themes.mjs");
const { STEP_MS } = await load("live/grid.mjs");
const { width: textWidth } = await load("text.mjs");

const diagram = await import(pathToFileURL(resolve(diagramPath)).href);
const { frame, period, still, alt } = await diagram.default(load);
const { title, intent, source } = diagram;

// Each cell is its glyph plus its role ("" ink, "mute", "ok", "fail"), joined by SEP.
const SEP = "\u0001", EMPTY = "\0";
const cellsAt = (step) => {
  const g = frame(step);
  g.toText(); // settles the grid
  return g.ch.map((row, r) => row.map((ch, c) => (ch === " " ? " " : ch + SEP + g.cl[r][c])));
};
const cycle = Array.from({ length: period }, (_, s) => cellsAt(s));
const key = (f) => f.map((row) => row.join("")).join("\n");
if (key(cellsAt(period)) !== key(cycle[0])) throw new Error("frames do not repeat after `period` steps");

// Base: the most common glyph in each cell over the cycle, drawn once.
const glyph = (f, r, c) => f[r]?.[c] || " ";
const rows = Math.max(...cycle.map((f) => f.length));
const modal = Array.from({ length: rows }, (_, r) =>
  Array.from({ length: Math.max(...cycle.map((f) => f[r]?.length ?? 0)) }, (_, c) => {
    const n = new Map();
    for (const f of cycle) n.set(glyph(f, r, c), (n.get(glyph(f, r, c)) ?? 0) + 1);
    return [...n].sort((x, y) => y[1] - x[1])[0][0];
  }));
const deltaOf = (f) => modal.map((row, r) => row.map((m, c) => (glyph(f, r, c) !== m ? glyph(f, r, c) : EMPTY)));

// Runs of identical frames, so each layer is one stretch of time.
const runs = [];
cycle.forEach((f, i) => {
  const d = deltaOf(f), k = key(d);
  if (runs.length && runs.at(-1).key === k) runs.at(-1).len++;
  else runs.push({ key: k, d, start: i, len: 1 });
});

// Layout as the kit's renderSvg: title, intent, a panel holding the figure, then the source.
const colors = resolveTheme(theme).colors;
const padX = SPACING.figurePadX, padY = SPACING.figurePadY;
const titleH = title ? 48 : 0, intentH = intent ? 26 : 0, sourceH = source ? 34 : 0;
const CELL = 10, ROW = 22; // figureSvg's defaults
const at = { colors, x0: padX * 2, y0: padY + titleH + intentH + padY };

// Draw a grid of cells one role at a time, each role in its own colour.
const ROLES = { "": "ink", mute: "muted", ok: "ok", fail: "flag" };
const draw = (d) => {
  const parts = Object.entries(ROLES).map(([role, tone]) => {
    const text = d.map((row) => row.map((v) => (v === EMPTY || v === " " || v.split(SEP)[1] !== role ? " " : v.split(SEP)[0])).join("").trimEnd()).join("\n");
    const c = colors[tone];
    return figureSvg(text, { ...at, colors: { ...colors, ink: c, data: c, muted: c, frame: c, ok: c, flag: c } });
  });
  return { svg: parts.map((p) => p.svg).join(""), defs: parts.map((p) => p.defs).join(""), width: Math.max(...parts.map((p) => p.width)), height: Math.max(...parts.map((p) => p.height)) };
};

// A frame layer: panel-coloured patches over each run of differing cells hide the base glyphs,
// then the frame's own glyphs go on top. Identical layers are defined once and reused.
const patches = (d) => d.flatMap((row, r) => {
  const out = [];
  for (let c = 0; c < row.length; ) {
    if (row[c] === EMPTY) { c++; continue; }
    const c0 = c;
    while (c < row.length && row[c] !== EMPTY) c++;
    out.push(`<rect x="${at.x0 + c0 * CELL}" y="${at.y0 + r * ROW}" width="${(c - c0) * CELL}" height="${ROW}"/>`);
  }
  return out;
}).join("");
const uniq = new Map();
for (const r of runs) if (!uniq.has(r.key)) {
  const fig = draw(r.d);
  uniq.set(r.key, { id: `f${uniq.size}`, svg: `<g fill="${colors.panel}">${patches(r.d)}</g>${fig.svg}`, defs: fig.defs });
}
const baseFig = draw(modal);
const stillFig = draw(cellsAt(still));
const defs = [...new Set([baseFig, stillFig, ...uniq.values()].flatMap((p) => p.defs.match(/<pattern[\s\S]*?<\/pattern>/g) ?? []))].join("")
  + [...uniq.values()].map((u) => `<g id="${u.id}">${u.svg}</g>`).join("");

const total = period * STEP_MS;
const pct = (s) => +((100 * s) / period).toFixed(3);
const css = runs.map((r, i) => {
  const a = pct(r.start), b = pct(r.start + r.len);
  const frames = a === 0 ? `0%{opacity:1}${b}%{opacity:0}100%{opacity:0}` : `0%{opacity:0}${a}%{opacity:1}${b}%{opacity:0}100%{opacity:0}`;
  return `@keyframes k${i}{${frames}}.l${i}{animation:k${i} ${total}ms step-end infinite}`;
}).join("");
const layers = runs.map((r, i) => `<use class="l l${i}" href="#${uniq.get(r.key).id}"/>`).join("");
const style = `<style>.l{opacity:0}${css}.s{display:none}@media (prefers-reduced-motion: reduce){.b{display:none}.s{display:inline}}</style>`;

const figW = Math.max(baseFig.width, stillFig.width), figH = Math.max(baseFig.height, stillFig.height);
const W = Math.max(figW + padX * 4, title ? textWidth(title) * 11 + padX * 2 : 0);
const panelTop = padY + titleH + intentH, panelH = figH + padY * 2;
const H = panelTop + panelH + sourceH + padY;
const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
const mono = `font-family="Menlo, 'DejaVu Sans Mono', Consolas, monospace" font-size="${ROW * 0.66}"`;
const sans = `font-family="system-ui, -apple-system, 'Segoe UI', sans-serif"`;
const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${W}" height="${H}" viewBox="0 0 ${W} ${H}" role="img" aria-label="${esc(alt)}">
<title>${esc(title)}</title>
<defs>${defs}</defs>${style}
<rect width="100%" height="100%" fill="${colors.bg}"/>
${title ? `<text x="${padX}" y="${padY + 26}" ${sans} font-size="22" font-weight="600" fill="${colors.ink}">${esc(title)}</text>` : ""}
${intent ? `<text x="${padX}" y="${padY + titleH + 14}" ${sans} font-size="15" fill="${colors.muted}">${esc(intent)}</text>` : ""}
<rect x="${padX}" y="${panelTop}" width="${W - padX * 2}" height="${panelH}" rx="6" fill="${colors.panel}"/>
<g ${mono}><g class="b">${baseFig.svg}${layers}</g><g class="s">${stillFig.svg}</g></g>
${source ? `<text x="${padX}" y="${panelTop + panelH + 24}" ${sans} font-size="12.5" fill="${colors.muted}">Source: ${esc(source)}</text>` : ""}
</svg>
`;
writeFileSync(outPath, svg);
console.log(`${outPath}: ${period} steps (${(total / 1000).toFixed(1)} s cycle), ${runs.length} layers, ${uniq.size} unique, ${(svg.length / 1024).toFixed(0)} KB`);
