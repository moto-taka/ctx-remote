#!/usr/bin/env python3
"""Coalesce agent shutdown events into bounded remote-primary imports."""

from __future__ import annotations

import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import time


VALID_SOURCES = {"claude", "codex", "qwen-code"}
LOCK_BUSY = 75


def source_targets(source: str) -> list[tuple[str, Path]]:
    home = Path.home()
    targets = {
        "claude": [
            ("claude", home / ".claude" / "projects"),
            ("claude", home / ".claude-sapeet" / "projects"),
        ],
        "codex": [("codex", home / ".codex" / "sessions")],
        "qwen-code": [("qwen-code", home / ".qwen" / "projects")],
    }
    return [(provider, path) for provider, path in targets[source] if path.exists()]


def state_dir() -> Path:
    return Path(os.environ.get("CTX_TURSO_STATE_DIR", "~/.local/state/ctx-remote")).expanduser()


def hook_binary() -> str:
    configured = os.environ.get("CTX_REMOTE_BIN")
    if configured:
        return configured
    candidate = Path("~/.local/bin/ctx-remote").expanduser()
    return str(candidate) if candidate.is_file() else "ctx-remote"


def acquire_lock(blocking: bool = False):
    root = state_dir()
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    handle = (root / "import.lock").open("a+")
    flags = fcntl.LOCK_EX if blocking else fcntl.LOCK_EX | fcntl.LOCK_NB
    try:
        fcntl.flock(handle.fileno(), flags)
    except BlockingIOError:
        handle.close()
        return None
    return handle


def write_status(result: str, source: str, attempts: int, error: str = "") -> None:
    root = state_dir()
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    status = root / "hook-sync-status.json"
    temporary = root / f"hook-sync-status.{os.getpid()}.tmp"
    home = str(Path.home())
    safe_error = error.replace(home, "~")[:500]
    payload = {
        "updated_at": int(time.time()),
        "result": result,
        "source": source,
        "attempts": attempts,
        "error": safe_error,
    }
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as stream:
        json.dump(payload, stream, separators=(",", ":"))
        stream.write("\n")
    os.replace(temporary, status)


def request(source: str) -> int:
    if source not in VALID_SOURCES:
        print(f"unsupported hook source: {source}", file=sys.stderr)
        return 2
    root = state_dir()
    pending = root / "hook-sync.pending.d"
    pending.mkdir(parents=True, exist_ok=True, mode=0o700)
    temporary = root / f"hook-sync.{os.getpid()}.pending"
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as stream:
        stream.write(f"{source}\n")
    os.replace(temporary, pending / source)
    if os.environ.get("CTX_REMOTE_HOOK_FOREGROUND") == "1":
        return worker()
    subprocess.Popen(
        [sys.executable, os.path.abspath(__file__), "worker"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
        close_fds=True,
    )
    return 0


def run_target(provider: str, path: Path) -> tuple[bool, int, str]:
    command = [
        hook_binary(),
        "turso",
        "import",
        "--provider",
        provider,
        "--path",
        str(path),
        "--batch-size",
        os.environ.get("CTX_TURSO_BATCH_SIZE", "250"),
        "--quiet",
        "--json",
    ]
    timeout = int(os.environ.get("CTX_REMOTE_HOOK_TIMEOUT_SECONDS", "300"))
    last_error = ""
    for attempt in range(1, 4):
        try:
            result = subprocess.run(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
                timeout=timeout,
                check=False,
            )
            if result.returncode == 0:
                return True, attempt, ""
            last_error = result.stderr.strip() or f"ctx-remote exited {result.returncode}"
        except subprocess.TimeoutExpired:
            last_error = f"ctx-remote import exceeded {timeout}s"
        if attempt < 3:
            time.sleep(attempt)
    return False, 3, last_error


def run_import(source: str) -> bool:
    targets = source_targets(source)
    if not targets:
        (state_dir() / f"hook-sync.{source}.failed").unlink(missing_ok=True)
        write_status("skipped", source, 0)
        return True
    attempts = 0
    for provider, path in targets:
        succeeded, target_attempts, error = run_target(provider, path)
        attempts += target_attempts
        if not succeeded:
            write_status("error", source, attempts, error)
            return False
    (state_dir() / f"hook-sync.{source}.failed").unlink(missing_ok=True)
    (state_dir() / "hook-sync.failed").unlink(missing_ok=True)
    write_status("ok", source, attempts)
    return True


def worker() -> int:
    lock = acquire_lock(blocking=True)
    pending = state_dir() / "hook-sync.pending.d"
    try:
        time.sleep(float(os.environ.get("CTX_REMOTE_HOOK_DEBOUNCE_SECONDS", "0.25")))
        for cycle in range(2):
            markers = sorted(path for path in pending.glob("*") if path.name in VALID_SOURCES)
            if not markers:
                return 0
            for marker in markers:
                source = marker.name
                working = state_dir() / f"hook-sync.{os.getpid()}.{cycle}.{source}.working"
                try:
                    os.replace(marker, working)
                except FileNotFoundError:
                    continue
                if not run_import(source):
                    failed = state_dir() / f"hook-sync.{source}.failed"
                    os.replace(working, failed)
                    return 1
                working.unlink(missing_ok=True)
        return 0
    finally:
        lock.close()


def locked_exec(arguments: list[str]) -> int:
    if not arguments:
        print("locked-exec requires a command", file=sys.stderr)
        return 2
    lock = acquire_lock()
    if lock is None:
        return LOCK_BUSY
    try:
        return subprocess.run(arguments, check=False).returncode
    finally:
        lock.close()


def main() -> int:
    if len(sys.argv) >= 3 and sys.argv[1] == "request":
        return request(sys.argv[2])
    if len(sys.argv) == 2 and sys.argv[1] == "worker":
        return worker()
    if len(sys.argv) >= 3 and sys.argv[1] == "locked-exec":
        return locked_exec(sys.argv[2:])
    print("usage: ctx-remote-hook-sync request SOURCE | worker | locked-exec COMMAND...", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
