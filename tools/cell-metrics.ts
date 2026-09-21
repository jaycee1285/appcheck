// Derives terminal cell and tile sizes from the live config and the font files.
// Run: bun run tools/cell-metrics.ts
import { readFileSync } from "fs";
import { $ } from "bun";

function metrics(path: string) {
  const b = new DataView(readFileSync(path).buffer);
  const n = b.getUint16(4);
  const t: Record<string, number> = {};
  for (let i = 0; i < n; i++) {
    const p = 12 + i * 16;
    const tag = String.fromCharCode(
      b.getUint8(p), b.getUint8(p + 1), b.getUint8(p + 2), b.getUint8(p + 3),
    );
    t[tag] = b.getUint32(p + 8);
  }
  const upm = b.getUint16(t["head"] + 18);
  return {
    advanceEm: b.getUint16(t["hmtx"]) / upm,
    lineEm: (b.getInt16(t["hhea"] + 4) - b.getInt16(t["hhea"] + 6) + b.getInt16(t["hhea"] + 8)) / upm,
  };
}

const conf = readFileSync(`${process.env.HOME}/.config/kitty/kitty.conf`, "utf8");
const family = conf.match(/^\s*font_family\s+(.+)$/m)?.[1].trim() ?? "monospace";
const pt = Number(conf.match(/^\s*font_size\s+([\d.]+)$/m)?.[1] ?? 11);

// Scale 1.0 compositors put kitty on the 96 DPI base for points.
const scale = Number((await $`wlr-randr`.text()).match(/Scale:\s*([\d.]+)/)?.[1] ?? 1);
const px = (pt * 96) / 72 * scale;

const resolved = (await $`fc-match -f %{file} ${family}`.text()).trim();
const requested = (await $`fc-match -f %{family} ${family}`.text()).trim();
console.log(`kitty font_family  ${family}`);
console.log(`kitty font_size    ${pt}pt  (scale ${scale} -> ${px.toFixed(2)}px)`);
console.log(`fontconfig gives   ${requested}`);
console.log(`                   ${resolved}`);
if (!family.toLowerCase().includes(requested.toLowerCase().split(" ")[0]))
  console.log(`  ** the configured family does not match; this is a fallback **`);

const screen = (await $`wlr-randr`.text()).match(/(\d+)x(\d+) px/);
const [W, H] = [Number(screen?.[1] ?? 1920), Number(screen?.[2] ?? 1200)];

const faces: Record<string, string> = { [`${requested} (in use)`]: resolved };
for (const extra of ["IBM Plex Mono", "Paper Mono"]) {
  const f = (await $`fc-match -f %{file} ${extra}`.text()).trim();
  if (f !== resolved) faces[extra] = f;
}

for (const [name, file] of Object.entries(faces)) {
  const m = metrics(file);
  const cw = Math.round(m.advanceEm * px);
  const ch = Math.ceil(m.lineEm * px);
  console.log(`\n=== ${name} ===  ${m.advanceEm.toFixed(3)}em x ${m.lineEm.toFixed(3)}em  ->  cell ${cw}x${ch}`);
  for (const [label, w, h] of [
    ["full   ", W, H],
    ["half  |", W / 2, H],
    ["half  -", W, H / 2],
    ["quarter", W / 2, H / 2],
  ] as [string, number, number][])
    console.log(`  ${label}  ${w}x${h}px  ->  ${Math.floor(w / cw)}x${Math.floor(h / ch)} cells`);
}
