// Reality-check the ledger against the filesystem. Read-only: existence, type,
// and provenance only. Never runs a binary, never writes.
import data from "../apptrack.toml";
import { existsSync, lstatSync, readdirSync, realpathSync } from "fs";

const HOME = process.env.HOME!;
const BINS = [
  `${HOME}/.local/bin`,
  `${HOME}/.cargo/bin`,
  `${HOME}/bin`,
  `${HOME}/.bun/bin`,
  `/etc/profiles/per-user/${process.env.USER}/bin`,
  `${HOME}/.nix-profile/bin`,
  "/run/current-system/sw/bin",
];

const present = new Map<string, string[]>();
for (const dir of BINS) {
  if (!existsSync(dir)) continue;
  for (const entry of readdirSync(dir)) {
    (present.get(entry) ?? present.set(entry, []).get(entry)!).push(dir);
  }
}

const where = (dir: string) =>
  dir.includes(".cargo") ? "cargo"
  : dir.includes(".bun") ? "bun"
  : dir.includes("nix") || dir.includes("per-user") ? "nix"
  : dir.includes(".local/bin") ? "local/bin"
  : dir;

type App = {
  name: string; category: string; disposition: string; installed?: boolean;
  identity: string; recipe?: unknown; installed_paths?: string[];
  provenance?: { source?: string; installer?: string; managed_by_apptrack?: boolean };
};
const apps: App[] = (data as any).apps;
const arg = process.argv[2] ?? "using";
const chosen = apps.filter((a) => arg === "all" || a.disposition === arg);

let found = 0, missing = 0, stale = 0;
const rows: string[] = [];
for (const a of chosen) {
  const dirs = present.get(a.name) ?? [];
  const sources = [...new Set(dirs.map(where))];
  // Recorded paths that no longer exist are the interesting stale case.
  const dead = (a.installed_paths ?? []).filter((p) => !existsSync(p.replace("~", HOME)));
  let link = "";
  if (dirs.length) {
    try {
      const p = `${dirs[0]}/${a.name}`;
      if (lstatSync(p).isSymbolicLink()) link = ` -> ${realpathSync(p).split("/").slice(0, 4).join("/")}`;
    } catch {}
  }
  if (dirs.length) found++; else missing++;
  if (dead.length) stale++;
  rows.push(
    [
      a.name.padEnd(20),
      (sources.join(",") || "MISSING").padEnd(12),
      (a.provenance?.source ?? "-").padEnd(8),
      a.recipe ? "recipe" : "      ",
      dead.length ? `STALE PATH: ${dead.join(" ")}` : "",
      link,
    ].join(" "),
  );
}

console.log(`${arg}: ${chosen.length} apps — ${found} on disk, ${missing} not found, ${stale} with a recorded path that is gone\n`);
console.log("name                 found-in     ledger   recipe");
console.log(rows.sort().join("\n"));

const candidates = chosen.filter((a) => (present.get(a.name)?.length ?? 0) > 0 && !a.recipe);
console.log(`\n${candidates.length} installed without a recipe (candidates for intake):`);
for (const a of candidates) {
  const src = [...new Set((present.get(a.name) ?? []).map(where))].join(",");
  const gh = a.identity.startsWith("https://github.com/") ? a.identity : "no github identity";
  console.log(`  ${a.name.padEnd(20)} ${src.padEnd(12)} ${gh}`);
}
