#!/usr/bin/env python3
"""Smoke tests for shutdown-event coalescing and the shared import lock."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[2]
RUNNER = ROOT / "scripts" / "ctx-remote-hook-sync.py"


def wait_until(predicate, timeout: float = 8.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError("timed out waiting for hook worker")


def main() -> int:
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        state = root / "state"
        calls = root / "calls"
        remote = root / "ctx-remote"
        remote.write_text(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CTX_TEST_CALLS\"\nsleep 1\n",
            encoding="utf-8",
        )
        remote.chmod(0o755)
        env = os.environ.copy()
        env.update(
            {
                "CTX_REMOTE_BIN": str(remote),
                "CTX_TEST_CALLS": str(calls),
                "CTX_TURSO_STATE_DIR": str(state),
                "CTX_REMOTE_HOOK_DEBOUNCE_SECONDS": "0.4",
                "CTX_REMOTE_HOOK_TIMEOUT_SECONDS": "5",
            }
        )
        processes = [
            subprocess.Popen([sys.executable, str(RUNNER), "request", source], env=env)
            for source in ("claude", "codex", "qwen-code")
        ]
        assert all(process.wait(timeout=3) == 0 for process in processes)
        wait_until(lambda: (state / "hook-sync-status.json").exists())
        wait_until(lambda: not (state / "hook-sync.pending").exists())
        time.sleep(0.2)
        lines = calls.read_text(encoding="utf-8").splitlines()
        assert lines == ["import --batch-size 250"], lines

        def lock_is_clear() -> bool:
            probe = subprocess.run(
                [sys.executable, str(RUNNER), "locked-exec", "true"], env=env, check=False
            )
            return probe.returncode == 0

        wait_until(lock_is_clear)
        holder = subprocess.Popen(
            [sys.executable, str(RUNNER), "locked-exec", "sleep", "1"], env=env
        )
        time.sleep(0.15)
        busy = subprocess.run(
            [sys.executable, str(RUNNER), "locked-exec", "true"], env=env, check=False
        )
        assert busy.returncode == 75, busy.returncode
        assert holder.wait(timeout=3) == 0
    print("ctx-remote hook sync tests passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
