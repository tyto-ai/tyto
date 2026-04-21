#!/usr/bin/env python3
"""
MCP E2E smoke test for tyto.

Starts `tyto serve`, sends MCP messages over stdio (NDJSON), verifies:
  1. initialize handshake
  2. store_memories stores a record
  3. search_memory retrieves the stored record

Usage: e2e_mcp.py <binary-path>
Exits 0 on success, 1 on any failure.
"""
import json
import os
import subprocess
import sys
import tempfile
import time

# Substrings that mean "DB not ready yet, retry"
_TRANSIENT = ("syncing", "loading", "initializ", "replicat", "not ready")


def send(proc, msg):
    proc.stdin.write((json.dumps(msg) + "\n").encode())
    proc.stdin.flush()
    line = proc.stdout.readline()
    if not line:
        stderr = proc.stderr.read().decode(errors="replace")
        raise RuntimeError(f"Server closed stdout.\nstderr: {stderr}")
    return json.loads(line)


def is_transient(resp):
    """True if the response signals a retryable not-ready state."""
    # Top-level JSON-RPC error
    if "error" in resp:
        return any(w in str(resp["error"]).lower() for w in _TRANSIENT)
    # MCP application-level error (isError inside result)
    result = resp.get("result", {})
    if result.get("isError"):
        return any(w in json.dumps(result).lower() for w in _TRANSIENT)
    return False


def call_tool(proc, id_base, name, arguments, max_attempts=120, sleep=0.5):
    """Call an MCP tool, retrying on transient not-ready responses."""
    for attempt in range(max_attempts):
        resp = send(proc, {
            "jsonrpc": "2.0",
            "id": id_base + attempt,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        })
        if is_transient(resp):
            time.sleep(sleep)
            continue
        if "error" in resp:
            raise RuntimeError(f"{name} JSON-RPC error: {resp}")
        result = resp.get("result", {})
        if result.get("isError"):
            raise RuntimeError(f"{name} tool error: {resp}")
        return resp, attempt + 1
    raise RuntimeError(f"{name} never succeeded after {max_attempts} attempts. Last: {resp}")


def main():
    if len(sys.argv) < 2:
        print("Usage: e2e_mcp.py <binary>", file=sys.stderr)
        sys.exit(1)

    binary = os.path.abspath(sys.argv[1])
    if not os.path.isfile(binary):
        print(f"Binary not found: {binary}", file=sys.stderr)
        sys.exit(1)

    with tempfile.TemporaryDirectory() as tmpdir:
        with open(os.path.join(tmpdir, ".tyto.toml"), "w") as f:
            f.write('project_id = "e2e-smoke"\n')

        proc = subprocess.Popen(
            [binary, "serve"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=tmpdir,
        )

        try:
            # 1. Initialize
            resp = send(proc, {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "e2e", "version": "1.0"},
                },
            })
            assert "result" in resp, f"initialize failed: {resp}"
            print(f"  initialize: ok (server: {resp['result'].get('serverInfo', {}).get('name', '?')})")

            # initialized notification - no response expected
            proc.stdin.write((json.dumps({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
            }) + "\n").encode())
            proc.stdin.flush()

            # 2. store_memories (retry until DB is ready - fastembed model may still be loading)
            _, attempts = call_tool(proc, id_base=2, name="store_memories", arguments={
                "memories": [{
                    "content": "E2E cold-start smoke test: bootstrap installed correctly",
                    "type": "decision",
                    "title": "e2e-bootstrap-smoke",
                }]
            })
            print(f"  store_memories: ok (attempt {attempts})")

            # 3. search_memory (same retry budget - embedder may lag behind store)
            resp, attempts = call_tool(proc, id_base=200, name="search_memory", arguments={
                "query": "bootstrap smoke test installed",
            })
            result_text = json.dumps(resp.get("result", {}))
            assert "e2e-bootstrap-smoke" in result_text, (
                f"Stored memory not found in search results.\nResponse: {resp}"
            )
            print(f"  search_memory: ok (stored memory found, attempt {attempts})")

        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()

    print("E2E MCP smoke test passed")
    sys.exit(0)


if __name__ == "__main__":
    main()
