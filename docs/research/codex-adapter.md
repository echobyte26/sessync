# Codex Adapter — Research

> Research dated 2026-05-07. Confirm before relying on these answers — Codex CLI moves fast (multiple
> releases per week as of May 2026).

---

## Sources

### Files inspected locally

- `/Applications/Codex.app/Contents/Info.plist` — app version
- `/Applications/Codex.app/Contents/Resources/codex` — CLI binary (strings-dumped)
- `/Applications/Codex.app/Contents/Resources/runtime.json` — bundle version
- `~/.codex/config.toml` — user config
- `~/.codex/.codex-global-state.json` — electron persisted state
- `~/.codex/state_5.sqlite` — session state DB (schema + counts)
- `~/.codex/logs_2.sqlite` — telemetry DB (schema)
- `~/.codex/sqlite/codex-dev.db` — automation/inbox DB (schema)

### GitHub source files fetched

- `https://github.com/openai/codex/blob/main/codex-rs/hooks/src/types.rs`
- `https://github.com/openai/codex/blob/main/codex-rs/hooks/src/events/stop.rs`
- `https://github.com/openai/codex/blob/main/codex-rs/hooks/src/engine/mod.rs`
- `https://github.com/openai/codex/blob/main/codex-rs/hooks/src/engine/dispatcher.rs`
- `https://github.com/openai/codex/blob/main/codex-rs/hooks/src/schema.rs`
- `https://github.com/openai/codex/blob/main/codex-rs/core/src/hook_runtime.rs`
- `https://github.com/openai/codex/tree/main/codex-rs/thread-store/src/`
- `https://github.com/openai/codex/blob/main/codex-rs/thread-store/src/types.rs`
- `https://github.com/openai/codex/blob/main/codex-rs/thread-store/src/local/helpers.rs`
- `https://github.com/openai/codex/blob/main/codex-rs/thread-store/src/local/create_thread.rs`
- `https://github.com/openai/codex/blob/main/codex-rs/thread-store/src/local/mod.rs`
- `https://github.com/openai/codex/blob/main/codex-rs/thread-store/src/local/list_threads.rs`
- `https://github.com/openai/codex/blob/main/codex-rs/thread-store/src/local/read_thread.rs`
- `https://github.com/openai/codex/tree/main/codex-rs/rollout/src/`
- `https://github.com/openai/codex/blob/main/codex-rs/rollout/src/recorder.rs`
- `https://github.com/openai/codex/blob/main/codex-rs/rollout/src/metadata.rs`
- `https://github.com/openai/codex/blob/main/codex-rs/core/src/session/rollout_reconstruction.rs`

### Official docs fetched

- `https://developers.openai.com/codex/hooks`
- `https://developers.openai.com/codex/config-advanced`
- `https://developers.openai.com/codex/config-basic`
- `https://developers.openai.com/codex/config-reference`
- `https://developers.openai.com/codex/cli/features`
- `https://developers.openai.com/codex/changelog`

### GitHub issues inspected

- `https://github.com/openai/codex/issues/20864` — desktop sessions/ scanning cost
- `https://github.com/openai/codex/issues/16270` — sqlite growth

---

## Codex installed locally?

**Yes — desktop app, not the terminal CLI.**

| Item | Value |
|---|---|
| App | `/Applications/Codex.app` |
| Version | `26.506.31421` (bundle `26.506.11943`) |
| CLI binary | `/Applications/Codex.app/Contents/Resources/codex` (Mach-O arm64) |
| `codex` in PATH | **No** — `which codex` returns nothing |
| Data home | `~/.codex/` |

The binary is the same Rust CLI that powers the desktop TUI; it is also distributed standalone via
`npm install -g @openai/codex` for pure-terminal use. Both the desktop and standalone CLI share the
same `~/.codex/` data directory and the same storage schema described below.

---

## 1. Session storage

### Primary storage: dual-layer (SQLite index + JSONL rollout files)

Codex uses **two complementary stores** for sessions ("threads" in its vocabulary):

#### Layer A — `~/.codex/state_5.sqlite`

