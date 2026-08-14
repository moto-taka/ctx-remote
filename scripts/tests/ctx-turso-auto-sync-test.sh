#!/bin/zsh

set -euo pipefail

readonly ROOT="$(cd "${0:A:h}/../.." && pwd)"
readonly TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "${TEST_ROOT}"' EXIT

mkdir -p "${TEST_ROOT}/home/.codex/sessions" "${TEST_ROOT}/state" "${TEST_ROOT}/bin"
print -r -- "session" >"${TEST_ROOT}/home/.codex/sessions/session.jsonl"

print -r -- '#!/bin/zsh
if [[ "${1:-}" == "auth" && "${2:-}" == "whoami" ]]; then
  if [[ "${CTX_TEST_AUTH_STATUS:-}" == "not-logged-in" ]]; then
    print -r -- "You are not logged in, please login with turso auth login before running other commands."
  fi
  exit 0
fi
count=0
if [[ -r "${CTX_TEST_ROOT}/token-count" ]]; then
  count="$(<"${CTX_TEST_ROOT}/token-count")"
fi
(( count += 1 ))
print -r -- "${count}" >"${CTX_TEST_ROOT}/token-count"
if (( count == 1 )); then
  print -r -- "expired-token"
else
  print -r -- "fresh-token"
fi' >"${TEST_ROOT}/bin/turso"

print -r -- '#!/bin/zsh
[[ "${1:-}" == "locked-exec" ]]
shift
exec "$@"' >"${TEST_ROOT}/bin/hook-sync"

print -r -- '#!/bin/zsh
if [[ "${CTX_TURSO_AUTH_TOKEN:-}" == "expired-token" ]]; then
  print -u2 -- "Error: JWT error: InvalidToken"
  exit 1
fi
[[ "${CTX_TURSO_AUTH_TOKEN:-}" == "fresh-token" || "${CTX_TURSO_AUTH_TOKEN:-}" == "stable-token" ]]
print -r -- "{\"uploaded_events\":2,\"scanned_events\":3}"
' >"${TEST_ROOT}/bin/ctx"

chmod 755 "${TEST_ROOT}/bin/turso" "${TEST_ROOT}/bin/hook-sync" "${TEST_ROOT}/bin/ctx"

export HOME="${TEST_ROOT}/home"
export CTX_TEST_ROOT="${TEST_ROOT}"
export CTX_TURSO_DATABASE_NAME="ctx-test"
export CTX_TURSO_DATABASE_URL="libsql://ctx-test.turso.io"
export CTX_BIN="${TEST_ROOT}/bin/ctx"
export TURSO_BIN="${TEST_ROOT}/bin/turso"
export CTX_TURSO_HOOK_SYNC_BIN="${TEST_ROOT}/bin/hook-sync"
export CTX_TURSO_STATE_DIR="${TEST_ROOT}/state"
export CTX_TURSO_STATUS_FILE="${TEST_ROOT}/state/sync-status.env"
export CTX_TURSO_MAX_DEFER_SECONDS=0
export CTX_TURSO_SYNC_ONCE=1

zsh "${ROOT}/scripts/ctx-turso-auto-sync.sh"

[[ "$(<"${TEST_ROOT}/token-count")" == "2" ]]
grep -Fqx "last_result=ok" "${TEST_ROOT}/state/sync-status.env"
grep -Fqx "uploaded_events=2" "${TEST_ROOT}/state/sync-status.env"

export CTX_TEST_AUTH_STATUS="not-logged-in"
export CTX_TURSO_STATE_DIR="${TEST_ROOT}/auth-error-state"
export CTX_TURSO_STATUS_FILE="${TEST_ROOT}/auth-error-state/sync-status.env"
if zsh "${ROOT}/scripts/ctx-turso-auto-sync.sh"; then
  print -u2 -- "expected unauthenticated sync to fail"
  exit 1
fi
grep -Fqx "last_result=error" "${TEST_ROOT}/auth-error-state/sync-status.env"
grep -Fq "Turso CLI authentication failed:" "${TEST_ROOT}/auth-error-state/sync-status.env"

export CTX_TURSO_AUTH_TOKEN="stable-token"
export CTX_TURSO_STATE_DIR="${TEST_ROOT}/static-token-state"
export CTX_TURSO_STATUS_FILE="${TEST_ROOT}/static-token-state/sync-status.env"
zsh "${ROOT}/scripts/ctx-turso-auto-sync.sh"
[[ "$(<"${TEST_ROOT}/token-count")" == "2" ]]
grep -Fqx "last_result=ok" "${TEST_ROOT}/static-token-state/sync-status.env"
grep -Fqx "uploaded_events=2" "${TEST_ROOT}/static-token-state/sync-status.env"

print -r -- "CTX_TURSO_DATABASE_URL=libsql://config-token.turso.io
CTX_TURSO_AUTH_TOKEN=stable-token" >"${TEST_ROOT}/remote-config.env"
unset CTX_TURSO_DATABASE_URL CTX_TURSO_AUTH_TOKEN
export CTX_TURSO_SYNC_CONFIG="${TEST_ROOT}/remote-config.env"
remote_status="$(zsh "${ROOT}/scripts/ctx-remote.sh" status --json)"
[[ "${remote_status}" == '{"uploaded_events":2,"scanned_events":3}' ]]
print -r -- "ctx-turso auto-sync token refresh test passed"
