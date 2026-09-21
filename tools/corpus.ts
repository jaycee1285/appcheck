import data from "../apptrack.toml";

type App = {
  name: string; category: string; description?: string;
  tags?: string[]; disposition: string; recipe?: unknown;
};
const apps: App[] = (data as any).apps;

const w = (s: string) => [...s].length; // all ASCII here; cells == chars
const pct = (xs: number[], p: number) =>
  [...xs].sort((a, b) => a - b)[Math.floor((xs.length - 1) * p)];

const names = apps.map((a) => w(a.name));
const cats = [...new Set(apps.map((a) => a.category))];
const descs = apps.map((a) => w(a.description ?? ""));

console.log(`apps ${apps.length}   categories ${cats.length}`);
console.log(`\nname width    max ${Math.max(...names)}  p95 ${pct(names, 0.95)}  p50 ${pct(names, 0.5)}`);
console.log(`desc width    max ${Math.max(...descs)}  p95 ${pct(descs, 0.95)}  p50 ${pct(descs, 0.5)}`);
console.log(`\nlongest names: ${apps.map(a => a.name).sort((a,b)=>w(b)-w(a)).slice(0,5).join(", ")}`);

console.log(`\ncategory widths (rendered in the map's left column):`);
for (const c of cats.sort((a, b) => w(b) - w(a)))
  console.log(`  ${String(w(c)).padStart(2)}  ${c}  (${apps.filter(a => a.category === c).length} apps)`);

// Map row budget: 4 + category column + three count cells of (digits+4), per minimum_size().
const digits = String(Math.max(...cats.map(c => apps.filter(a => a.category === c).length))).length;
const catW = Math.max(...cats.map(w));
const mapNeeds = 4 + catW + 3 * (digits + 4);
console.log(`\nmap needs ${mapNeeds} cols at full category width (${catW}) + counts (${digits} digits)`);

// Detail/inspect rows are name + description on one line.
const widest = Math.max(...apps.map((a) => w(a.name) + 2 + w(a.description ?? "")));
console.log(`widest name+description line: ${widest} cols`);

const QUARTER = 87;
console.log(`\nagainst a ${QUARTER}-col quarter:`);
console.log(`  map fits: ${mapNeeds <= QUARTER}  (${QUARTER - mapNeeds} cols spare)`);
console.log(`  descriptions over ${QUARTER} cols: ${descs.filter((d) => d > QUARTER).length}`);
console.log(`  name+desc lines over ${QUARTER} cols: ${apps.filter((a) => w(a.name) + 2 + w(a.description ?? "") > QUARTER).length}`);

const withRecipe = apps.filter((a) => a.recipe);
console.log(`\nrecipes: ${withRecipe.length} (${withRecipe.map((a) => a.name).join(", ")})`);
const byDisp: Record<string, number> = {};
for (const a of apps) byDisp[a.disposition] = (byDisp[a.disposition] ?? 0) + 1;
console.log(`dispositions: ${JSON.stringify(byDisp)}`);