The authoritative metadata index. Schema (confirmed by direct inspection):

```sql
CREATE TABLE threads (
    id TEXT PRIMARY KEY,           -- UUID, the session/thread ID
    rollout_path TEXT NOT NULL,    -- path to the rollout JSONL file
    created_at INTEGER NOT NULL,   -- unix seconds
    updated_at INTEGER NOT NULL,   -- unix seconds
    source TEXT NOT NULL,          -- "local", "vscode", "exec", "mcp", "subagent", ...
    model_provider TEXT NOT NULL,
    cwd TEXT NOT NULL,             -- working directory at session start
    title TEXT NOT NULL,
    sandbox_policy TEXT NOT NULL,  -- e.g. "default"
    approval_mode TEXT NOT NULL,   -- e.g. "suggest"
    tokens_used INTEGER NOT NULL DEFAULT 0,
    has_user_event INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    archived_at INTEGER,
    git_sha TEXT,
    git_branch TEXT,
    git_origin_url TEXT,
    cli_version TEXT NOT NULL DEFAULT '',
    first_user_message TEXT NOT NULL DEFAULT '',
    agent_nickname TEXT,
    agent_role TEXT,
    memory_mode TEXT NOT NULL DEFAULT 'enabled',
    model TEXT,                    -- e.g. "o3", "gpt-4o"
    reasoning_effort TEXT,
    agent_path TEXT,
    created_at_ms INTEGER,         -- unix ms (derived by trigger)
    updated_at_ms INTEGER,         -- unix ms (derived by trigger)
    thread_source TEXT
);
```

> **Implementation note (2026-05-07):** The original draft of this doc listed `sandbox_policy`,
> `approval_mode`, and `has_user_event` only in a comment. Direct `.schema` inspection confirmed
> these are actual `NOT NULL` columns, and that column order differs from what was documented above.
> The `write_session` implementation supplies defaults (`'default'`, `'suggest'`, `0`) for all
> NOT NULL columns that sessync doesn't own. The table name is `threads`, not `sessions` — the
> research doc was correct on this, but early drafts of the task spec used "sessions table" loosely.

Additional tables in `state_5.sqlite`: `thread_dynamic_tools`, `stage1_outputs` (memory/compaction),
`agent_jobs`, `agent_job_items`, `thread_spawn_edges`, `remote_control_enrollments`, `thread_goals`.

The `threads` table on this machine has **0 rows** — the desktop app was installed but never used to
start a session. The schema was confirmed from `.schema` output.

#### Layer B — `~/.codex/sessions/<date-path>/rollout-<timestamp>-<uuid>.jsonl`

Each session has a corresponding JSONL rollout file. Path construction (from source analysis):

```
~/.codex/sessions/YYYY/MM/DD/rollout-YYYY-MM-DDTHH-MM-SS-<thread-uuid>.jsonl
```

Example filename from code comments:

```
rollout-2025-05-07T17-24-21-5973b6c0-94b8-487b-a530-2aeb6098ae0e.jsonl
```

Archived sessions move to `~/.codex/archived_sessions/` with the same nested structure.

Source: `codex-rs/rollout/src/recorder.rs`, `codex-rs/thread-store/src/local/helpers.rs` (the
`thread_id_from_rollout_path()` function strips `.jsonl`, extracts the trailing 36-char UUID, and
verifies a hyphen precedes it).

#### JSONL content format

Each line is a `RolloutLine` JSON object: `{ "item": <RolloutItem> }`.

`RolloutItem` variants (from binary strings + source):
- `session_meta` — **always the first line**; contains `id`, `timestamp`, `cwd`, `model_provider`,
  `cli_version`, `source`, `git` (branch/sha/url), `dynamic_tools`
- `response_item` — model turn (text, tool calls, reasoning)
- `event_msg` — events like `ExecCommandEnd`, `HookCompleted`, `TurnComplete`, `SessionConfigured`
- `compacted` — compaction checkpoint
- `turn_context` — context snapshot

Command output is truncated to 10 000 bytes per item on write.

