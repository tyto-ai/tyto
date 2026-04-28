#!/usr/bin/env node
/**
 * Usage: node scripts/pack-local.mjs
 *
 * Builds local npm tarballs from the current repo state for testing without
 * publishing to npm. Output goes to tmp/npm/.
 *
 * After running:
 *   npx tmp/npm/coree-ai-coree-<version>-local.tgz serve
 *   npx tmp/npm/coree-ai-coree-<version>-local.tgz --version
 *
 * Or install the claude-local plugin in Claude Code:
 *   /plugin install coree-local
 */
import * as fs from 'node:fs';
import * as path from 'node:path';
import { execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = path.resolve(fileURLToPath(import.meta.url), '../..');
const NPM_ROOT = path.join(REPO_ROOT, 'npm', '@coree-ai');
const OUT_DIR = path.join(REPO_ROOT, 'tmp', 'npm');

const PLATFORMS = [
  { pkg: 'coree-linux-x64',    binary: 'coree',     ext: '' },
  { pkg: 'coree-linux-arm64',  binary: 'coree',     ext: '' },
  { pkg: 'coree-darwin-arm64', binary: 'coree',     ext: '' },
  { pkg: 'coree-win32-x64',    binary: 'coree.exe', ext: '.exe' },
];

const isWindows = process.platform === 'win32';
const localBinary = path.join(REPO_ROOT, 'target', 'release', isWindows ? 'coree.exe' : 'coree');

if (!fs.existsSync(localBinary)) {
  console.error(`Local binary not found: ${localBinary}`);
  console.error('Run: cargo build --release');
  process.exit(1);
}

fs.mkdirSync(OUT_DIR, { recursive: true });

const mainPkgPath = path.join(NPM_ROOT, 'coree', 'package.json');
const mainManifest = JSON.parse(fs.readFileSync(mainPkgPath, 'utf8'));
const baseVersion = mainManifest.version.replace(/-local.*$/, '');
const localVersion = `${baseVersion}-local`;

console.log(`Packing version: ${localVersion}`);
console.log(`Output: ${path.relative(REPO_ROOT, OUT_DIR)}\n`);

// Pack each platform package, copying in the local binary for the current platform only.
// Other platforms get a placeholder binary so the package structure is valid.
const packedPlatforms = {};
for (const { pkg, binary, ext } of PLATFORMS) {
  const pkgDir = path.join(NPM_ROOT, pkg);
  const manifest = JSON.parse(fs.readFileSync(path.join(pkgDir, 'package.json'), 'utf8'));
  manifest.version = localVersion;

  const binaryDst = path.join(pkgDir, binary);
  const isCurrentPlatform =
    (process.platform === 'linux' && process.arch === 'x64' && pkg === 'coree-linux-x64') ||
    (process.platform === 'linux' && process.arch === 'arm64' && pkg === 'coree-linux-arm64') ||
    (process.platform === 'darwin' && process.arch === 'arm64' && pkg === 'coree-darwin-arm64') ||
    (process.platform === 'win32' && process.arch === 'x64' && pkg === 'coree-win32-x64');

  if (isCurrentPlatform) {
    fs.copyFileSync(localBinary, binaryDst);
    if (!isWindows) fs.chmodSync(binaryDst, 0o755);
    console.log(`  ${pkg}: copied local binary`);
  } else {
    // Write a stub so the package file list is valid
    if (!fs.existsSync(binaryDst)) {
      fs.writeFileSync(binaryDst, '');
    }
    console.log(`  ${pkg}: stub binary (not current platform)`);
  }

  fs.writeFileSync(path.join(pkgDir, 'package.json'), JSON.stringify(manifest, null, 2) + '\n');
  try {
    const tgz = execSync(`npm pack ${pkgDir} --pack-destination ${OUT_DIR}`, { encoding: 'utf8' }).trim();
    packedPlatforms[pkg] = tgz;
  } finally {
    manifest.version = baseVersion;
    fs.writeFileSync(path.join(pkgDir, 'package.json'), JSON.stringify(manifest, null, 2) + '\n');
  }
}

// Pack main package with file: deps pointing to local tarballs
const mainCopy = JSON.parse(fs.readFileSync(mainPkgPath, 'utf8'));
mainCopy.version = localVersion;
for (const { pkg } of PLATFORMS) {
  const tgzName = path.basename(packedPlatforms[pkg]);
  mainCopy.optionalDependencies[`@coree-ai/${pkg}`] = `file:${path.join(OUT_DIR, tgzName)}`;
}
fs.writeFileSync(mainPkgPath, JSON.stringify(mainCopy, null, 2) + '\n');
let mainTgz;
try {
  mainTgz = execSync(`npm pack ${path.join(NPM_ROOT, 'coree')} --pack-destination ${OUT_DIR}`, { encoding: 'utf8' }).trim();
  console.log(`\n  main: ${path.basename(mainTgz)}`);
} finally {
  mainManifest.version = baseVersion;
  for (const key of Object.keys(mainManifest.optionalDependencies)) {
    mainManifest.optionalDependencies[key] = baseVersion;
  }
  fs.writeFileSync(mainPkgPath, JSON.stringify(mainManifest, null, 2) + '\n');
}

// Write the local plugin files derived from agents/claude/, substituting the npm
// package reference with the local tarball path.
// npx requires the file: URI scheme to run a local tarball.
const npmRef = `@coree-ai/coree@${baseVersion}`;
const tgzAbsPath = `file:${path.join(OUT_DIR, path.basename(mainTgz))}`;
const canonicalDir = path.join(REPO_ROOT, 'agents', 'claude');
const pluginDir = path.join(REPO_ROOT, 'agents', 'claude-local', 'plugin');
const hooksDir = path.join(pluginDir, 'hooks');
const claudePluginDir = path.join(pluginDir, '.claude-plugin');
fs.mkdirSync(hooksDir, { recursive: true });
fs.mkdirSync(claudePluginDir, { recursive: true });

// plugin.json: change name and version only
const pluginJson = JSON.parse(fs.readFileSync(path.join(canonicalDir, '.claude-plugin', 'plugin.json'), 'utf8'));
pluginJson.name = 'coree-local';
pluginJson.version = localVersion;
fs.writeFileSync(path.join(claudePluginDir, 'plugin.json'), JSON.stringify(pluginJson, null, 2) + '\n');

// .mcp.json: replace the npm ref in the args array
const mcpJson = JSON.parse(fs.readFileSync(path.join(canonicalDir, '.mcp.json'), 'utf8'));
for (const server of Object.values(mcpJson.mcpServers)) {
  server.args = server.args.map(a => a === npmRef ? tgzAbsPath : a);
}
fs.writeFileSync(path.join(pluginDir, '.mcp.json'), JSON.stringify(mcpJson, null, 2) + '\n');

// hooks/hooks.json: replace the npm ref in every hook command string
const hooksJson = JSON.parse(fs.readFileSync(path.join(canonicalDir, 'hooks', 'hooks.json'), 'utf8'));
for (const entries of Object.values(hooksJson.hooks)) {
  for (const entry of entries) {
    for (const hook of entry.hooks) {
      hook.command = hook.command.replaceAll(npmRef, tgzAbsPath);
    }
  }
}
fs.writeFileSync(path.join(hooksDir, 'hooks.json'), JSON.stringify(hooksJson, null, 2) + '\n');

console.log('\nLocal plugin written to agents/claude-local/plugin/');
console.log('\nTo test:');
console.log(`  npx "${tgzAbsPath}" --version`);
console.log(`  npx "${tgzAbsPath}" serve`);
console.log('\nTo install as a local plugin, add agents/claude-local as a marketplace in /plugin');
