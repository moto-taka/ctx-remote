#!/bin/zsh

set -euo pipefail

readonly CONFIG_FILE="${CTX_TURSO_SYNC_CONFIG:-${HOME}/.config/ctx/turso-auto-sync.env}"

if [[ -r "${CONFIG_FILE}" ]]; then
  while IFS='=' read -r key value; do
    case "${key}" in
      CTX_TURSO_DATABASE_NAME)
        CTX_TURSO_DATABASE_NAME="${CTX_TURSO_DATABASE_NAME:-${value}}"
        ;;
      CTX_TURSO_DATABASE_URL)
        CTX_TURSO_DATABASE_URL="${CTX_TURSO_DATABASE_URL:-${value}}"
        ;;
      CTX_TURSO_AUTH_TOKEN)
        CTX_TURSO_AUTH_TOKEN="${CTX_TURSO_AUTH_TOKEN:-${value}}"
        ;;
      CTX_BIN)
        CTX_BIN="${CTX_BIN:-${value}}"
        ;;
      TURSO_BIN)
        TURSO_BIN="${TURSO_BIN:-${value}}"
        ;;
      CTX_TURSO_HOOK_SYNC_BIN)
        CTX_TURSO_HOOK_SYNC_BIN="${CTX_TURSO_HOOK_SYNC_BIN:-${value}}"
        ;;
    esac
  done <"${CONFIG_FILE}"
fi

: "${CTX_TURSO_DATABASE_URL:?remote-primary is not configured; install the ctx Turso service first}"

CTX_BIN="${CTX_BIN:-$(command -v ctx)}"
export CTX_TURSO_DATABASE_URL
if [[ -n "${CTX_TURSO_AUTH_TOKEN:-}" ]]; then
  export CTX_TURSO_AUTH_TOKEN
fi

if [[ "${1:-}" == "hook-sync" ]]; then
  [[ $# == 2 ]] || {
    print -u2 "usage: ctx-remote hook-sync claude|codex|qwen-code"
    exit 2
  }
  CTX_TURSO_HOOK_SYNC_BIN="${CTX_TURSO_HOOK_SYNC_BIN:-${HOME}/.local/libexec/ctx/ctx-remote-hook-sync}"
  exec "${CTX_TURSO_HOOK_SYNC_BIN}" request "$2"
fi

if [[ "${1:-}" == "hook-status" ]]; then
  readonly HOOK_STATUS_FILE="${CTX_TURSO_STATE_DIR:-${HOME}/.local/state/ctx-remote}/hook-sync-status.json"
  [[ -r "${HOOK_STATUS_FILE}" ]] || {
    print -r -- '{"result":"never"}'
    exit 1
  }
  exec /bin/cat "${HOOK_STATUS_FILE}"
fi

if [[ "${1:-}" == "sync-status" ]]; then
  readonly STATUS_FILE="${CTX_TURSO_STATUS_FILE:-${HOME}/.local/state/ctx-remote/sync-status.env}"
  service_state="stopped"
  if launchctl print "gui/$(id -u)/io.ctx.remote-primary-sync" >/dev/null 2>&1; then
    service_state="running"
  fi
  print -r -- "service=${service_state}"
  if [[ ! -r "${STATUS_FILE}" ]]; then
    print -r -- "last_result=never"
    exit 1
  fi
  while IFS='=' read -r key value; do
    case "${key}" in
      last_cycle_epoch | last_result | failures | sources_synced | uploaded_events | scanned_events | last_error)
        print -r -- "${key}=${value}"
        ;;
    esac
  done <"${STATUS_FILE}"
  exit 0
fi

if [[ "${1:-}" == "turso" && "${2:-}" == "push" && ! -f "${HOME}/.ctx/work.sqlite" ]]; then
  print -u2 "ctx-remote turso push migrates an existing local ctx work.sqlite."
  print -u2 "No local ctx index exists, which is expected in remote-primary mode."
  print -u2 "Automatic sync uses 'ctx turso import'; check it with 'ctx-remote sync-status'."
  exit 2
fi

if [[ -z "${CTX_TURSO_AUTH_TOKEN:-}" ]]; then
  : "${CTX_TURSO_DATABASE_NAME:?database name is required to issue a short-lived token}"
  TURSO_BIN="${TURSO_BIN:-$(command -v turso)}"
  CTX_TURSO_AUTH_TOKEN="$(
    "${TURSO_BIN}" db tokens create "${CTX_TURSO_DATABASE_NAME}" --expiration 1d 2>/dev/null
  )"
  [[ -n "${CTX_TURSO_AUTH_TOKEN}" ]] || {
    print -u2 "could not issue a short-lived Turso token"
    exit 1
  }
  export CTX_TURSO_AUTH_TOKEN
fi

exec "${CTX_BIN}" "$@"