#### Session index (secondary)

`~/.codex/session_index.jsonl` — a bounded list kept for fast recent-session lookup. Not the
primary source of truth; the SQLite DB and rollout files are.

#### Other SQLite files

- `~/.codex/logs_2.sqlite` — debug/telemetry logs only, not session content
- `~/.codex/sqlite/codex-dev.db` — automations and inbox items (scheduled tasks)

### Summary

| Attribute | Value |
|---|---|
| Format | JSONL (`~/.codex/sessions/…/*.jsonl`), indexed by SQLite |
| Session ID | UUID (e.g. `5973b6c0-94b8-487b-a530-2aeb6098ae0e`) |
| Naming | `rollout-<ISO8601-with-dashes>-<uuid>.jsonl` |
| Directory org | Date-partitioned: `sessions/YYYY/MM/DD/` |
| Archive location | `archived_sessions/YYYY/MM/DD/` (same naming) |
| Config env var | `CODEX_HOME` (defaults to `~/.codex`) |

---

## 2. Resume mechanism

```
codex resume [OPTIONS] [SESSION_ID] [PROMPT]
```

Direct inspection of `codex resume --help` (from the local binary):

- `codex resume` — interactive fuzzy-picker showing recent sessions (filtered to current cwd by default)
- `codex resume --last` — jump to the most recent session without picker
- `codex resume --all` — show all sessions across all cwds (adds CWD column to picker)
- `codex resume <SESSION_ID>` — UUID or thread name; UUIDs take precedence if parseable

The `SESSION_ID` argument accepts:
1. A bare UUID (preferred, unambiguous)
2. A "thread name" / title string (fuzzy match)

Additional resume flags: `--cd <DIR>` to override working directory, `--model <MODEL>` to override
model, `-m <FILE>` for images, `--include-non-interactive` to show exec/headless sessions.

**Equivalent to `claude --resume <uuid>` is `codex resume <uuid>`.**

---

## 3. Stop hook / lifecycle integration

**FLAG: Codex DOES have a Stop hook — but it requires a feature-flag to activate.**

### Hook system confirmed

Binary string analysis found extensive hook infrastructure compiled in:
- Source paths: `codex-rs/core/src/hook_runtime.rs`, `codex-rs/hooks/src/events/stop.rs`,
  `codex-rs/hooks/src/engine/dispatcher.rs`, etc.
- Event names embedded in binary: `session_start`, `user_prompt_submit`, `stop`, `pre_tool_use`,
  `post_tool_use`, `pre_compact`, `post_compact`, `permission_request`

### Activation requirement

Hooks are **gated** behind a feature flag. You must add this to `~/.codex/config.toml`:

```toml
[features]
codex_hooks = true
```

Without this flag, hook configuration is parsed but silently skipped.

### Hook configuration format

Hooks can be configured in two equivalent ways:

**Option A — inline in `~/.codex/config.toml`:**

```toml
[features]
codex_hooks = true

[[hooks.Stop]]

[[hooks.Stop.hooks]]
type = "command"
command = "sessync push --quiet  # sessync-auto-push"
timeout = 30
```

