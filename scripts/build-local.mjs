#!/usr/bin/env node
/**
 * Usage: node scripts/build-local.mjs
 *
 * Builds the Rust binary, packs local npm tarballs, and smoke-tests the result.
 * Output goes to tmp/npm/ and agents/claude-local/.
 */
import { execSync } from 'node:child_process';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as fs from 'node:fs';

const REPO_ROOT = path.resolve(fileURLToPath(import.meta.url), '../..');

function run(cmd, label) {
  console.log(`\n>> ${label ?? cmd}`);
  execSync(cmd, { cwd: REPO_ROOT, stdio: 'inherit' });
}

run('cargo build --release');
run('node scripts/pack-local.mjs');

// Find the packed main tarball
const outDir = path.join(REPO_ROOT, 'tmp', 'npm');
const tgz = fs.readdirSync(outDir).find(f => f.match(/^coree-ai-coree-[^-]+-local\.tgz$/));
if (!tgz) {
  console.error('Could not find packed tarball in tmp/npm/');
  process.exit(1);
}
const tgzUri = `file:${path.join(outDir, tgz)}`;

run(`npx --yes "${tgzUri}" --version`, 'smoke test: coree --version');

console.log(`\nDone. Local plugin is at agents/claude-local/`);
console.log(`Tarball: ${tgzUri}`);
