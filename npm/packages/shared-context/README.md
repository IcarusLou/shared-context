# @bytedance-dev/shared-context

Thin launcher for the signed Shared Context macOS CLI. It selects the matching optional platform
package, verifies its SHA-256 digest and code signature, then forwards arguments and standard I/O.

Installation does not change Cursor or Codex configuration. Run `sctx setup` explicitly to install
the stable runtime and Agent registrations.
