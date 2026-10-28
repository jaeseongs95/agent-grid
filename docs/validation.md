# Initial release validation

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
