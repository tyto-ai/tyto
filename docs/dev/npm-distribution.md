# npm Distribution

Distribute coree as npm packages using the `optionalDependencies` pattern.
Reference implementations: **biome** and **esbuild** (pure `optionalDependencies`,
no postinstall download, `spawnSync` entrypoint).

## Why

Plugin installers (Claude Code, Gemini, Codex) copy only the plugin source directory
into a local cache. Anything outside that directory is absent at runtime. npm distribution
solves this and eliminates lazy model downloads - the binary and model arrive together at
install time.

## Package layout

```
npm/@coree-ai/
  coree/                              main package (2.4kB)
    bin/coree                         Node.js platform resolver + env setup
    .claude-plugin/plugin.json
    .mcp.json
    hooks/hooks.json
    THIRD_PARTY_NOTICES
    package.json                      depends on model pkg + optional platform pkgs

  coree-model-bge-small-en-v1.5/     model package (76.6MB, published once per model change)
    model/                            HuggingFace hub cache (real files, no symlinks)
    package.json

  coree-linux-x64/                    platform packages (~20MB each, published every release)
  coree-linux-arm64/
  coree-darwin-arm64/
  coree-win32-x64/
```

The separation is intentional: binary releases happen frequently; the model changes only
when fastembed changes its default. Users updating coree only re-download platform packages
(~20MB) since npm's lockfile keeps the model package at its pinned version.

## `bin/coree` - the entrypoint

A Node.js wrapper that:
1. Selects the platform binary via `require.resolve` on the appropriate optional package
2. Finds the bundled model via `require.resolve('@coree-ai/coree-model-bge-small-en-v1.5/package.json')`
3. Sets `COREE_MODEL_DIR` to the model package's `model/` directory
4. Detects the Codex PWD mismatch and sets `COREE__PROJECT_ROOT` if needed
5. `spawnSync` with `stdio: 'inherit'` (correct for MCP JSON-RPC)

`spawnSync` is correct here - the child inherits stdin/stdout for the JSON-RPC stream
and the parent blocks until exit. Derived from biome's `bin/biome`.

## The model package

### What fastembed actually needs

fastembed's default model (`BGESmallENV15`, explicitly set in `src/embed.rs`) uses:

- repo: `Xenova/bge-small-en-v1.5`
- model file: `onnx/model.onnx` (~127MB FP32)
- tokenizer files: `tokenizer.json`, `config.json`, `special_tokens_map.json`, `tokenizer_config.json`

This was confirmed by reading the fastembed 5.13.3 source (`#[default]` attribute on
`BGESmallENV15`) and inspecting the local cache at `~/.cache/tyto/models/`.

### Why snapshot_download was wrong

The original CI used `snapshot_download(repo_id='Xenova/bge-small-en-v1.5')` which fetches
the entire HuggingFace repo including every ONNX quantization variant (FP16, INT8, INT4, etc.)
- 438MB uncompressed, 301MB compressed. Only `onnx/model.onnx` and the tokenizer files are
ever read at runtime.

### Why verbatimSymlinks was wrong

The HuggingFace hub cache format stores real files in `blobs/` (content-addressed by SHA) and
symlinks them from `snapshots/<commit>/<filename>`. `fs.cpSync` with `verbatimSymlinks: true`
preserved these symlinks into the npm package. But **npm pack silently skips symlinks** - the
`snapshots/` directory was absent from the tarball entirely. The bundled model was never usable.

### The fix: `scripts/fetch-model.py`

```
python3 scripts/fetch-model.py dist/model
```

1. Uses `hf_hub_download` to fetch only the 5 needed files (avoids all other quantizations)
2. Resolves all symlinks in `snapshots/` to real files (`shutil.copy2` + `os.unlink`)
3. Deletes `blobs/` (now redundant) to avoid storing data twice

Result: `snapshots/<commit>/<file>` are real files. npm pack includes them. fastembed finds
the model. No duplication. Package compressed to **76.6MB**.

