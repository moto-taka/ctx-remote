#!/bin/zsh

set -euo pipefail

usage() {
  print -u2 "usage: $0 --database-name NAME --database-url libsql://HOST"
}

database_name=""
database_url=""
while (( $# > 0 )); do
  case "$1" in
    --database-name)
      (( $# >= 2 )) || {
        usage
        exit 2
      }
      database_name="$2"
      shift 2
      ;;
    --database-url)
      (( $# >= 2 )) || {
        usage
        exit 2
      }
      database_url="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

[[ "${database_name}" =~ '^[A-Za-z0-9_-]+$' ]] || {
  print -u2 "database name may contain only letters, digits, underscores, and hyphens"
  exit 2
}
[[ "${database_url}" =~ '^((libsql)|(https))://[A-Za-z0-9._:/-]+$' ]] || {
  print -u2 "database URL must be a libsql:// or https:// URL without credentials or query parameters"
  exit 2
}

ctx_bin="$(command -v ctx)"
turso_bin="$(command -v turso)"
script_dir="${0:A:h}"
source_script="${script_dir}/ctx-turso-auto-sync.sh"
[[ -x "${source_script}" ]] || {
  print -u2 "missing executable sync script: ${source_script}"
  exit 1
}

readonly service_label="io.ctx.remote-primary-sync"
readonly install_dir="${HOME}/.local/libexec/ctx"
readonly config_dir="${HOME}/.config/ctx"
readonly launch_agents_dir="${HOME}/Library/LaunchAgents"
readonly installed_script="${install_dir}/ctx-turso-auto-sync"
readonly config_file="${config_dir}/turso-auto-sync.env"
readonly plist_file="${launch_agents_dir}/${service_label}.plist"
readonly launch_domain="gui/$(id -u)"

install -d -m 755 "${install_dir}" "${config_dir}" "${launch_agents_dir}"
install -m 755 "${source_script}" "${installed_script}"

umask 077
{
  print -r -- "CTX_TURSO_DATABASE_NAME=${database_name}"
  print -r -- "CTX_TURSO_DATABASE_URL=${database_url}"
  print -r -- "CTX_BIN=${ctx_bin}"
  print -r -- "TURSO_BIN=${turso_bin}"
} >"${config_file}"

{
  print -r -- '<?xml version="1.0" encoding="UTF-8"?>'
  print -r -- '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
  print -r -- '<plist version="1.0">'
  print -r -- '<dict>'
  print -r -- '  <key>Label</key>'
  print -r -- "  <string>${service_label}</string>"
  print -r -- '  <key>ProgramArguments</key>'
  print -r -- '  <array>'
  print -r -- "    <string>${installed_script}</string>"
  print -r -- '  </array>'
  print -r -- '  <key>RunAtLoad</key>'
  print -r -- '  <true/>'
  print -r -- '  <key>KeepAlive</key>'
  print -r -- '  <true/>'
  print -r -- '  <key>ProcessType</key>'
  print -r -- '  <string>Background</string>'
  print -r -- '  <key>ThrottleInterval</key>'
  print -r -- '  <integer>30</integer>'
  print -r -- '  <key>StandardOutPath</key>'
  print -r -- '  <string>/dev/null</string>'
  print -r -- '  <key>StandardErrorPath</key>'
  print -r -- '  <string>/dev/null</string>'
  print -r -- '</dict>'
  print -r -- '</plist>'
} >"${plist_file}"
chmod 644 "${plist_file}"
plutil -lint "${plist_file}" >/dev/null

launchctl bootout "${launch_domain}/${service_label}" >/dev/null 2>&1 || true
launchctl bootstrap "${launch_domain}" "${plist_file}"
launchctl enable "${launch_domain}/${service_label}"

print "enabled ${service_label}"
print "credentials: short-lived token generated in memory from the logged-in Turso CLI"
print "local ctx SQLite: disabled by remote-primary mode"
