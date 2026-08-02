# ctx-remote gotchas

## A running launchd job does not prove remote sync

Validate automatic sync from the launchd execution context and then retrieve a
new, unique event from the remote database. An interactive `ctx-remote` success
does not cover launchd environment differences.

On macOS, `rustls-native-certs` can fail while reading the platform certificate
store from a background launchd process even though the same binary works in an
interactive shell. The sync service sets `SSL_CERT_FILE=/etc/ssl/cert.pem` to
use the system CA bundle without keychain access.

A quiet-window check over an entire provider directory can also starve all
completed sessions while any other session remains active. The service retains
the normal quiet window but forces an idempotent import after a bounded maximum
defer interval.

## Agent hook execution does not prove remote persistence

Session-end hooks return after placing a coalesced sync request. Verify
`ctx-remote hook-status` and then read the new event from Turso; a valid local
hook configuration can still fail when the Turso CLI login has expired.

Codex hook commands may be protected by a trusted hash. Extend the already
trusted Hindsight `retain.py` Stop hook instead of rewriting
`~/.codex/hooks.json`, which can silently disable all Codex hooks.
