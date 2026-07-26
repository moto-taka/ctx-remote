---
name: ctx-agent-history-search
description: Use ctx to search local or Turso remote-primary coding-agent history before acting. Use when prior agent sessions may contain relevant insights, decisions, attempts, or transcript context.
---

# ctx Agent History Search

Use ctx whenever you need to reference previous coding-agent sessions. Those
transcripts can contain user intent, decisions, previous work timelines, past
attempts, and what worked or failed.

Use this skill in two modes:

- retrieval before work, when prior sessions may contain decisions, commands,
  failures, or source citations that affect the current task;
- history research reports, when the user asks an agent or read-only subagent to
  research a historical topic across prior local agent sessions.

## Prerequisites

- Require the `ctx` CLI. If it is missing and installing tools is appropriate
  for the task, install it with:

  ```bash
  curl -fsSL https://ctx.rs/install | sh
  ```

- Do not run `ctx setup`, `ctx index`, or `ctx import` merely because local
  status reports zero events. Those commands create or update a local ctx
  SQLite index.
- If ctx remains unavailable, say history search is unavailable and do not
  invent results.

## Backend Selection

Perform this check before any status, setup, or search command:

```bash
if [ -n "${CTX_TURSO_DATABASE_URL:-}" ] && [ -n "${CTX_TURSO_AUTH_TOKEN:-}" ]; then
  echo remote-env
elif [ -r "${HOME}/.config/ctx/turso-auto-sync.env" ]; then
  echo remote-service
elif [ -n "${CTX_TURSO_DATABASE_URL:-}" ]; then
  echo remote-env-incomplete
else
  echo local
fi
```

Treat either remote result as authoritative:

- `remote-env`: use `ctx`; its top-level `status`, `search`, and `show`
  commands route to remote-primary.
- `remote-service`: use `ctx-remote`, installed by the fork's Turso service.
  It reads only the machine-local non-secret database configuration, creates a
  one-day token without printing it, and runs ctx in remote-primary mode.
- `remote-env-incomplete`: report the missing remote credential. Do not run a
  local command or initialize a local index.
- If the remote-service marker exists but `ctx-remote` is missing or fails,
  report the remote helper problem. Never fall back to local initialization.
- Never interpret local `0 events`, `missing work.sqlite`, or `uninitialized`
  as missing history when a remote marker exists.
- In remote mode, never run `ctx setup`, `ctx index`, `ctx import`, `ctx sql`,
  `ctx doctor`, `ctx locate`, or `ctx mcp` unless the user explicitly requests
  a local store or a remote import operation.

Only use the local workflow when neither remote marker exists. If the local
index is uninitialized, report that fact and initialize it only when the user
explicitly asks for local indexing.

## Remote-primary Workflow

Use `ctx-remote` below for a remote-service installation. Substitute `ctx` when
the remote environment variables are already present.

```bash
ctx-remote status
ctx-remote sources
ctx-remote search "<query>"
ctx-remote search "<query>" --provider codex
ctx-remote search "<query>" --term "<related term>"
ctx-remote search "<query>" --session <ctx-session-id>
ctx-remote show event <ctx-event-id> --window 5
ctx-remote show session <ctx-session-id>
```

When given an ID directly, try remote `show session` and `show event` before
searching for the literal ID with `ctx-remote search "<id>"`. Provider-native
IDs may be searchable even when they are not ctx session IDs. A failed remote
lookup does not authorize creating a local index.

Remote-primary search currently supports query text, `--term`, `--provider`,
`--session`, and `--event-type`. Do not use local-only filters such as
`--workspace`, `--file`, `--since`, `--include-subagents`, semantic backends,
or `--include-current-session`.

## Local Workflow

1. Confirm ctx is ready when starting from a cold context:

   ```bash
   ctx status
   ctx sources
   ```

   Use `ctx status --json` or `ctx sources --json` only when a script needs
   exact fields.

2. Search with normal language first. Add terms or filters when useful:

   ```bash
   ctx search "<query>"
   ctx search "<query>" --refresh off
   ctx search "<query>" --provider codex
   ctx search "<query>" --workspace <workspace>
   ctx search "<query>" --file <path>
   ctx search "<query>" --since 30d
   ctx search "<query>" --term "<related term>" --term "<error text>"
   ctx search "<query>" --session <ctx-session-id>
   ctx search "<query>" --verbose
   ```

   Use default text output for agent reading. Do not add `--json` for
   search, show, or locate unless you are piping it into `jq` or a script, or
   you need exact machine-readable fields. JSON output is much larger and can
   quickly consume the context window.

   When the prompt asks for a topic history or report across multiple sessions,
   run several `ctx search` queries with different wording and filters to find
   promising sessions. Use scoped
   `ctx search "<query>" --session <ctx-session-id>` when a session looks
   relevant and you need dense event-level matches from that session.

   Default search returns primary-agent sessions so human intent and decisions
   stay prominent. Use `--include-subagents` when implementation details, code
   review notes, test output, or failure traces from subagent sessions are
   likely to matter.

   Use `--verbose` when you need full ctx IDs, provider IDs, citations, and
   copyable follow-up commands without switching to JSON.

   You can write a session transcript to a temporary file, check the file size,
   and then read the relevant parts:

   ```bash
   ctx show session <ctx-session-id> --format markdown --out /tmp/ctx-session.md
   wc -c /tmp/ctx-session.md
   ```

   In Codex, ctx excludes the active session tree by default when
   `CODEX_THREAD_ID` is available, so the current prompt and subagents do not
   dominate historical retrieval. Use `--include-current-session` only when the
   active session tree is the target.

