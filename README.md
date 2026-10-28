# Agent Grid

A Windows desktop grid for running coding-agent CLI sessions side by side.

Agent Grid is an independent MIT implementation. It does not include code,
assets or branding from Claudes. That project's multi-terminal workflow was
a product reference; its source-available license does not permit unapproved
modification or redistribution.

## Development

Requires Node.js 24, Rust stable, MSVC build tools and WebView2 on Windows.

```powershell
npm ci
npm test
npm run build
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri -- dev
```

Package with `npm run tauri -- build --bundles nsis`.
If Unicode workspace paths cause native tool failures, build in a separate
checkout under `D:/codex/build/agent-grid`.

## Codex

Install and sign in to the official Codex CLI separately. Agent Grid uses
your existing login and honors `CODEX_HOME`. It never installs or updates the
CLI and never changes its global permission settings. Model selection stays
in Codex's own `/model` interface.

Each managed session has an owned authenticated loopback app-server and a
normal Codex terminal. External sessions are listed read-only through the
official daemon proxy; no daemon is started to populate that list. An
unreachable server or unknown runtime status is reported explicitly.

Closing the window hides it to the tray. Complete exit terminates owned
processes after confirmation. Restoring after a complete exit starts new
processes against the saved conversation ID, rather than reviving an old PID.
Authentication tokens are not stored in the application database.

## Validation and releases

Windows CI runs frontend checks, Rust tests and packaging. Releases include
an unsigned Windows x64 portable executable, NSIS installer and SHA-256 sums.
Unsigned builds may trigger Windows reputation warnings.

Actual test results and remaining limits belong in release notes; a successful
build is not evidence of long-duration stability or complete CLI compatibility.
