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
      CTX_TURSO_HOOK_SYNC_BIN) CTX_TURSO_HOOK_SYNC_BIN="${value}" ;;
    esac
  done <"${CONFIG_FILE}"
fi

: "${CTX_TURSO_DATABASE_NAME:?set CTX_TURSO_DATABASE_NAME or install the launchd service}"
: "${CTX_TURSO_DATABASE_URL:?set CTX_TURSO_DATABASE_URL or install the launchd service}"

CTX_BIN="${CTX_BIN:-$(command -v ctx)}"
TURSO_BIN="${TURSO_BIN:-$(command -v turso)}"
SYNC_INTERVAL_SECONDS="${CTX_TURSO_SYNC_INTERVAL_SECONDS:-60}"
QUIET_WINDOW_MINUTES="${CTX_TURSO_QUIET_WINDOW_MINUTES:-2}"
MAX_DEFER_SECONDS="${CTX_TURSO_MAX_DEFER_SECONDS:-600}"
TOKEN_REFRESH_SECONDS="${CTX_TURSO_TOKEN_REFRESH_SECONDS:-72000}"
BATCH_SIZE="${CTX_TURSO_BATCH_SIZE:-250}"
STATE_DIR="${CTX_TURSO_STATE_DIR:-${HOME}/.local/state/ctx-remote}"
STATUS_FILE="${CTX_TURSO_STATUS_FILE:-${STATE_DIR}/sync-status.env}"
CTX_TURSO_HOOK_SYNC_BIN="${CTX_TURSO_HOOK_SYNC_BIN:-${HOME}/.local/libexec/ctx/ctx-remote-hook-sync}"

export CTX_TURSO_DATABASE_URL
if [[ -z "${SSL_CERT_FILE:-}" && -r /etc/ssl/cert.pem ]]; then
  export SSL_CERT_FILE=/etc/ssl/cert.pem
fi

token_issued_at=0
typeset -A last_sync_epoch
cycle_sources_synced=0
cycle_uploaded_events=0
cycle_scanned_events=0
cycle_last_error=""

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

source_is_ready() {
  local label="$1"
  local source_path="$2"
  local now last_sync

  source_is_quiet "${source_path}" && return 0
  now="$(date +%s)"
  last_sync="${last_sync_epoch[${label}]:-0}"
  (( last_sync == 0 || now - last_sync >= MAX_DEFER_SECONDS ))
}

json_metric() {
  local report="$1"
  local key="$2"
  local pattern="\"${key}\"[[:space:]]*:[[:space:]]*([0-9]+)"
  if [[ "${report}" =~ ${pattern} ]]; then
    print -r -- "${match[1]}"
  else
    print -r -- "0"
  fi
}

write_status() {
  local result="$1"
  local failures="$2"
  local now temp_file

  now="$(date +%s)"
  temp_file="${STATUS_FILE}.tmp.$$"
  umask 077
  mkdir -p "${STATE_DIR}" || return 1
  {
    print -r -- "last_cycle_epoch=${now}"
    print -r -- "last_result=${result}"
    print -r -- "failures=${failures}"
    print -r -- "sources_synced=${cycle_sources_synced}"
    print -r -- "uploaded_events=${cycle_uploaded_events}"
    print -r -- "scanned_events=${cycle_scanned_events}"
    [[ -z "${cycle_last_error}" ]] || print -r -- "last_error=${cycle_last_error}"
  } >"${temp_file}" || return 1
  chmod 600 "${temp_file}" || return 1
  mv -f "${temp_file}" "${STATUS_FILE}"
}

sync_source() {
  local label="$1"
  local provider="$2"
  local source_path="$3"
  local report uploaded scanned error_file error_summary command_status

  [[ -e "${source_path}" ]] || return 0
  source_is_ready "${label}" "${source_path}" || return 0
  mkdir -p "${STATE_DIR}" || return 1
  error_file="${STATE_DIR}/sync-error.tmp.$$"
  report="$("${CTX_TURSO_HOOK_SYNC_BIN}" locked-exec "${CTX_BIN}" --quiet turso import \
    --provider "${provider}" \
    --path "${source_path}" \
    --batch-size "${BATCH_SIZE}" \
    --json 2>"${error_file}")"
  command_status=$?
  if (( command_status == 75 )); then
    rm -f "${error_file}"
    return 0
  fi
  if (( command_status != 0 )); then
    error_summary="$(<"${error_file}")"
    error_summary="${error_summary//$'\n'/ }"
    error_summary="${error_summary//=/:}"
    cycle_last_error="${label}: ${error_summary[1,500]}"
    rm -f "${error_file}"
    logger -t ctx-turso-sync -- "${label} sync failed; retrying next cycle"
    return 1
  fi
  rm -f "${error_file}"
  uploaded="$(json_metric "${report}" "uploaded_events")"
  scanned="$(json_metric "${report}" "scanned_events")"
  (( cycle_sources_synced += 1 ))
  (( cycle_uploaded_events += uploaded ))
  (( cycle_scanned_events += scanned ))
  last_sync_epoch[${label}]="$(date +%s)"
  write_status "running" 0 ||
    logger -t ctx-turso-sync -- "could not update sync status"
}

while true; do
  cycle_sources_synced=0
  cycle_uploaded_events=0
  cycle_scanned_events=0
  cycle_last_error=""
  if ! refresh_token; then
    logger -t ctx-turso-sync -- "could not issue a short-lived Turso token; retrying"
    write_status "error" 1 ||
      logger -t ctx-turso-sync -- "could not update sync status"
    sleep "${SYNC_INTERVAL_SECONDS}"
    continue
  fi

  failures=0
  write_status "running" 0 ||
    logger -t ctx-turso-sync -- "could not update sync status"
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
  if (( failures == 0 )); then
    write_status "ok" 0 ||
      logger -t ctx-turso-sync -- "could not update sync status"
  else
    write_status "error" "${failures}" ||
      logger -t ctx-turso-sync -- "could not update sync status"
  fi
  if [[ "${CTX_TURSO_SYNC_ONCE:-0}" == "1" ]]; then
    exit "${failures}"
  fi
  sleep "${SYNC_INTERVAL_SECONDS}"
done
