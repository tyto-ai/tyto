#!/usr/bin/env bash
# Wrapper used by both .mcp.json and hooks to launch the memso binary.
# Honours MEMSO_BINARY_OVERRIDE for dev/custom builds; falls back to the
# binary installed by bootstrap.sh in CLAUDE_PLUGIN_DATA.
exec "${MEMSO_BINARY_OVERRIDE:-${CLAUDE_PLUGIN_DATA}/memso}" "$@"
