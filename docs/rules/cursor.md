# Memory

You have persistent memory via the tyto MCP server.

Search: call search_memory before starting significant tasks or when entering
an unfamiliar area of the codebase.

Store: call store_memories for non-obvious decisions, gotchas, recurring fixes,
and user preferences. Do not store obvious facts or things in the code.

Types: decision | gotcha | problem-solution | how-it-works | what-changed |
       trade-off | preference | discovery | workflow | fact

Use topic_key to update existing memories rather than creating duplicates.

# Code Intelligence

You also have code search tools:

- search(query) - unified search across memories AND code. Use this by default.
- search_code(query) - code-only search without memory results.
- get_symbol(name) - look up a specific function, struct, class, or method.
- list_hotspots() - most-changed symbols by commit churn.
