## Persistent Memory

You have persistent memory across sessions via the tyto MCP server.

### Retrieving context

At the start of every session, call:
  search_memory(query="project overview conventions decisions", project_id="<project>")

Before starting a significant task or when entering an unfamiliar area, call:
  search_memory(query="<relevant topic>")

Call get_memories(ids=[<id>]) to read the full content of any result that looks relevant.

### Storing memories

Call store_memories when you:
- Learn something non-obvious about this project's architecture or conventions
- Make or discover an architectural decision or trade-off
- Find the solution to a non-obvious or recurring problem
- Encounter a gotcha (something that breaks in a non-obvious way)
- Observe a preference or constraint the user has expressed

Do NOT store: obvious facts, temporary state, things derivable from reading the
code, or anything already documented in CLAUDE.md.

### Field guidance

- type: choose the most specific type from:
  decision, gotcha, problem-solution, how-it-works, what-changed,
  trade-off, preference, discovery, workflow, fact
- topic_key: a short stable slug for the subject, e.g. "auth-session-store",
  "time-crate-policy". Memories with the same topic_key are updated in place
  rather than duplicated.
- importance: 0.0-1.0
  - 0.9+ for architectural decisions affecting the whole system
  - 0.7+ for gotchas, non-obvious constraints, security-relevant facts
  - 0.5  for useful context, patterns, preferences
  - 0.3  for supplementary facts
- facts: array of short discrete statements, e.g.
  ["Uses tower-sessions-sqlx-store 0.15.0", "PostgresStore auto-migrates on startup"]

## Code Intelligence

You also have four code intelligence tools that search the indexed source code:

- search(query) - unified search across memories AND code simultaneously. Use this by default.
- search_code(query) - code-only search when memory results would add noise.
- get_symbol(name, file_path?) - look up a specific function, struct, class, or method.
- list_hotspots(min_churn?, limit?) - most-changed symbols; use before touching volatile areas.

search() degrades gracefully to memory-only if the index is not yet ready.
The index builds in the background on startup; tools return empty code results during the first build.
