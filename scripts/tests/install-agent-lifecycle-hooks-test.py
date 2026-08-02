#!/usr/bin/env python3
"""Verify lifecycle hook installation preserves existing agent settings."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[2]
INSTALLER = ROOT / "scripts" / "install-agent-lifecycle-hooks.py"


def write_json(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding="utf-8")


def commands(config: dict, event: str) -> list[str]:
    return [
        hook["command"]
        for group in config.get("hooks", {}).get(event, [])
        for hook in group.get("hooks", [])
        if "command" in hook
    ]


def main() -> int:
    with tempfile.TemporaryDirectory() as temporary:
        home = Path(temporary)
        write_json(
            home / ".claude" / "settings.json",
            {"hooks": {"SessionEnd": [{"hooks": [{"type": "command", "command": "keep-me"}]}]}},
        )
        write_json(home / ".qwen" / "settings.json", {"ui": {"theme": "dark"}})
        retain = home / ".hindsight" / "codex" / "scripts" / "retain.py"
        retain.parent.mkdir(parents=True)
        retain.write_text("import os\n", encoding="utf-8")
        env = os.environ.copy()
        env["HOME"] = str(home)
        for _ in range(2):
            subprocess.run([sys.executable, str(INSTALLER)], env=env, check=True)

        claude = json.loads((home / ".claude" / "settings.json").read_text(encoding="utf-8"))
        qwen = json.loads((home / ".qwen" / "settings.json").read_text(encoding="utf-8"))
        claude_end = commands(claude, "SessionEnd")
        qwen_start = commands(qwen, "SessionStart")
        qwen_end = commands(qwen, "SessionEnd")
        assert claude_end.count("keep-me") == 1
        assert claude_end.count("~/.local/bin/ctx-remote hook-sync claude") == 1
        assert len(qwen_start) == 1
        assert qwen_end.count("~/.local/bin/ctx-remote hook-sync qwen-code") == 1
        assert qwen["ui"]["theme"] == "dark"
        assert retain.read_text(encoding="utf-8").count("# ctx-remote lifecycle hook") == 1
        assert not (home / ".codex" / "hooks.json").exists()
        assert not (home / ".config" / "opencode" / "plugins" / "ctx-remote.ts").exists()
    print("agent lifecycle hook installer tests passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
