# Codex.app Reload Research

> Research dated 2026-05-13. Verify before relying — Codex.app behavior changes between releases.

## Sources inspected

**Bundle inspection:**
- `/Applications/Codex.app/Contents/Info.plist` — plist key survey
- `/Applications/Codex.app/Contents/Resources/app.asar` — extracted via `@electron/asar`
- `/tmp/codex-asar/.vite/build/main-DnQgBHvi.js` — Electron main-process bundle (~1.1 MB)
- `/tmp/codex-asar/.vite/build/app-session-tZw_L1R0.js` — session bootstrap bundle (~4.3 MB)
- `/tmp/codex-asar/webview/assets/app-main-Bucm979x.js` — renderer bundle (~723 KB)
- `/tmp/codex-asar/webview/assets/app-server-manager-signals-C1h8B-R-.js` — app-server IPC layer
- `/tmp/codex-asar/webview/assets/sidebar-signals-B477TzmP.js`, `sidebar-project-groups-Be4EWk1a.js`
- `/Applications/Codex.app/Contents/Resources/codex` — bundled Rust CLI binary (arm64)
- `~/.codex/state_5.sqlite` — local session database

**GitHub source (Codex CLI, open source):**
- `https://github.com/openai/codex/tree/main/codex-rs/app-server/src`
- `https://raw.githubusercontent.com/openai/codex/main/codex-rs/rollout/src/state_db.rs`
- `https://raw.githubusercontent.com/openai/codex/main/codex-rs/state/src/runtime.rs`
- `https://raw.githubusercontent.com/openai/codex/main/codex-rs/app-server/src/fs_watch.rs`
- `https://raw.githubusercontent.com/openai/codex/main/codex-rs/app-server/src/in_process.rs`

**Commands run:**
```bash
plutil -p /Applications/Codex.app/Contents/Info.plist
npx @electron/asar list /Applications/Codex.app/Contents/Resources/app.asar
npx @electron/asar extract /Applications/Codex.app/Contents/Resources/app.asar /tmp/codex-asar
strings /Applications/Codex.app/Contents/Resources/codex
sqlite3 ~/.codex/state_5.sqlite "PRAGMA journal_mode; PRAGMA wal_autocheckpoint;"
find /Applications/Codex.app -name "*.sdef"
find /Applications/Codex.app -name "*.xpc"
```

---

## Codex.app version analysed

`CFBundleShortVersionString`: **26.506.31421** (`CFBundleVersion`: 2620)  
Built with Electron (NSPrincipalClass = `AtomApplication`), DTXcodeBuild 16F6, SDK macosx15.5.

---

## 1. URL scheme handler

**Registered scheme:** `codex://` — confirmed in `CFBundleURLTypes`:

```xml
"CFBundleURLTypes" => [
  { "CFBundleURLName" => "Codex", "CFBundleURLSchemes" => ["codex"] }
]
```

**Known URL patterns found in bundle inspection:**

| URL | Where found | Purpose |
|-----|-------------|---------|
| `codex://connector/oauth_callback` | `main-DnQgBHvi.js` | OAuth connector callback |
| `codex://threads/new` | `codex` Rust binary string literal | Open a new-thread page in the desktop app |

**Deep-link routing:** The Electron app registers the `codex` scheme via `setAsDefaultProtocolClient`. On macOS, incoming `open -a Codex codex://...` calls arrive via the `second-instance` event. The handler calls `deepLinks.queueProcessArgs(argv)`, which eventually calls `navigateToRoute`. Supported route kinds found in the bundle (`applyCodexAppConfig`, `pluginInstall`); **no `reload`, `refresh`, or `reload-sessions` route exists**.

`open codex://threads/new` is the most useful externally-triggerable URL; it tells Codex.app to display a new-thread page. It does **not** trigger a session-list re-read.

**Verdict: no reload/refresh URL scheme found.**

---

## 2. AppleScript dictionary

No `.sdef` file found anywhere in the bundle:

```bash
find /Applications/Codex.app -name "*.sdef"  # → no output
```

`Info.plist` does contain `NSAppleEventsUsageDescription` ("Codex uses Apple Events to control Mac apps on your behalf"), but this describes Codex *sending* Apple Events to other apps (for computer-use automation), not Codex *receiving* them.

No `OSAScriptingDefinition` key in Info.plist. No AppleScript dictionary is registered.

**Verdict: AppleScript is not supported; `osascript -e 'tell app "Codex" to ...'` only produces the default `run` / `quit` events, none session-related.**

---

## 3. NSDistributedNotificationCenter

Exhaustive search across all extracted JS bundles and the Rust binary:

```bash
grep -rP "NSDistributed|CFNotification|DistributedNotification" \
  /tmp/codex-asar/.vite/build/ /tmp/codex-asar/webview/assets/
# → no output
```

The `objc-js` native addon is present (used for `NSRunningApplication`, `NSBundle`, AppKit classes for the tray icon / launch-services integration), but no `NSDistributedNotificationCenter` usage was found.

**Verdict: Codex.app does not listen for any distributed notifications. This channel is closed.**