### HuggingFace hub format in the model package

After `fetch-model.py` runs:

```
models--Xenova--bge-small-en-v1.5/
  refs/
    main                           <- commit hash (real file)
  snapshots/
    ea104dacec62c0de699686887e3f920caeb4f3e3/
      config.json                  <- real file (~683B)
      special_tokens_map.json      <- real file (~125B)
      tokenizer_config.json        <- real file (~366B)
      tokenizer.json               <- real file (~695KB)
      onnx/
        model.onnx                 <- real file (~127MB FP32)
```

No `blobs/` directory. fastembed reads `snapshots/<commit>/<file>` directly - it does not
require blobs/ to be present.

### Local testing without Python

The binary caches the model at `~/.cache/tyto/models/` (old name). To build the npm packages
locally without Python or a fresh download:

```bash
cp -rL ~/.cache/tyto/models/ dist/model
rm -rf dist/model/models--Xenova--bge-small-en-v1.5/blobs
node scripts/generate-npm-packages.mjs
```

`cp -rL` follows symlinks, resolving them to real files. Removing `blobs/` leaves the
structure that `fetch-model.py` produces in CI.

## CI workflows

### `release.yml` - `publish-npm` job

Runs after all platform binaries are built. Steps:
1. Download all build artifacts (platform tarballs)
2. Extract and rename binaries (each tarball contains a file named `coree`, rename to match artifact)
3. `pip install huggingface_hub && python3 scripts/fetch-model.py dist/model`
4. `node scripts/generate-npm-packages.mjs`
5. Publish model package first, then platform packages, then main package

Uses npm trusted publishing (OIDC) - no `NPM_TOKEN`. Requires `id-token: write` permission.
`registry-url: 'https://registry.npmjs.org'` must be set in `setup-node` for the OIDC exchange.

### `dev-release.yml` - `pack-npm-dev` job

Same steps but uses `npm pack` instead of `npm publish` (no auth required). Uploads `.tgz`
files to the `dev` GitHub release for manual inspection. Runs on every push to main as a
dry-run of the full publish pipeline. Version is `{version}-dev.{run_number}`.

### Trusted publishers (npmjs.com)

All 6 packages must have a trusted publisher configured:
- Repository: `coree-ai/coree`
- Workflow: `release.yml`
- Environment: `npm-publish`

Packages: `@coree-ai/coree`, `@coree-ai/coree-model-bge-small-en-v1.5`,
`@coree-ai/coree-linux-x64`, `@coree-ai/coree-linux-arm64`,
`@coree-ai/coree-darwin-arm64`, `@coree-ai/coree-win32-x64`

To publish stub packages locally (one-time setup, requires 2FA):
```bash
npm login
npm publish npm/@coree-ai/<package-name> --access public --no-provenance
```

## Rust changes

### `src/embed.rs`

- Explicit `const MODEL: EmbeddingModel = EmbeddingModel::BGESmallENV15` instead of
  `EmbeddingModel::default()` so the active model is unambiguous in the source.
- `COREE_MODEL_DIR` env var: when set, used as the fastembed `cache_dir` directly.
  The npm `bin/coree` sets this to the model package's `model/` directory.
- `COREE_FORCE_MODEL_REFRESH=1`: deletes the cache dir before loading (troubleshooting).

### `src/config.rs`

- `project_root`: changed from `PathBuf` with `#[serde(skip)]` to `Option<PathBuf>`
  with `#[serde(default)]`.
- Two-pass `Config::load()`: first pass extracts `project_root` from global config + env
  vars, second pass uses it as `start_dir` for `.coree.toml` discovery.
- `project_root()` accessor: always `Some` after `Config::load()`, panics otherwise.
- `COREE__PROJECT_ROOT` env var: allows the project root to be set externally.

## Codex: why `bin/coree` solves the project root problem

