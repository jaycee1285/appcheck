#!/usr/bin/env node
// Read-only capture of saved model cards and local weight bundles.
import { open, readlink, readdir, stat } from 'node:fs/promises';
import { homedir } from 'node:os';
import { extname, join, relative } from 'node:path';

const home = homedir();
const notesRoot = join(home, 'syncthing/flownotes/AI/models/inbox');
const roots = [join(home, 'repos/models'), join(home, 'syncthing/models')];
const downloadsRoot = join(home, 'Downloads');

async function filesUnder(dir) {
  const files = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) files.push(...await filesUnder(path));
    else if (entry.isFile()) {
      const info = await stat(path);
      files.push({ path, bytes: info.size, modifiedAt: info.mtime.toISOString() });
    } else if (entry.isSymbolicLink()) {
      const linkTarget = await readlink(path);
      const info = await stat(path).catch(() => null);
      files.push({ path, bytes: 0, linkTarget, targetBytes: info?.size ?? null });
    }
  }
  return files.sort((a, b) => a.path.localeCompare(b.path));
}

async function note(path) {
  // Frontmatter is enough for identity and provenance; model-card bodies are not ingested.
  const handle = await open(path, 'r');
  const buffer = Buffer.alloc(8192);
  let bytesRead;
  try { ({ bytesRead } = await handle.read(buffer, 0, buffer.length, 0)); }
  finally { await handle.close(); }
  const text = buffer.toString('utf8', 0, bytesRead);
  // One saved card uses *** and a dashed separator instead of YAML fences.
  const field = key => text.match(new RegExp(`^${key}:\\s*(.+)$`, 'm'))?.[1]
    ?.trim().replace(/^['"]|['"]$/g, '') ?? null;
  const rawUrl = field('url');
  if (!rawUrl) throw new Error(`No source URL: ${path}`);
  const url = rawUrl.replace(/^<|>$/g, '');
  return { url, title: field('title'), capturedAt: field('captured'), notePath: path };
}

async function bundles(root) {
  const result = [];
  async function visit(dir) {
    const entries = await readdir(dir, { withFileTypes: true });
    if (entries.some(entry => entry.isFile() || entry.isSymbolicLink())) {
      const files = await filesUnder(dir);
      result.push({ path: dir, root, bytes: files.reduce((sum, file) => sum + file.bytes, 0),
        files: files.map(file => ({ path: relative(dir, file.path), bytes: file.bytes,
          ...(file.modifiedAt ? { modifiedAt: file.modifiedAt } : {}),
          ...(file.linkTarget ? { linkTarget: file.linkTarget, targetBytes: file.targetBytes } : {}) })) });
    } else {
      for (const entry of entries) if (entry.isDirectory()) await visit(join(dir, entry.name));
    }
  }
  for (const entry of await readdir(root, { withFileTypes: true })) {
    if (entry.isDirectory()) await visit(join(root, entry.name));
  }
  return result.sort((a, b) => a.path.localeCompare(b.path));
}

async function looseDownloads() {
  const extensions = new Set(['.gguf', '.safetensors', '.onnx', '.nemo', '.tflite']);
  const paths = [];
  async function visit(dir) {
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name);
      if (entry.isDirectory()) await visit(path);
      else if (entry.isFile() && extensions.has(extname(entry.name).toLowerCase())) paths.push(path);
    }
  }
  await visit(downloadsRoot);
  return Promise.all(paths.sort().map(async path => {
    const info = await stat(path);
    return { path, bytes: info.size, modifiedAt: info.mtime.toISOString(),
      format: extname(path).slice(1).toLowerCase() };
  }));
}

const paths = (await readdir(notesRoot)).filter(name => name.endsWith('.md')).sort();
const notes = await Promise.all(paths.map(name => note(join(notesRoot, name))));
const byUrl = new Map();
for (const item of notes) byUrl.set(item.url, [...(byUrl.get(item.url) ?? []), item.notePath]);
const repeatedUrls = [...byUrl].filter(([, paths]) => paths.length > 1)
  .map(([url, paths]) => ({ url, notePaths: paths }));
const diskBundles = (await Promise.all(roots.map(bundles))).flat();
const downloadsArtifacts = await looseDownloads();
process.stdout.write(JSON.stringify({ schemaVersion: 1, scannedAt: new Date().toISOString(),
  notes, repeatedUrls, diskBundles, downloadsArtifacts }, null, 2) + '\n');