---

## 4. Apple Events / generic osascript

No scripting dictionary (see §2). The generic Apple Events `run` and `quit` work (Electron responds to them). No custom event handlers are registered. Sending arbitrary Apple Events to `com.openai.codex` would have no effect beyond what the default suite provides.

**Verdict: no usable Apple Events surface.**

---

## 5. XPC / Mach services

**Info.plist XPC keys:** none. `grep -i -E "xpc|machservice|extension"` on the plist returns only `CFBundleTypeExtensions` (document file extensions).

**Sub-bundles found:**

| Bundle | Purpose |
|--------|---------|
| `Codex Helper.app`, `Codex Helper (GPU).app`, etc. | Standard Electron helper processes |
| `Codex Computer Use.app` | Plugin for computer-use automation |
| `Sparkle.framework/XPCServices/Downloader.xpc` | Sparkle auto-updater downloader |
| `Sparkle.framework/XPCServices/Installer.xpc` | Sparkle auto-updater installer |

None of these XPC services expose a "reload sessions" interface. The Sparkle services are auto-updater infrastructure only.

The app-server IPC is handled entirely through an **in-process Tokio message-passing channel** (not an XPC service), as confirmed in `codex-rs/app-server/src/in_process.rs`.

**Verdict: no usable XPC surface for session reload.**

---

## 6. SQLite WAL trick

**WAL mode confirmed:**

```bash
sqlite3 ~/.codex/state_5.sqlite "PRAGMA journal_mode; PRAGMA wal_autocheckpoint;"
# → wal
# → 1000
```

WAL files present: `state_5.sqlite-shm`, `state_5.sqlite-wal` (WAL is 0 bytes when app not running = fully checkpointed).

**Does Codex.app watch the WAL?** No. Inspection of `codex-rs/state/src/runtime.rs` shows the database is opened once at startup via `sqlx::SqlitePool` with a max of 5 connections, all persistent. No SQLite update hook, WAL hook, or file-system watcher on the database file was found in either the Rust binary or the JS bundles:

```bash
grep -oP ".{50}wal.hook|wal_hook|update.hook|checkpoint.callback.{50}" \
  /tmp/codex-asar/.vite/build/main-DnQgBHvi.js
# → no output
strings /Applications/Codex.app/Contents/Resources/codex | grep -E "wal.hook|update.hook"
# → "Failed to update hook config:"  (one telemetry string, not an install path)
```

Forcing a WAL checkpoint from outside (`PRAGMA wal_checkpoint(TRUNCATE)`) would not signal Codex.app because it holds no file descriptor watcher or WAL hook on `state_5.sqlite`.

**Verdict: WAL checkpoint trick has no effect on Codex.app's in-memory session list.**

---

## 7. Source code patterns (Codex GitHub)

### Session-list loading flow

The Electron renderer calls `thread/list` on the **in-process app-server** (a Rust Tokio task embedded inside the Electron process via `node-pty`/IPC channels). The `thread/list` RPC queries `codex_rollout::state_db`, which wraps a persistent `Arc<sqlx::SqlitePool>`.

Key code in `app-server-manager-signals-C1h8B-R-.js`:

```js
// Three call sites for thread/list:
await e.sendRequest("thread/list", {limit: 200, cursor: a, sortKey: ..., sourceKinds: pe, archived: n});
this.params.requestClient.sendRequest("thread/list", {limit: t, cursor: e, ...});
await this.sendRequest("thread/list", {archived: false, cursor: null, limit: null, ...});
```

### What triggers a re-read

`refreshRecentConversations()` is the function that calls `thread/list` and repopulates the sidebar. It is invoked in exactly **two** situations:

1. **WebSocket reconnect recovery** — triggered when the app-server process disconnects and reconnects:
   ```js
   // app-main-Bucm979x.js
   J.info("websocket_reconnect_recovery_start", ...);
   await Kn("refresh-recent-conversations-for-host", {hostId: r});
   ```

2. **User-initiated archive/unarchive** — internal UI action.

There is no polling timer. There is no file-system watcher on `~/.codex/sessions/` or `state_5.sqlite`. The sidebar does not self-refresh.

### `thread/started` notification (partial path)

When a new thread is created or resumed via the app-server RPC, the server emits a `thread/started` JSON-RPC notification. The renderer handles it:

```js
case "thread/started": {
  let {thread: e} = n.params;
  let t = this.upsertConversationFromThread(e);
  // → ensureRecentConversationId(t) → adds to sidebar
}
```

This could theoretically surface a synced session in the sidebar **if** an external process called `thread/resume` (or `thread/start`) via the control socket — but that would actually start a new interactive session, not just announce an existing one. There is no `thread/imported` or `thread/discovered` notification type.

### App-server control socket

When Codex.app is running, the local app-server socket lives at:

```
"${CODEX_HOME:-$HOME/.codex}/app-server-control/app-server-control.sock"
```

This was confirmed in both the Electron main bundle:

```js
// main-DnQgBHvi.js
yf = '"${CODEX_HOME:-$HOME/.codex}/app-server-control"'
bf = '"${CODEX_HOME:-$HOME/.codex}/app-server-control/app-server.log"'
```

and the Rust binary:

```
app-server-control.sock   (string literal in codex binary)
```

The Rust binary also ships a `StdioToUdsCommand` subcommand ("Internal: relay stdio to a Unix domain socket") that can pipe JSON-RPC over stdio to this socket. However, this socket is the **in-process** app-server; messages sent to it are processed by the Rust layer but do **not** cause the Electron UI to call `refreshRecentConversations`. The UI only re-reads the thread list on websocket reconnect.

### File-watching (fs/watch)

The app-server exposes an `fs/watch` RPC (debounces at 200 ms, sends `fs/changed` notifications to the subscribing connection). However, `fs/watch` is a capability offered *to* connected clients; the app-server itself does not watch `state_5.sqlite` or `~/.codex/sessions/` for external changes.

### App-server restart mechanism

The Electron renderer can send `codex-app-server-restart` (internal IPC) to the main process:

```js
case "codex-app-server-restart":
  await this.getAppServerConnection(i.hostId).restart({killCodexProcess: i.killCodexProcess ?? false});
```

When `killCodexProcess` is `false`, it gracefully restarts the in-process Rust task. This causes a websocket disconnect + reconnect, which triggers `refreshRecentConversations`. The command the app itself uses to kill the codex process on remote SSH hosts is:

```bash
pkill -9 -u "$(id -u)" -f 'codex.*app-server'
```

For the local (in-process) case, there is no separate OS process to kill — it is an internal task restart.

---

## Verdict

### C. No usable mechanism

None of the investigated channels (URL scheme, AppleScript, distributed notifications, Apple Events, XPC, WAL) provides a way for an external process to make Codex.app re-read its SQLite session list **without restarting the app process**.

The only mechanism that triggers a sidebar refresh (`refreshRecentConversations` → `thread/list`) is a **websocket reconnect event**, which happens only when the in-process app-server task restarts. That task is embedded inside the Electron process itself; there is no stable external signal to restart it without either restarting the whole Electron process or injecting into it.

Sending JSON-RPC to the control socket (`~/.codex/app-server-control/app-server-control.sock`) processes requests on the server side but does not notify the Electron renderer to re-read the thread list.

---

## Recommended implementation for sessync v0.7.0

**Implement C:** after `sessync resume --tool codex` writes sessions to SQLite, do:

```bash
killall Codex && open /Applications/Codex.app
```

This is disruptive but is the only verified mechanism. Recommended UX mitigations:

1. Print a clear one-line notice to the user before triggering the restart:
   ```
   Note: Codex.app must restart to show the synced session (no graceful reload available).
   Restarting Codex.app…
   ```
2. Gate the restart behind an explicit flag (`--restart-app`) so users can opt out and accept that the session appears only in Codex CLI until they manually reopen the app.
3. After `open /Applications/Codex.app`, wait ~2 s then call `open codex://threads/new` to bring the app to the foreground on the new-thread page.

**Half-viable alternative (not recommended for v0.7.0):** Kill only the `codex` Rust subprocess (if it ever runs as a visible OS process in a future version) with `pkill -f 'codex.*app-server'`. The Electron shell would then auto-restart it and run `refreshRecentConversations`. However, in the current version (26.506.31421), the app-server is in-process and there is no such killable subprocess for local usage. This approach would only work for remote SSH host connections; not worth the complexity.

---

## Risks / things that could break in future Codex releases

1. **In-process → subprocess migration**: If OpenAI moves the local app-server to a separate OS process (like the remote SSH case), the `pkill -f 'codex.*app-server'` trick would work and `killall Codex` would no longer be necessary. Monitor release notes.

2. **URL scheme expansion**: The `codex://` handler currently only routes to `oauth_callback` and (in the CLI binary) `threads/new`. A future `codex://reload-sessions` route could be added; watch the GitHub repo for changes to the `navigateToRoute` implementation and `queueCodexDeepLinkUrl`.

3. **File-watching addition**: If OpenAI adds `chokidar` or native FSEvents watching on `state_5.sqlite` or `~/.codex/sessions/`, that would enable a WAL-checkpoint-based trigger. The Rust `fs_watch.rs` already has a 200 ms debounce pattern ready for new watch targets.

4. **`NSDistributedNotificationCenter` registration**: As the app grows features, it may eventually register distributed notification names (e.g., for menu bar badge updates). This would enable a `notifyd` / `osascript` based refresh. None exists today.

5. **Electron IPC exposure**: Electron's `contextBridge`/`ipcRenderer` API, if ever exposed to an external `webContents`, could allow calling `refresh-recent-conversations-for-host`. Unlikely given security model, but possible via a companion browser extension or MCP tool.

6. **SQLite connection pool**: The `sqlx::SqlitePool` (max 5 connections, WAL mode) keeps file handles open. External writes by sessync that do not go through sqlx's WAL writer are still valid SQLite writes and will be visible to subsequent queries — but only after the Electron app re-reads, which only happens on reconnect/restart.