3. Inspect relevant results before relying on them:

   ```bash
   ctx show event <ctx-event-id> --window 5
   ctx show session <ctx-session-id>
   ```

4. Locate original provider material when source identity or resume hints matter:

   ```bash
   ctx locate event <ctx-event-id>
   ctx locate session <ctx-session-id>
   ```

5. Write a transcript of relevant sessions when you, the human, or another
   agent needs a file:

   ```bash
   ctx show session <ctx-session-id> --format markdown --out <output-path>
   ```

## When Search Is Not Enough

This section is local-only. In remote-primary mode, report that SQL is
unavailable instead of creating a local index.

Use `ctx sql` only when normal search cannot express the question, such as
counts, joins, audits, or scripts over stable local views. Do not use SQL for
broad transcript text search; `ctx search` is built for that.

Start with the bundled SQL docs:

```bash
ctx docs show sql
ctx docs search "stable views"
```

Common SQL examples:

```bash
ctx sql "SELECT provider, COUNT(*) AS sessions FROM ctx_sessions GROUP BY provider"
ctx sql "SELECT event_type, COUNT(*) AS events FROM ctx_events GROUP BY event_type ORDER BY events DESC"
ctx sql "SELECT path, provider, provider_session_id FROM ctx_files_touched WHERE path LIKE '%AGENTS.md%' LIMIT 20"
```

`ctx sql` is read-only and queries the existing index. It does not refresh,
import, initialize, or migrate ctx storage.

## History Research Reports

When asked to research a historical topic, stay read-only unless the user also
asks for edits. The agent writes the report; ctx only retrieves material from
the selected history backend.

1. Restate the topic, scope, and desired length if the prompt is ambiguous.
   Prefer concise reports by default; use a longer report when the user asks for
   chronology, alternatives, or detailed evidence.
2. Run several targeted searches. Vary query terms across user wording, file or
   module names, error text, commands, branch names, and decision terms. Start
   with `ctx search "<topic>"`, then broaden with `--term` or narrow with
   `--workspace`, `--provider`, `--file`, `--since`, or
   `--session <ctx-session-id>`.
   In remote-primary mode, use `ctx-remote` and only its supported filters.
   Use `--include-subagents` when reviews, implementation attempts, test output,
   or failure traces are likely to live in delegated sessions. Add
   `--refresh off` when the report must not update the local ctx index.
3. Inspect focused sources before drawing conclusions. Prefer `ctx show event`
   for a hit plus nearby turns, and `ctx show session` when the whole session
   arc matters:

   ```bash
   ctx show event <ctx-event-id> --window 5
   ctx show session <ctx-session-id>
   ```

   Use full or log mode only when default output omits necessary evidence.
4. Compare evidence across sessions. Note agreements, conflicts, stale results,
   missing raw sources, and gaps where searches did not find evidence.
5. Produce the report as agent synthesis with citations.

Concise report shape:

- answer or finding;
- strongest supporting ctx IDs;
- important caveats or gaps;
- optional next search or verification step.

Long report shape:

- question and scope;
- search method, including key queries and filters;
- findings or chronology;
- evidence table with provider, ctx session ID, ctx event ID when available, and
  why each source matters;
- conflicts, gaps, and suggested follow-up.

## Citation Rules

- Cite ctx material when it affects your answer or implementation.
- Include the provider, ctx session ID, ctx event ID when available, provider
  session ID when available, and source path or cursor when present.
- If you synthesize across multiple snippets, label the conclusion as your
  synthesis and cite the supporting snippets.
- If a source citation is stale or unavailable, say ctx returned indexed text
  but the raw source could not be opened.

## Safety Rules

- Prefer text output for agent reading. Use JSON only for scripts, `jq`, or
  exact field extraction, and keep JSON outputs small.
- Do not say ctx inferred a decision unless the cited text explicitly states
  that decision.
- Do not state that ctx wrote model analysis.
- Do not paste raw transcripts, large JSON payloads, secrets, tokens, or private
  paths into a user-facing report. Summarize reviewed evidence and quote only
  short excerpts needed to support a claim.
- Treat `~/.ctx`, provider transcript paths, and JSON output as private local
  history unless the user explicitly asks to share reviewed excerpts.
