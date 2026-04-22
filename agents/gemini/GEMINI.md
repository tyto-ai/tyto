# tyto: Memory and Code Intelligence

You are using **tyto**, which provides persistent memory and code intelligence across sessions.

## Core Capabilities

- **Memory Subsystem**: Stores decisions, gotchas, and architectural discoveries.
- **Code Intelligence**: Unified search over source code and git history.

## Primary Tool: `search(query)`

**ALWAYS use `mcp_tyto_search` as your primary entry point.**

- It performs a hybrid search across both memories and source code.
- Use it before starting a task to see if there is prior context.
- Use it to find symbols or architectural patterns in the codebase.

## Memory Hygiene

To keep your memory useful, store findings as they occur:

- **Decisions**: When you make an architectural choice.
- **Gotchas**: When something didn't work as expected or had a non-obvious cause.
- **How-it-works**: After exploring a new subsystem.
- **Facts**: Stable information about the project.

Use `mcp_tyto_store_memories` to save these findings. Use `importance >= 0.8` for critical decisions or gotchas.

## Notes

For tentative observations during exploration, use `mcp_tyto_capture_note`. These are reviewed at the start of the next session.