**Option B — separate `~/.codex/hooks.json`:**

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "sessync push --quiet  # sessync-auto-push",
            "timeout": 30
          }
        ]
      }
    ]
  }
}
```

Project-level hooks can also live in `<repo>/.codex/hooks.json` or `<repo>/.codex/config.toml`
(only loaded when the project is trusted).

### Stop hook stdin payload

The hook command receives a JSON object on stdin with at minimum:

```json
{
  "session_id": "<thread-uuid>",
  "transcript_path": "<path-to-rollout-jsonl>",
  "cwd": "<working-directory>",
  "hook_event_name": "Stop",
  "model": "o3",
  "permission_mode": "default",
  "turn_id": "<turn-uuid>",
  "stop_hook_active": false,
  "last_assistant_message": "..."
}
```

Source: `codex-rs/hooks/src/events/stop.rs` (StopRequest struct), official hooks docs.

### Stop hook output (what the hook can return)

The hook exits with:
- **Exit 0** + optional JSON on stdout: can include `"decision": "block"` with `"reason"` to
  prevent stopping (request continuation), or `"decision": "continue"` to allow stop.
- **Exit 2**: stderr is treated as a blocking reason (shorthand for block).
- **Other exit codes**: failure (logged, session still stops).

For sessync's use case (auto-push on stop), exit 0 with no output is the correct behavior — just
run the push and let Codex stop normally.

### Similarity to Claude Code

The hook schema is nearly identical to Claude Code's `~/.claude/settings.json` Stop hook:

| Attribute | Claude Code | Codex |
|---|---|---|
| Config file | `~/.claude/settings.json` | `~/.codex/config.toml` or `~/.codex/hooks.json` |
| Hook event key | `"Stop"` | `"Stop"` |
| Command field | `"command": "..."` | `"command": "..."` |
| Feature flag needed? | No | Yes: `[features] codex_hooks = true` |
| stdin payload | JSON with session/cwd | JSON with session/cwd/transcript_path |

---

## 4. Project / cwd association

Codex does **not** encode cwd into a filesystem path the way Claude Code does. Instead:

1. **`cwd` is stored directly** as a plain string in `state_5.sqlite` `threads.cwd` and as the
   `cwd` field in the `session_meta` first line of each rollout JSONL.

2. The `codex resume` picker **filters by current cwd** by default (`--all` disables this filter).
   The filter is performed against the stored `cwd` column in the SQLite index.

3. Rollout files are **date-partitioned**, not cwd-partitioned. There are no per-project
   subdirectories analogous to Claude Code's `~/.claude/projects/-Users-foo-bar/` layout.

### Implication for `project_key_for(cwd)`

The existing `path_codec::project_key_for_cwd()` function (which hashes the cwd string) **can be
reused as-is** for `CodexAdapter`. Both adapters can share the same deterministic hash because:
- The raw `cwd` string is available in both the JSONL `session_meta` line and the SQLite row.
- The hash is stable across machines for the same absolute path (same limitation as Claude Code).

For cross-machine matching where paths differ (e.g. `/Users/alice/proj` vs `/home/alice/proj`), a
`git_origin_url` field is available in `session_meta` and in `threads.git_origin_url`. This could
serve as a richer, machine-independent project key, but that would be a new capability beyond what
the existing trait supports.

---

## 5. Session metadata available

From the `threads` table schema (SQLite, confirmed by inspection) and `session_meta` JSONL line
(from source analysis):

| Field | Available? | Source |
|---|---|---|
| `session_id` (UUID) | Yes | Both SQLite `id` col + JSONL `session_meta.id` |
| `cwd` | Yes | Both |
| `modified_at` (`updated_at`) | Yes | SQLite `updated_at_ms`; JSONL file mtime as fallback |
| `first_user_message` | **Yes — pre-indexed** | SQLite `first_user_message` col (no parsing needed) |
| `title` | Yes | SQLite `title` col |
| `model` | Yes | SQLite `model` col + JSONL `session_meta.model_provider` |
| `tokens_used` | Yes | SQLite `tokens_used` col |
| `cli_version` | Yes | SQLite `cli_version` + JSONL `session_meta.cli_version` |
| `git_branch` | Yes | SQLite `git_branch` |
| `git_origin_url` | Yes | SQLite `git_origin_url` |
| `byte_size` | Via file stat | `rollout_path` on disk, stat the file |
| `source` | Yes | SQLite `source` col: "cli", "vscode", "exec", "mcp", "subagent" |
| `archived` | Yes | SQLite `archived` col |

Key point: **`first_user_message` is pre-indexed in the SQLite `threads` table**. The
`ClaudeCodeAdapter` must parse JSONL lines to find it; `CodexAdapter` can read it directly from
the DB, making `list_local_sessions()` substantially cheaper.

---

## 6. Multi-session per project

**Yes — multiple sessions per cwd are fully supported.**

- The SQLite `threads` table has a composite index `idx_threads_archived_cwd_created_at_ms` on
  `(archived, cwd, created_at_ms)`, confirming that multiple rows share the same `cwd`.
- The `codex resume` picker shows all sessions for a cwd sorted by recency.
- There is no "one active session per project" constraint anywhere in the schema.

This is the same model as Claude Code: many sessions per project directory.

---

## Open questions / what couldn't be answered

1. **Exact path of `rollout_path` when using `CODEX_HOME` override.** The filename format
   (`rollout-<ts>-<uuid>.jsonl`) and date-partitioned directory structure are confirmed from source,
   but the full `precompute_log_file_info()` implementation wasn't readable in the WebFetch
   response. Recommend: run `codex resume --all` in a real session on a machine with usage and
   inspect actual files under `~/.codex/sessions/`.

2. **`transcript_path` in Stop hook stdin vs `rollout_path` in SQLite.** The official docs list
   `transcript_path` as a field in the Stop hook stdin payload. Whether this is the same path as
   `rollout_path` in the SQLite row (likely yes) or a separate ephemeral file needs confirmation
   from a live hook test.

3. **Hook feature flag scope.** It's unclear whether `[features] codex_hooks = true` must be in the
   user-level `~/.codex/config.toml`, or whether it can be set project-locally in
   `<repo>/.codex/config.toml`. If project-scoped, `sessync hook install` for Codex would need to
   instruct users to set it globally.

4. **`codex resume <uuid>` across machines.** If a rollout JSONL file doesn't exist on the
   receiving machine (because `sessync resume` wrote it but the SQLite `threads` row was never
   created), does `codex resume <uuid>` fall back to scanning `~/.codex/sessions/` by filename?
   Source suggests it does (the `ReadThreadByRolloutPathParams` path), but this needs a live test.

5. **Official SDK for reading the SQLite DB.** No published Rust/Python/JS SDK was found for
   reading `state_5.sqlite` programmatically. Recommend using `rusqlite` directly.

---

## Recommendations for `CodexAdapter` implementation

### A. ToolAdapter trait fit

The existing five-method trait fits **without modification**:

| Method | Codex implementation |
|---|---|
| `name()` | `"codex"` |
| `list_local_sessions()` | Query `state_5.sqlite` `threads` table; one row = one `LocalSession`. No JSONL scanning needed for metadata. |
| `read_session(session_id)` | Look up `rollout_path` from SQLite by `id`, read the file bytes. |
| `write_session(session_id, target_cwd, raw)` | Write bytes to `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`, then upsert a row into `threads` so `codex resume` finds it. |
| `project_key_for(cwd)` | Reuse existing `path_codec::project_key_for_cwd(cwd)`. |

One potential addition: a `requires_hook_feature_flag() -> Option<(&'static str, &'static str)>`
method that returns `Some(("features.codex_hooks", "true"))` for tools that require config
activation. But this is cosmetic — the `hook install` subcommand can just print a notice.

### B. Project key derivation

Keep the existing SHA-256 hash of the raw cwd string from `path_codec`. No changes needed.

Optional future enhancement: if `git_origin_url` is non-null, use `sha256(git_origin_url)` as the
project key instead, for cross-machine project matching even when home directory paths differ. This
would be a new trait method `project_key_for_session(meta: &SessionMeta) -> ProjectKey` rather than
modifying the existing `project_key_for(cwd)` signature.

### C. Hook integration

Codex does have a Stop hook with compatible semantics. Recommended approach:

Extend `commands::hook.rs` to support a `--tool` argument:

```
sessync hook install --tool codex
sessync hook install --tool claude-code   # existing behaviour
sessync hook install                      # install for all detected tools
```

For Codex, `install_hook_at` should:
1. Read `~/.codex/config.toml` (TOML, not JSON).
2. Check for `[features] codex_hooks = true`; if absent, add it and warn the user.
3. Append `[[hooks.Stop]] / [[hooks.Stop.hooks]]` block with `type = "command"` and the
   tagged command string.
4. Write back atomically (tmp + rename, same pattern as the Claude Code implementation).

The `SESSYNC_HOOK_TAG` comment (`# sessync-auto-push`) can remain identical — it's just a string
appended to the command.

