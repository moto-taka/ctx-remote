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
      CTX_BIN)
        CTX_BIN="${CTX_BIN:-${value}}"
        ;;
      TURSO_BIN)
        TURSO_BIN="${TURSO_BIN:-${value}}"
        ;;
    esac
  done <"${CONFIG_FILE}"
fi

: "${CTX_TURSO_DATABASE_URL:?remote-primary is not configured; install the ctx Turso service first}"

CTX_BIN="${CTX_BIN:-$(command -v ctx)}"
export CTX_TURSO_DATABASE_URL

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
