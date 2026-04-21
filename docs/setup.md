# Setup Guide

## 1. Build and install the binary

```bash
cargo install --path .
```

This puts `tyto` in `~/.cargo/bin/`. Make sure that is in your `$PATH`.

## 2. Configure Claude Code

```bash
tyto install
```

This adds the MCP server and hooks to `~/.claude/settings.json` automatically.
It is safe to run multiple times - already-configured entries are skipped.

Use `tyto install --dry-run` to preview changes before writing.

Restart Claude Code after running install.

## 3. Add memory instructions to your project's CLAUDE.md

Copy the contents of `docs/rules/CLAUDE.md` into your project's CLAUDE.md,
replacing `<project>` with your project name.

## 4. Per-project scoping

Create a `.tyto.toml` in your project root with a `project_id`:

```toml
project_id = "my-project"
```

Or set `TYTO__PROJECT_ID` in your environment.

## 5. Optional: enable Turso Cloud sync

To sync memories across machines, add to your `.tyto.toml`:

```toml
project_id = "my-project"

[memory]
mode = "remote"
remote_mode = "replica"
remote_url = "libsql://your-db.turso.io"
```

Set `TYTO__MEMORY__REMOTE_AUTH_TOKEN` in your environment (e.g. via `.envrc` with direnv).

The `.tyto.toml` can be committed; keep the auth token in the environment only.

## Memory storage

By default, tyto stores memories in the platform data directory, keyed by project path:

- Linux: `~/.local/share/tyto/managed/-home-user-myproject/memory.db`
- macOS: `~/Library/Application Support/tyto/managed/-home-user-myproject/memory.db`
- Windows: `%APPDATA%\tyto\managed\-home-user-myproject\memory.db`

To store in the project directory instead:

```toml
[memory]
mode = "local"
local_path = ".tyto/memory.db"
```

## Code intelligence

tyto automatically indexes your source code on startup, giving agents four additional tools:

- `search` — unified search across memories **and** code simultaneously (recommended default)
- `search_code` — code-only hybrid search (vector + BM25) without memory results
- `get_symbol` — look up a specific function, struct, class, or method by name
- `list_hotspots` — list the most frequently-changed symbols (commit churn)

Indexing runs in the background after startup. Tools return empty results during the first
index build and populate as files are processed.

The index database is stored outside the project directory at:
- Linux: `~/.local/share/tyto/managed/-home-user-myproject/index.db`
- macOS: `~/Library/Application Support/tyto/managed/-home-user-myproject/index.db`
- Windows: `%APPDATA%\tyto\managed\-home-user-myproject\index.db`

To disable indexing or exclude paths, add to `.tyto.toml`:

```toml
[index]
mode = "disabled"    # disable entirely
git_history = false  # skip git commit history (faster on large repos)
exclude = [
    "vendor/**",
    "third_party/**",
]
```
