# Agent Grid

A Windows desktop grid for running coding-agent CLI sessions side by side.

The first adapter supports the official Codex CLI. Arrange terminals in a
resizable grid, group them by working directory, search terminal output and
send input to selected sessions. Waiting and unknown sessions are excluded
from group input. Each target reports its own delivery result.

![Agent Grid design preview](docs/design-demo.png)

*Design demo with static terminal examples; this image is not runtime evidence.*

Agent Grid is an independent MIT implementation. It does not include code,
assets or branding from Claudes. That project's multi-terminal workflow was
a product reference; its source-available license does not permit unapproved
modification or redistribution. See the [reference license](https://github.com/paulallington/Claudes/blob/v1.9.82/LICENSE).

## Running

Download the portable ZIP or per-user installer from
[Releases](https://github.com/jaeseongs95/agent-grid/releases). Windows x64 and
WebView2 are required. The application is unsigned.
Extract the whole portable ZIP and run `agent-grid.exe`. Keep
`agent-grid-helper.exe` beside it: Windows requires a console process for the
terminal side while the main application uses a normal desktop window.

Install and sign in to the official Codex CLI first. Its native executable
must be on `PATH`; alternatively set `AGENT_GRID_CODEX` to its absolute path.
The CLI must support authenticated app-server WebSockets and `--remote`.
Older CLI versions are not silently downgraded to a different connection mode.

Choose **프로젝트 가져오기** to select an existing local folder. Imported
projects remain in the sidebar after their terminals stop. The session browser
opens for that project; choose an existing conversation or start a new session
with its working directory already filled in.

**세션 불러오기** opens a two-pane popup. Search session titles and filter by
project on the left; inspect dates, the conversation ID and recent messages on
the right before resuming. Already open conversations focus their existing
terminal. Browsing uses read-only requests through a short-lived owned server,
so it does not require an existing desktop daemon. Recent messages are fetched
in bounded pages and are not stored in Agent Grid's database.

Use **새 세션** to enter a directory directly. Use the official TUI to choose
a model, sign in or answer permission prompts. Select
terminal cards before using group input. A reported delivery means bytes
reached that terminal; it does not mean a model accepted or completed a turn.

## Development

Requires Node.js 24, Rust stable, MSVC build tools and WebView2 on Windows.

```powershell
npm ci
npm test
npm run build
node scripts/notices.mjs
cargo test --locked --manifest-path src-tauri/Cargo.toml
npm run tauri -- dev
```

Package the console helper before the desktop bundle:

```powershell
cargo build --locked --manifest-path src-tauri/Cargo.toml --release --bin agent-grid-helper
npm run tauri -- build --config src-tauri/tauri.bundle.conf.json --bundles nsis
```
If Unicode workspace paths cause native tool failures, build in a separate
checkout under `D:/codex/build/agent-grid`.
Generate third-party notices before native compilation; they are bundled
with the installer and distributed alongside the portable executable.

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
Only layout and session metadata are stored in the app's own SQLite database.
Terminal transcripts and Codex authentication files are not copied there.

## Validation and releases

Windows CI runs frontend checks, Rust tests and packaging. Releases include
an unsigned Windows x64 portable ZIP, NSIS installer and SHA-256 sums.
Unsigned builds may trigger Windows reputation warnings.

Actual test results and remaining limits belong in release notes; a successful
build is not evidence of long-duration stability or complete CLI compatibility.

The packaged executable provides separate diagnostic modes:

```powershell
agent-grid.exe --self-test --duration-seconds 60 --report D:/codex/build/pty-result.json
agent-grid.exe --codex-smoke --sessions 2 --cwd D:/projects/already-trusted --report D:/codex/build/codex-result.json
```

The first exercises real native PTYs with local shell fixtures, including
2/8/16-session isolation, stale-generation rejection, bounded output,
cancellation and 100 create/stop cycles followed by eight sustained PTYs.
Use `--duration-seconds 14400` for four hours of sustained PTY activity.
The second starts actual Codex servers and remote TUIs without submitting
model prompts or answering trust/approval dialogs. `--cwd` must name an existing,
already trusted directory; the probe reads its startup context and does not
write project files. Keep the report directory outside that project. Omitting
`--cwd` creates fresh directories beside the report, which may stop at Codex's
normal trust screen. The probe
requires observed thread IDs and live statuses, and records terminal hashes
rather than transcripts. Neither mode proves physical IME or GUI behavior.