Codex spawns MCP servers with `cwd` = plugin cache (not project dir). Shell scripts reset
`PWD` before any code runs. But **Node.js does NOT reset PWD**.

```
Shell (cwd=/plugin/cache, env PWD=/project):
  PWD gets reset to /plugin/cache before first line

Node.js (cwd=/plugin/cache, env PWD=/project):
  process.env.PWD == '/project'   <- preserved
  process.cwd()   == '/plugin/cache'
```

`bin/coree` detects this mismatch (`PWD !== process.cwd()`) and sets
`COREE__PROJECT_ROOT = process.env.PWD` before spawning the binary.

Codex `.mcp.json` must whitelist `PWD` in `env_vars`:

```json
{
  "mcpServers": {
    "coree": {
      "command": "./bin/coree",
      "args": ["serve"],
      "cwd": ".",
      "env_vars": ["PWD", "COREE_BINARY_OVERRIDE", "COREE__MEMORY__REMOTE_AUTH_TOKEN", "RUST_LOG"]
    }
  }
}
```

## Agent plugin launch comparison

| | Claude Code | Gemini | Codex |
|---|---|---|---|
| Command interpolation | `${CLAUDE_PLUGIN_ROOT}` | `${extensionPath}` | None |
| MCP server cwd | Session dir (project) | Session dir (project) | Plugin cache |
| PWD preserved | N/A | N/A | Yes (if whitelisted) |
| Fix needed | None | None | `bin/coree` sets `COREE__PROJECT_ROOT` |

## Licensing

- **coree binary**: Apache-2.0
- **Bundled BAAI/bge-small-en-v1.5 model**: MIT (only requirement: preserve copyright notice)
- **`THIRD_PARTY_NOTICES`** in main npm package: BAAI model MIT license text

## Package size summary

| Package | Compressed | Published |
|---|---|---|
| `@coree-ai/coree` | 2.4 kB | Every release |
| `@coree-ai/coree-model-bge-small-en-v1.5` | 76.6 MB | When fastembed default model changes |
| `@coree-ai/coree-linux-x64` | ~20 MB | Every release |
| `@coree-ai/coree-linux-arm64` | ~21 MB | Every release |
| `@coree-ai/coree-darwin-arm64` | ~18 MB | Every release |
| `@coree-ai/coree-win32-x64` | ~19 MB | Every release |

First install: ~136MB total. Subsequent binary-only updates: ~20MB (model skipped by lockfile).

## Updating the model package

When fastembed changes its default model:

1. Update `REPO_ID` and `FILES` in `scripts/fetch-model.py`
2. Update `const MODEL` in `src/embed.rs`
3. Rename `npm/@coree-ai/coree-model-bge-small-en-v1.5/` to the new model name
4. Update the `dependencies` entry in `npm/@coree-ai/coree/package.json`
5. Update the `require.resolve` path in `npm/@coree-ai/coree/bin/coree`
6. Publish the new model package stub on npmjs.com and add trusted publisher
7. Write a schema migration if `DIMS` changes in `src/embed.rs`

Note: the model package version is independent of the coree binary version. Pin it to
`1.0.0` (or `2.0.0` for the next model) rather than tracking the coree semver.

## Known limitations and future work

- **Windows untested**: the `coree-win32-x64` platform package is built but end-to-end
  install on Windows has not been verified.
- **`fetch-model.py` requires Python**: CI runners have Python pre-installed, but local
  builds without the tyto cache require `pip install huggingface_hub`. An alternative is
  to add a `coree download-model <dir>` subcommand to the binary itself.
- **End-to-end install not verified**: a fresh `/plugin install @coree-ai/coree` from a
  clean machine has not been tested. The pack output has been inspected and the structure
  confirmed correct, but a live install test is still outstanding.
- **Model package published at version `1.0.0` stub**: the trusted publisher is configured
  but the real model content is only published on the next tagged release.
- **`linux-arm64` and `darwin-arm64` untested locally**: cross-compiled, not run.
