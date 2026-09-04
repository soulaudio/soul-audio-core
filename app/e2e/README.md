# soul-player WebDriver UI smoke (local-only)

Tier 3 of the desktop player's test pyramid: a WebdriverIO suite driving the
real, packaged webview via `tauri-driver`. It builds the debug binary with the
`e2e` Cargo feature (which exposes the test-only `load_folder_path` command,
bypassing the native OS folder-picker dialog that WebDriver cannot drive),
launches it, loads the repo's `testdata/` fixture folder, and asserts the
library + transport UI respond.

**This suite is local-only and is NOT run in CI.** It requires native
WebView2/Edge driver tooling that isn't reliably available in CI runners
(Task 7 ships a disabled Windows GitHub Actions workflow for it — enable that
manually if/when CI tooling is sorted out). Run it by hand before a release or
whenever you want to sanity-check the shipped UI end-to-end.

## Prerequisites

1. **Rust toolchain** with the workspace already building (`cargo build` at
   the repo root should succeed).
2. **`tauri-driver`**:
   ```sh
   cargo install tauri-driver --locked
   ```
   This installs `tauri-driver(.exe)` into `~/.cargo/bin`, which
   `wdio.conf.ts` spawns directly.
3. **Windows only: `msedgedriver.exe` on PATH.** Tauri on Windows drives the
   OS WebView2 runtime via Microsoft Edge's WebDriver, and the driver version
   must match the installed WebView2 runtime. The easiest way to get a
   matching driver:
   ```sh
   cargo install --git https://github.com/chippers/msedgedriver-tool
   msedgedriver-tool
   ```
   `msedgedriver-tool` downloads a `msedgedriver.exe` matched to your
   installed WebView2 version into the current directory. Put that directory
   on `PATH` (or copy `msedgedriver.exe` into a directory already on `PATH`,
   e.g. `~/.cargo/bin`) before running the suite.
4. **Node.js** (for WebdriverIO itself) — same version the rest of `app/`
   uses.

On Linux, `tauri-driver` instead drives WebKitGTK via `WebKitWebDriver`
(install via your distro's package manager); macOS is not currently a Tauri
WebDriver target.

## Running

From `app/e2e/`:

```sh
npm install
npm test
```

`npm test` runs `wdio run ./wdio.conf.ts`, which:

1. (`onPrepare`) Runs `npm run tauri build -- --debug --no-bundle --features
   e2e` in `app/` to produce
   `app/src-tauri/target/debug/soul-player.exe` with the `load_folder_path`
   test command compiled in.
2. (`beforeSession`) Spawns `tauri-driver`, which in turn launches the app
   binary and speaks WebDriver to it (via `msedgedriver`/WebKitWebDriver).
3. Runs `test/specs/player.e2e.ts`:
   - asserts the shell renders (`<body>` exists);
   - injects the `testdata/` fixture folder via
     `window.__TAURI_INTERNALS__.invoke('load_folder_path', { dir })` (the
     page-context Tauri 2 invoke — the same call
     `@tauri-apps/api/core`'s `invoke()` makes under the hood), then waits for
     `[data-testid="track-row"]` rows to render and
     `[data-testid="now-playing-title"]` to appear;
   - clicks the last row and asserts the now-playing title changes.
4. (`afterSession`) Kills the `tauri-driver` process.

`SOUL_PLAYER_OFFLINE=1` is set for both the build and the driven process so
playback runs against the `NullSink` (device-free), matching the offline
smoke path already used elsewhere in the app.

## Troubleshooting

- **"tauri build failed"**: run `npm run tauri build -- --debug --no-bundle
  --features e2e` manually from `app/` to see the underlying Cargo error.
- **tauri-driver exits immediately / "unable to connect to msedgedriver"**:
  your `msedgedriver.exe` version doesn't match the installed WebView2
  runtime, or it isn't on `PATH`. Re-run `msedgedriver-tool` and confirm
  `where msedgedriver` resolves.
- **Session never connects / times out**: confirm nothing else is bound to
  port 4444, and that `tauri-driver` printed a listening line to stdout
  (it's piped through in `wdio.conf.ts`).
