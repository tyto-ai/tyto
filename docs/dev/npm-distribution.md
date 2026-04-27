# npm Distribution Plan (issue #26, milestone v0.9.0)

Distribute coree as a proper npm package using the `optionalDependencies` pattern.
Reference implementations studied: **biome** (`~/Development/biome`) and **esbuild** (`~/Development/esbuild`).
Biome is the primary reference - pure `optionalDependencies`, no postinstall download, `spawnSync` entrypoint.

## Why this matters

Plugin installers (Claude Code, Gemini, Codex) copy only the plugin source directory into a
local cache. Anything outside that directory is absent at runtime. The current `agents/shared/`
layout breaks installed plugins. npm distribution solves this and eliminates lazy downloads.

## Repo layout (single repo, same as biome/esbuild)

```
npm/
  @coree-ai/
    coree/                   <- main package (files live in repo)
      package.json
      bin/
        coree                <- Node.js entrypoint (platform resolver)
      .claude-plugin/
        plugin.json
      .mcp.json
      hooks/
        hooks.json
      model/                 <- empty in repo; fastembed ONNX model bundled by CI at release
      THIRD_PARTY_NOTICES    <- BAAI model MIT copyright notice
    coree-linux-x64/         <- platform packages: only package.json lives in repo
      package.json
    coree-linux-arm64/
      package.json
    coree-darwin-arm64/
      package.json
    coree-win32-x64/
      package.json
scripts/
  generate-npm-packages.mjs  <- copies binaries + model into packages, updates versions
```

## `npm/@coree-ai/coree/bin/coree`

Derived from `packages/@biomejs/biome/bin/biome` in the biome repo.

`spawnSync` with `stdio: 'inherit'` is correct for an MCP server - the child inherits
stdin/stdout for the JSON-RPC stream and the parent blocks until exit.

## `.mcp.json` after npm distribution

```json
{
  "mcpServers": {
    "coree": {
      "command": "node",
      "args": ["${CLAUDE_PLUGIN_ROOT}/bin/coree", "serve"]
    }
  }
}
```

## `scripts/generate-npm-packages.mjs`

Derived from `packages/@biomejs/biome/scripts/generate-packages.mjs`.
Copies platform binaries from `dist/` into platform package dirs, updates all versions,
copies model into main package `model/`.

## `release.yml` additions - `publish-npm` job

Added to `.github/workflows/release.yml`. Uses npm trusted publishing (OIDC) -
no `NPM_TOKEN` secret needed. The `id-token: write` permission allows GitHub Actions
to mint an OIDC token that npm exchanges for a publish token automatically.
Requires trusted publisher configured on npmjs.com for each package (link to
`coree-ai/coree` repo, `release.yml` workflow, `npm-publish` environment).

## Rust changes made

### `src/embed.rs` - `COREE_MODEL_DIR` support

When `COREE_MODEL_DIR` is set, use it as the model cache dir instead of the default
`~/.cache/coree/models/`. The bundled npm package sets this to `./model/` where the
ONNX files are pre-bundled at release time.

### `src/config.rs` - `COREE__PROJECT_ROOT` support

- `project_root` field: changed from `PathBuf` with `#[serde(skip)]` to `Option<PathBuf>`
  with `#[serde(default)]`.
- `Config::load()`: two-pass Figment read - extract `project_root` first (from global config
  + env vars), then use as `start_dir` for `.coree.toml` discovery.
- Added `project_root()` accessor: always `Some` after `Config::load()`, panics otherwise.
- Added `validate_project_root()`: checks path is absolute and exists as a directory.

## Codex plugin: why `bin/coree` solves the project root problem

Codex spawns MCP servers with `cwd` = plugin cache (not project dir). Shell scripts
reset `PWD` to match actual cwd before any code runs. But **Node.js does NOT reset PWD**.

```
# Shell (cwd=/plugin/cache, env PWD=/project):
# PWD gets reset to /plugin/cache before first line runs

# Node.js (cwd=/plugin/cache, env PWD=/project):
# process.env.PWD == '/project'  <- preserved
# process.cwd()   == '/plugin/cache'
```

