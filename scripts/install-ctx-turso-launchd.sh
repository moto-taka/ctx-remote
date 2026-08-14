#!/bin/zsh

set -euo pipefail

usage() {
  print -u2 "usage: CTX_TURSO_AUTH_TOKEN=... $0 --database-name NAME --database-url libsql://HOST"
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
remote_source_script="${script_dir}/ctx-remote.sh"
hook_sync_source_script="${script_dir}/ctx-remote-hook-sync.py"
hindsight_source_script="${script_dir}/hindsight-session-context.py"
agent_hooks_installer="${script_dir}/install-agent-lifecycle-hooks.py"
[[ -x "${source_script}" ]] || {
  print -u2 "missing executable sync script: ${source_script}"
  exit 1
}
[[ -x "${remote_source_script}" ]] || {
  print -u2 "missing executable remote CLI script: ${remote_source_script}"
  exit 1
}
[[ -x "${hook_sync_source_script}" ]] || {
  print -u2 "missing executable hook sync script: ${hook_sync_source_script}"
  exit 1
}
[[ -x "${hindsight_source_script}" ]] || {
  print -u2 "missing executable Hindsight context script: ${hindsight_source_script}"
  exit 1
}
[[ -x "${agent_hooks_installer}" ]] || {
  print -u2 "missing agent lifecycle hook installer: ${agent_hooks_installer}"
  exit 1
}

readonly service_label="io.ctx.remote-primary-sync"
readonly install_dir="${HOME}/.local/libexec/ctx"
readonly user_bin_dir="${HOME}/.local/bin"
readonly config_dir="${HOME}/.config/ctx"
readonly launch_agents_dir="${HOME}/Library/LaunchAgents"
readonly installed_script="${install_dir}/ctx-turso-auto-sync"
readonly installed_remote_script="${user_bin_dir}/ctx-remote"
readonly installed_hook_sync_script="${install_dir}/ctx-remote-hook-sync"
readonly installed_hindsight_script="${install_dir}/hindsight-session-context"
readonly config_file="${config_dir}/turso-auto-sync.env"
readonly plist_file="${launch_agents_dir}/${service_label}.plist"
readonly launch_domain="gui/$(id -u)"

auth_token="${CTX_TURSO_AUTH_TOKEN:-}"
if [[ -z "${auth_token}" && -r "${config_file}" ]]; then
  while IFS='=' read -r key value; do
    if [[ "${key}" == "CTX_TURSO_AUTH_TOKEN" ]]; then
      auth_token="${value}"
      break
    fi
  done <"${config_file}"
fi

install -d -m 755 "${install_dir}" "${user_bin_dir}" "${config_dir}" "${launch_agents_dir}"
install -m 755 "${source_script}" "${installed_script}"
install -m 755 "${remote_source_script}" "${installed_remote_script}"
install -m 755 "${hook_sync_source_script}" "${installed_hook_sync_script}"
install -m 755 "${hindsight_source_script}" "${installed_hindsight_script}"

umask 077
{
  print -r -- "CTX_TURSO_DATABASE_NAME=${database_name}"
  print -r -- "CTX_TURSO_DATABASE_URL=${database_url}"
  [[ -z "${auth_token}" ]] || print -r -- "CTX_TURSO_AUTH_TOKEN=${auth_token}"
  print -r -- "CTX_BIN=${ctx_bin}"
  print -r -- "TURSO_BIN=${turso_bin}"
  print -r -- "CTX_TURSO_HOOK_SYNC_BIN=${installed_hook_sync_script}"
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
"${agent_hooks_installer}"

print "enabled ${service_label}"
print "remote CLI: ${installed_remote_script}"
print "sync status: ctx-remote sync-status"
if [[ -n "${auth_token}" ]]; then
  print "credentials: CTX_TURSO_AUTH_TOKEN stored in the protected config file"
else
  print "credentials: short-lived token generated from the logged-in Turso CLI"
fi
print "local ctx SQLite: disabled by remote-primary mode"
