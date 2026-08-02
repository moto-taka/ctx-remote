#!/usr/bin/env python3
"""Recall shared Hindsight memory for agent session-start hooks."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import sys


def read_input() -> dict:
    if sys.stdin.isatty():
        return {}
    try:
        return json.load(sys.stdin)
    except (EOFError, json.JSONDecodeError):
        return {}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--plain", action="store_true")
    parser.add_argument("--hook-event", default="SessionStart")
    parser.add_argument("--cwd")
    parser.add_argument("--session-id")
    parser.add_argument("--provider", default="agent")
    args = parser.parse_args()
    hook_input = read_input()
    cwd = args.cwd or hook_input.get("cwd") or os.getcwd()
    session_id = args.session_id or hook_input.get("session_id") or "startup"

    scripts = Path("~/.hindsight/codex/scripts").expanduser()
    if not scripts.is_dir():
        return 0
    sys.path.insert(0, str(scripts))
    try:
        from lib.bank import derive_bank_id
        from lib.client import HindsightClient
        from lib.config import load_config
        from lib.content import format_current_time, format_memories
        from lib.daemon import get_api_url
    except (ImportError, OSError):
        return 0

    config = load_config()
    if not config.get("autoRecall"):
        return 0
    try:
        api_url = get_api_url(config, allow_daemon_start=False)
        client = HindsightClient(api_url, config.get("hindsightApiToken"))
        bank_id = derive_bank_id({"cwd": cwd, "session_id": session_id}, config)
        project = Path(cwd).name or "current project"
        query = (
            f"Recall durable decisions, preferences, gotchas, and unfinished work "
            f"needed to resume project {project} in {args.provider}."
        )
        response = client.recall(
            bank_id=bank_id,
            query=query,
            max_tokens=config.get("recallMaxTokens", 1024),
            budget=config.get("recallBudget", "mid"),
            types=config.get("recallTypes"),
            timeout=config.get("recallTimeout", 10),
        )
    except Exception as error:
        if os.environ.get("HINDSIGHT_DEBUG") == "1":
            print(f"hindsight session recall failed: {error}", file=sys.stderr)
        return 0

    results = response.get("results", [])
    if not results:
        return 0
    context = (
        "<hindsight_memories>\n"
        f"{config.get('recallPromptPreamble', 'Relevant memories:')}\n"
        f"Current time - {format_current_time()}\n\n"
        f"{format_memories(results)}\n"
        "</hindsight_memories>"
    )
    if args.plain:
        print(context)
    else:
        json.dump(
            {
                "hookSpecificOutput": {
                    "hookEventName": args.hook_event,
                    "additionalContext": context,
                }
            },
            sys.stdout,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
