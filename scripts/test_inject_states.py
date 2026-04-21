#!/usr/bin/env python3
"""
Deterministic tests for inject loading/ready state behaviour.

Does NOT start tyto serve. Instead it manipulates serve.lock and serve.ready
directly, so the tests are non-flaky regardless of how fast the model loads.

Tests:
  1. Loading state  (Unix only): inject emits the first-run loading message
  2. Ready state:   inject emits the "MCP server is running" message
  3. Timing:        inject returns within 2 s in both states (non-blocking check)

Usage: test_inject_states.py <binary-path>
Exits 0 on success, 1 on any failure.
"""
import os
import subprocess
import sys
import tempfile
import time

TIMEOUT_S = 2.0  # inject must return within this many seconds


def run_inject(binary, cwd):
    """Run tyto inject --type session and return (stdout, elapsed_seconds)."""
    t0 = time.monotonic()
    result = subprocess.run(
        [binary, "inject", "--type", "session"],
        capture_output=True,
        text=True,
        cwd=cwd,
        timeout=30,
    )
    elapsed = time.monotonic() - t0
    return result.stdout, elapsed


def assert_timing(elapsed, label):
    assert elapsed < TIMEOUT_S, (
        f"{label}: inject took {elapsed:.2f}s, expected < {TIMEOUT_S}s. "
        f"inject must never block waiting for serve to become ready."
    )
    print(f"  timing: ok ({elapsed:.3f}s < {TIMEOUT_S}s)")


def main():
    if len(sys.argv) < 2:
        print("Usage: test_inject_states.py <binary>", file=sys.stderr)
        sys.exit(1)

    binary = os.path.abspath(sys.argv[1])
    if not os.path.isfile(binary):
        print(f"Binary not found: {binary}", file=sys.stderr)
        sys.exit(1)

    failures = []

    # ------------------------------------------------------------------ #
    # Test 1: Loading state (Unix only — uses fcntl exclusive file lock)  #
    # Windows lock semantics differ; the loading path is exercised there  #
    # implicitly by the MCP test retry loop in e2e_mcp.py.                #
    # ------------------------------------------------------------------ #
    if sys.platform != "win32":
        print("Test 1: inject during Loading state (Unix)")
        try:
            import fcntl

            with tempfile.TemporaryDirectory() as tmpdir:
                tyto_dir = os.path.join(tmpdir, ".tyto")
                os.makedirs(tyto_dir)

                # mode=local forces serve.lock/serve.ready into .tyto/ inside
                # the tmpdir, making the path predictable regardless of platform.
                with open(os.path.join(tmpdir, ".tyto.toml"), "w") as f:
                    f.write('project_id = "inject-state-test"\n'
                            '[memory]\nmode = "local"\nlocal_path = ".tyto/memory.db"\n')

                lock_path = os.path.join(tyto_dir, "serve.lock")
                # serve.ready intentionally absent — this is the Loading state.

                with open(lock_path, "w") as lock_file:
                    fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)

                    output, elapsed = run_inject(binary, tmpdir)

                # Lock released when 'with' exits.

                assert_timing(elapsed, "loading-state")

                # inject must tell the agent about the loading state.
                loading_keywords = ["starting up", "embedding model", "session_context"]
                missing = [kw for kw in loading_keywords if kw not in output]
                assert not missing, (
                    f"Loading message missing keywords: {missing}\nActual output:\n{output}"
                )
                print(f"  loading message: ok (keywords found: {loading_keywords})")

        except Exception as exc:
            failures.append(f"Test 1 (loading state): {exc}")
            print(f"  FAILED: {exc}")
    else:
        print("Test 1: Loading state — skipped on Windows (covered by MCP retry loop)")

    # ------------------------------------------------------------------ #
    # Test 2: Ready state (all platforms — just touches serve.ready)      #
    # ------------------------------------------------------------------ #
    print("Test 2: inject during Ready state")
    try:
        with tempfile.TemporaryDirectory() as tmpdir:
            tyto_dir = os.path.join(tmpdir, ".tyto")
            os.makedirs(tyto_dir)

            with open(os.path.join(tmpdir, ".tyto.toml"), "w") as f:
                f.write('project_id = "inject-state-test"\n'
                        '[memory]\nmode = "local"\nlocal_path = ".tyto/memory.db"\n')

            # Write serve.ready — signals that serve is fully up.
            open(os.path.join(tyto_dir, "serve.ready"), "w").close()

            output, elapsed = run_inject(binary, tmpdir)

        assert_timing(elapsed, "ready-state")

        assert "MCP server is running" in output, (
            f"Expected 'MCP server is running' in ready-state output.\nActual:\n{output}"
        )
        print("  ready message: ok")

    except Exception as exc:
        failures.append(f"Test 2 (ready state): {exc}")
        print(f"  FAILED: {exc}")

    # ------------------------------------------------------------------ #
    # Result                                                               #
    # ------------------------------------------------------------------ #
    if failures:
        print(f"\n{len(failures)} test(s) failed:")
        for f in failures:
            print(f"  - {f}")
        sys.exit(1)

    print("\nInject state tests passed")
    sys.exit(0)


if __name__ == "__main__":
    main()