### D. CLI form for multi-tool

**Recommend option (a): default = all tools, `--tool <name>` filter.**

Rationale:
- `sessync push` should push everything — users typically want all sessions synced.
- `--tool codex` is a natural escape hatch for debugging or selective push.
- Introducing per-tool subcommands (`sessync codex push`) adds surface area without benefit: the
  tools share all infrastructure (OSS, crypto, config) and there's no safety reason to separate them.
- Accidental cross-tool push is not a real risk: each tool's sessions are isolated by the
  `name()` prefix in OSS keys (`claude-code/<hash>/…` vs `codex/<hash>/…`), so there's no
  collision even if both tools are pushed simultaneously.

Option (b) (require `--tool`) is unnecessarily restrictive for the 90% case.
Option (c) (per-tool subcommands) increases discoverability cost without compensating benefit.

### E. Estimated implementation scope

**Medium scope — roughly 3–5 subagent-days.**

Breakdown:

| Task | Estimate |
|---|---|
| `CodexAdapter::list_local_sessions()` via rusqlite | 0.5 day |
| `CodexAdapter::read_session()` (SQLite lookup + file read) | 0.5 day |
| `CodexAdapter::write_session()` (write JSONL + upsert SQLite row) | 1 day |
| Hook install for TOML config | 1 day |
| `--tool` flag on push/resume/ls commands | 0.5 day |
| Integration tests (tmp dir with SQLite fixture) | 1 day |
| **Total** | **~4.5 days** |

