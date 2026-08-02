#!/usr/bin/env python3
"""Install ctx-remote lifecycle hooks without replacing existing hooks."""

from __future__ import annotations

import json
import os
from pathlib import Path


def load_json(path: Path) -> dict:
    if not path.exists():
        return {}
    with path.open(encoding="utf-8") as stream:
        return json.load(stream)


def save_json(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    with temporary.open("w", encoding="utf-8") as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write("\n")
    os.chmod(temporary, 0o600)
    os.replace(temporary, path)


def add_command_hook(
    config: dict,
    event: str,
    command: str,
    *,
    async_hook: bool = False,
    aliases: tuple[str, ...] = (),
) -> None:
    groups = config.setdefault("hooks", {}).setdefault(event, [])
    for group in groups:
        for hook in group.get("hooks", []):
            if hook.get("command") in (command, *aliases):
                hook.update({"type": "command", "command": command, "timeout": 5000})
                if async_hook:
                    hook["async"] = True
                return
    hook = {"type": "command", "command": command, "timeout": 5000}
    if async_hook:
        hook["async"] = True
    groups.append({"hooks": [hook]})


def install_claude(home: Path) -> None:
    path = home / ".claude" / "settings.json"
    config = load_json(path)
    add_command_hook(
        config,
        "SessionEnd",
        "~/.local/bin/ctx-remote hook-sync claude",
        async_hook=True,
        aliases=("ctx-remote hook-sync claude",),
    )
    save_json(path, config)


def install_qwen(home: Path) -> None:
    path = home / ".qwen" / "settings.json"
    config = load_json(path)
    add_command_hook(
        config,
        "SessionStart",
        "~/.local/libexec/ctx/hindsight-session-context --hook-event SessionStart --provider qwen-code",
    )
    add_command_hook(
        config,
        "SessionEnd",
        "~/.local/bin/ctx-remote hook-sync qwen-code",
        async_hook=True,
        aliases=("ctx-remote hook-sync qwen-code",),
    )
    save_json(path, config)


def install_codex(home: Path) -> None:
    retain = home / ".hindsight" / "codex" / "scripts" / "retain.py"
    if not retain.is_file():
        return
    marker = "# ctx-remote lifecycle hook"
    content = retain.read_text(encoding="utf-8")
    if marker in content:
        return
    addition = f'''\n\n{marker}\ntry:\n    import subprocess as _ctx_subprocess\n    _ctx_remote = os.path.expanduser("~/.local/bin/ctx-remote")\n    if os.path.isfile(_ctx_remote):\n        _ctx_subprocess.Popen(\n            [_ctx_remote, "hook-sync", "codex"],\n            stdin=_ctx_subprocess.DEVNULL,\n            stdout=_ctx_subprocess.DEVNULL,\n            stderr=_ctx_subprocess.DEVNULL,\n            start_new_session=True,\n        )\nexcept Exception:\n    pass\n'''
    retain.write_text(content + addition, encoding="utf-8")


def main() -> int:
    home = Path.home()
    install_claude(home)
    install_qwen(home)
    install_codex(home)
    print("installed ctx-remote lifecycle hooks for Claude, Codex, and Qwen Code")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
