#!/bin/zsh

set -u

readonly CONFIG_FILE="${CTX_TURSO_SYNC_CONFIG:-${HOME}/.config/ctx/turso-auto-sync.env}"
if [[ -r "${CONFIG_FILE}" ]]; then
  while IFS='=' read -r key value; do
    case "${key}" in
      CTX_TURSO_DATABASE_NAME) CTX_TURSO_DATABASE_NAME="${value}" ;;
      CTX_TURSO_DATABASE_URL) CTX_TURSO_DATABASE_URL="${value}" ;;
      CTX_BIN) CTX_BIN="${value}" ;;
      TURSO_BIN) TURSO_BIN="${value}" ;;
    esac
  done <"${CONFIG_FILE}"
fi

: "${CTX_TURSO_DATABASE_NAME:?set CTX_TURSO_DATABASE_NAME or install the launchd service}"
: "${CTX_TURSO_DATABASE_URL:?set CTX_TURSO_DATABASE_URL or install the launchd service}"

CTX_BIN="${CTX_BIN:-$(command -v ctx)}"
TURSO_BIN="${TURSO_BIN:-$(command -v turso)}"
SYNC_INTERVAL_SECONDS="${CTX_TURSO_SYNC_INTERVAL_SECONDS:-60}"
QUIET_WINDOW_MINUTES="${CTX_TURSO_QUIET_WINDOW_MINUTES:-2}"
TOKEN_REFRESH_SECONDS="${CTX_TURSO_TOKEN_REFRESH_SECONDS:-72000}"
BATCH_SIZE="${CTX_TURSO_BATCH_SIZE:-100}"

export CTX_TURSO_DATABASE_URL

token_issued_at=0

refresh_token() {
  local now token
  now="$(date +%s)"
  if (( now - token_issued_at < TOKEN_REFRESH_SECONDS )) && [[ -n "${CTX_TURSO_AUTH_TOKEN:-}" ]]; then
    return 0
  fi

  token="$("${TURSO_BIN}" db tokens create "${CTX_TURSO_DATABASE_NAME}" --expiration 1d 2>/dev/null)" ||
    return 1
  [[ -n "${token}" ]] || return 1
  export CTX_TURSO_AUTH_TOKEN="${token}"
  token_issued_at="${now}"
}

source_is_quiet() {
  local source_path="$1"
  local recent

  if [[ -d "${source_path}" ]]; then
    recent="$(find "${source_path}" -type f -mmin "-${QUIET_WINDOW_MINUTES}" -print -quit 2>/dev/null)"
    [[ -z "${recent}" ]]
    return
  fi

  local now modified_at
  now="$(date +%s)"
  modified_at="$(stat -f '%m' "${source_path}" 2>/dev/null)" || return 1
  (( now - modified_at >= QUIET_WINDOW_MINUTES * 60 ))
}

sync_source() {
  local label="$1"
  local provider="$2"
  local source_path="$3"

  [[ -e "${source_path}" ]] || return 0
  source_is_quiet "${source_path}" || return 0
  if ! "${CTX_BIN}" --quiet turso import \
    --provider "${provider}" \
    --path "${source_path}" \
    --batch-size "${BATCH_SIZE}" \
    --json >/dev/null 2>&1; then
    logger -t ctx-turso-sync -- "${label} sync failed; retrying next cycle"
    return 1
  fi
}

while true; do
  if ! refresh_token; then
    logger -t ctx-turso-sync -- "could not issue a short-lived Turso token; retrying"
    sleep "${SYNC_INTERVAL_SECONDS}"
    continue
  fi

  failures=0
  sync_source "Codex" "codex" "${HOME}/.codex/sessions" || (( failures += 1 ))
  sync_source "Claude" "claude" "${HOME}/.claude/projects" || (( failures += 1 ))
  sync_source "Claude alternate" "claude" "${HOME}/.claude-sapeet/projects" ||
    (( failures += 1 ))
  sync_source "OpenCode" "opencode" "${HOME}/.local/share/opencode/opencode.db" ||
    (( failures += 1 ))
  sync_source "Qwen Code" "qwen-code" "${HOME}/.qwen/projects" || (( failures += 1 ))

  if (( failures > 0 )); then
    logger -t ctx-turso-sync -- "${failures} provider sync(s) failed"
  fi
  if [[ "${CTX_TURSO_SYNC_ONCE:-0}" == "1" ]]; then
    exit "${failures}"
  fi
  sleep "${SYNC_INTERVAL_SECONDS}"
done