The SQLite write in `write_session()` is the highest-complexity task: it must insert a well-formed
row into `threads` with the correct `rollout_path`, `cwd`, `model_provider`, `source`, and
trigger-maintained `created_at_ms`/`updated_at_ms` columns, so that `codex resume <uuid>` finds
the session after a `sessync resume`.

Compare to Claude Code adapter scope: if hooks existed and format was JSONL — Claude Code is that
case, and it was ~3 days. Codex is slightly larger because of the SQLite write requirement.

---

## Risks

### 1. SQLite schema versioning (HIGH)

The DB is named `state_5.sqlite`, implying at least four prior schema versions. OpenAI bumps the
schema version (and renames the file) when making breaking changes. A `CodexAdapter` that writes
to `state_5.sqlite` will **silently fail** after a schema migration — the old file is abandoned and
a new `state_6.sqlite` appears. Mitigation: detect the current state DB by globbing `state_*.sqlite`
and sorting by version number, rather than hardcoding `state_5`.

### 2. Feature flag requirement for hooks (MEDIUM)

`[features] codex_hooks = true` must be set before the Stop hook fires. If a user installs the
hook but forgets the feature flag, auto-push silently never runs. The `sessync hook install --tool
codex` command must check for and set this flag, and `sessync hook status --tool codex` must verify
it is present.

### 3. Hook system is still maturing (MEDIUM)

Hooks shipped behind a feature flag and the changelog shows active weekly changes to hook behaviour
(plugin-bundled hooks, hook trust enforcement, new hook events). The JSON output schema and stdin
payload fields could change without a major version bump. Recommend pinning to the
`hook_event_name` field for identity checks rather than relying on payload shape stability.

### 4. No official SDK for reading/writing the state DB (LOW-MEDIUM)

OpenAI does not publish a versioned Rust/Python crate for `state_5.sqlite`. A `CodexAdapter`
writing raw SQL is coupling to an internal format. However, the table name and column set have been
stable across multiple minor versions, and the risk is bounded because `read_session()` can fall
back to filesystem scanning if the SQLite lookup fails.

### 5. Desktop app vs CLI source divergence (LOW)

The locally-installed Codex is an Electron desktop app wrapping the same Rust core. As of May 2026
the desktop and CLI share `~/.codex/` and `state_5.sqlite`. If OpenAI separates them in the future
(e.g. desktop gets `~/.codex-desktop/`), the adapter root path would need tool-source
discrimination. This is currently speculative.