`bin/coree` detects this mismatch and sets `COREE__PROJECT_ROOT = process.env.PWD` before
spawning the binary. Zero config for the user.

Codex `.mcp.json` must whitelist `PWD` in `env_vars` for this to work:

```json
{
  "mcpServers": {
    "coree": {
      "command": "./bin/coree",
      "args": ["serve"],
      "cwd": ".",
      "env_vars": ["PWD", "COREE_BINARY_OVERRIDE", "COREE_CHANNEL",
                   "COREE__MEMORY__REMOTE_AUTH_TOKEN", "RUST_LOG"]
    }
  }
}
```

## Agent plugin launch comparison

| | Claude Code | Gemini | Codex |
|---|---|---|---|
| Command interpolation | `${CLAUDE_PLUGIN_ROOT}` | `${extensionPath}` | None |
| Needs `cwd` field | No | No | Yes (`"."`) |
| MCP server cwd | Session dir (project) | Session dir (project) | Plugin cache |
| PWD available | N/A | N/A | Yes (if whitelisted) |
| Shell resets PWD | Yes, but cwd is already correct | Yes, but cwd is already correct | Yes, losing project dir |
| Fix needed | None | None | `bin/coree` sets `COREE__PROJECT_ROOT` from `PWD` |

## Licensing

- **coree**: Apache-2.0 (`LICENSE` file, already exists)
- **Bundled BAAI/bge-small-en-v1.5 model**: MIT - only requirement is preserving copyright notice
- **`THIRD_PARTY_NOTICES`** (new file in main npm package):
  - Lists BAAI model with full MIT license text
  - Added to `files` array in `package.json`
- No Apache-2.0 `NOTICE` file required (only needed when redistributing upstream components that had one)

## Model cache structure

fastembed writes the HuggingFace hub cache format to `COREE_MODEL_DIR`:

```
models--Xenova--bge-small-en-v1.5/
  blobs/          <- actual file content, named by sha hash
  refs/main       <- commit hash of the snapshot used
  snapshots/<commit>/
    config.json              -> symlink to blobs/<sha>
    tokenizer.json           -> symlink to blobs/<sha>
    tokenizer_config.json    -> symlink to blobs/<sha>
    special_tokens_map.json  -> symlink to blobs/<sha>
    onnx/
      model.onnx             -> symlink to blobs/<sha256>  (~127MB)
```

In CI, `python3 -c "from huggingface_hub import snapshot_download; snapshot_download(repo_id='Xenova/bge-small-en-v1.5', cache_dir='dist/model')"` creates this exact structure. `generate-npm-packages.mjs` copies it with `verbatimSymlinks: true` to preserve the symlinks. The main package has `preferUnplugged: true` so pnpm/yarn don't zip it.

## Implementation checklist

- [x] `src/embed.rs`: add `COREE_MODEL_DIR` env var support
- [x] `src/config.rs`: `project_root` as `Option<PathBuf>`, `COREE__PROJECT_ROOT` support
- [x] Create `npm/@coree-ai/coree/` with `bin/coree`, `package.json`, `.claude-plugin/`, `.mcp.json`, `hooks/`, `THIRD_PARTY_NOTICES`
- [x] Create `npm/@coree-ai/coree-{linux-x64,linux-arm64,darwin-arm64,win32-x64}/package.json`
- [x] Create `scripts/generate-npm-packages.mjs`
- [x] Extend `release.yml` with `publish-npm` job
- [x] Update `.claude-plugin/marketplace.json` to npm source
- [x] Resolve model fetch in CI (huggingface_hub snapshot_download with cache_dir)
- [ ] Configure trusted publisher on npmjs.com for all 5 packages (link to coree-ai/coree, release.yml, npm-publish env)
- [ ] End-to-end verify: fresh install via `/plugin install`, confirm no lazy downloads
