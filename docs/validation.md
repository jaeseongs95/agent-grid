# Validation evidence

## Project import and session browser

The packaged application at `dd8f0627bd905cb5e2c76b14937f129d51d97f32`
passed 16 native UI checks. These included the actual Windows folder chooser's
cancel and selection paths, canonical directory verification, duplicate import,
project filtering, stored-session title search and rendered recent messages.
After the app exited, its isolated SQLite database retained the imported project.

After completed history queries, the still-running fixture process had zero
owned helper children and zero CLOSE_WAIT sockets. This is a measurement of the
fixture's completed queries, not a claim about every process on the computer.
The external-observer connection was unavailable after the computer reboot;
the UI displayed that explicitly. The history popup still worked through its
separate owned server, and no external daemon was started.

Frontend validation includes unit and browser tests, including
coalesced queries, stale-reply rejection, exact resume identity, text-only
preview rendering and composition-key handling. Regression fixtures enforce
the native 512-byte/control-free title boundary for long ASCII, multiline
Unicode, exact-boundary Unicode and control-only catalog titles while preserving
the selected thread ID and working directory. Browser mocks do not replace
the native checks above. Physical Windows IME testing remains separate.

## Executable preferences and resume admission

Executable preferences belong only to Agent Grid's SQLite database. Tests
cover saved/environment/PATH precedence, PE validation without executing the
selected file, invalid-save preservation, reset, reopening the database and
preserving existing workspace metadata. Each request snapshots one executable
for its server/TUI pair; saving a preference does not restart existing PTYs.

Browser fixtures cover settings cancellation, errors, persistence readback and
reopening while a chooser is pending. Resume fixtures cover owned error/stopping
cards, repeated workspace restore, close/reopen during a pending start and retry
after a rejected start. All resume entry points share the same per-thread
admission guard. Native executable-selection checks run separately against an
isolated packaged app through `AGENT_GRID_NATIVE_EXECUTABLE_PICKER=1`; final
native results and source identifiers belong in the release notes.

## Earlier core baseline

The results below describe the baseline packaged application at
`a8e7daed987b7a27cea917e9df271a4748f88ed0`, before the project importer and
session-browser popup were added. Results for those additions belong to the
final release's CI run and release notes; the section above records the packaged
feature checks performed before publication.

The first release is a Windows x64 preview. Long-duration stress testing is
being handed to the user; the four-hour automated run was cancelled at their
request and is **not** a passing result.

Evidence from the packaged native application:

- Actual Codex CLI 0.165.0: eight concurrent authenticated app-server/remote
  TUI sessions, distinct observed thread UUIDs, live status and captured PTY
  output. All owned processes returned to the initial baseline after stop.
- Packaged WebView2: two actual CLI terminals, invalid-resume rejection without
  a fresh-thread fallback, error-card removal, grid resizing and reordering,
  keyboard focus, maximize/restore, terminal search, read-only external records,
  and confirmed application exit.
- Synthetic Korean composition reached VT-rendered terminal rows through the
  real PTY. This does not verify a physical Windows IME.
- Native WM_CLOSE hid the window. Cross-thread ShowWindowAsync restored the same
  HWND, thread IDs and xterm DOM. The earlier synchronous ShowWindow fixture
  failed to restore the window; the product did not require a code change.
  Clicking the physical tray icon was not tested.
- Windows CI: six frontend tests, two browser tests, thirteen Rust unit tests,
  one real-PTY integration test and packaging. The packaged 60-second native
  fixture suite created 140 PTYs, including 2/8/16-session isolation and 100
  create/stop cycles. It observed actual input backpressure and bounded cleanup.
  Its repeated heartbeat count is not a count of independent test cases.
- The connection ownership regression test uses the production connection
  path and requires an unrelated listener to receive zero HTTP bytes before
  the connection is rejected. The actual Codex tests cover the accepted path.

No model turn, trust approval or external-session control was used in these
checks. The installer was built but was not installed on the test computer.
The native stress harness reports its own parent-process memory separately
from descendant processes; these values are not total desktop-app memory.

CI runs and exact source identifiers are linked from the release notes.
Portable ZIPs include both the desktop executable and its required console
helper. SHA-256 sums identify the distributed files.
