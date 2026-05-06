# sessync v2 / v1.x Backlog

> Captured at the M1 merge (2026-05-04). Update as items move to v2 plans.

## Pre-v2 — must do before tagging M1 done

| # | Item | Notes |
|---|---|---|
| ~~P1~~ | ~~Task 16 real-hardware smoke test~~ | **Done 2026-05-04 via local-fs backend.** PRD R2 (cross-path resume side-effects) validated: a session originally taken in `/Users/james/Project/ai/coding/project/azoth` was push'd, resumed into `/private/tmp/sessync-resume-test`, and `claude --resume <sid>` opened the full prior history. Real OSS still untested but the protocol/crypto/path-codec layer is proven. |
| P2 | Decide license | Required before opensourcing. MIT or Apache-2.0 are the obvious choices. |
| P3 | Real-OSS smoke test (deferred) | Same flow over Aliyun OSS instead of local-fs, on two Macs. Optional now that the algorithm is validated; required before opensourcing. |

## v1.x — must-fix design bugs

| # | Item | Notes |
|---|---|---|
| ~~**B1**~~ | ~~**Salt is generated per-device, not shared**~~ | ~~`sessync init` calls `rand::thread_rng().fill_bytes(&mut salt)` locally, so Mac A and Mac B end up with different salts even when filling in the same passphrase. The KDF then produces different keys → Mac B can't decrypt anything Mac A pushed. Discovered 2026-05-05 during real two-Mac smoke test.~~ **Done 2026-05-05.** `load_or_create_salt` in `src/commands/init.rs` checks `<prefix>.sessync-salt` on the backend at init time; first device uploads a fresh salt, subsequent devices reuse it. Validated by 3 integration tests in `tests/init_salt_sharing.rs`. |

## v0.2.0 — registered for next release batch

> Captured 2026-05-05 during real two-Mac smoke test. Don't open the v0.2.0
> implementation branch yet — let more requirements accumulate first, then
> ship one batch.

### Concurrency / divergence handling

Today: `sessync push` blindly overwrites the remote object, so a stale local
session pushed after the remote diverged silently destroys remote progress
(last-writer-wins). PRD Q4 deferred this by assuming users would behave
strictly serially; real usage immediately hit the failure mode.

| # | Item | Effort |
|---|---|---|
| **C1** | **Stale-check on push** — before each `storage.put`, list the remote object's mtime; if remote is newer than local, prompt: `"Remote session <id> is newer than local. Your push will OVERWRITE N bytes of remote content. Continue? [y/N]"`. `--force` skips. **Eliminates silent data loss** but doesn't auto-resolve. | S |
| **C2** | **Fork-on-conflict UI** — when stale-check trips, give the user three concrete choices: (a) discard local + pull remote; (b) overwrite remote (keep local); (c) save the local divergence as a fork (new session_id like `<orig>-fork-<hostname>-<n>`). No data loss in any branch. | M |
| **C3** | **Session lease via OSS conditional put** — when `claude --resume <id>` starts (via Stop hook integration land in M2 or a wrapper), write `<id>.lock` with TTL + heartbeat. Other devices' `sessync resume` checks the lock first and warns "session in use on <hostname>, last seen Xm ago — wait, or steal lock? [W/s]". Prevents divergence rather than resolving it after the fact. Depends on OSS conditional-put support (`x-oss-forbid-overwrite`). | M-L |

C1 unblocks "no more silent loss". C2 makes divergence non-destructive. C3 is the
cleanest UX (Google-Docs-style "X is editing") but requires more plumbing.
Reasonable order: C1 → C2 → C3.

True merge (line-level / CRDT) is deliberately out of scope — Claude Code's
session model is a parentUuid-linked jsonl with tool_use ID dependencies, not
a text file. Any line-level merge would corrupt the chain and break
`claude --resume`. The fork+lock combo is the practical ceiling for v0.x.

### Keychain trust stability across upgrades

Today: every `brew upgrade sessync` produces a new binary with a different
codesign hash, so macOS Keychain treats it as a stranger and re-prompts the
user for their login password the first time they run any sessync command
post-upgrade. Even clicking "Always Allow" in the prompt only persists for
the current binary hash — next upgrade, the same prompt returns.

| # | Item | Effort |
|---|---|---|
| ~~K1 / K2 / K3~~ | ~~stable identifier / SecKeychainItemSetAccess / Developer ID~~ — **superseded by K-new**. All three are macOS-specific patches; user wants "no prompt at all + cross-platform ready". | — |
| **K-new** | **Machine-bound passphrase file (replaces Keychain entirely)** — store passphrase at `~/.config/sessync/passphrase.enc` (chmod 0600 on Unix; Windows ACL Owner-only). Encryption key = `hmac_sha256(machine_id, "sessync-passphrase-v1")` where `machine_id` is OS-specific: macOS `ioreg → IOPlatformUUID`, Linux `/etc/machine-id`, Windows `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`. Properties: (1) install never prompts — file write doesn't trigger any OS dialog; (2) upgrade never prompts — encryption key is binary-independent; (3) cross-platform — same code path everywhere; (4) leaked file useless without same machine_id; (5) breaks if user moves config to another machine, but that's the desired behavior (re-init on new machine). Drops macOS Keychain dependency entirely. New cargo deps: `machine-uid` (or hand-written platform abstraction), `hmac`, already-have `sha2` + `age`. | M |

### Performance — `sessync resume` is 5-10s slow despite Q4 fix

Q4 (buffered(8) concurrent meta fetches) was supposed to bring resume
down to 1-2s. Real measurement on Mac Pro behind an HTTP proxy:
- "input command → 'Pick a project' picker": **>10s**
- "pick project → 'Pick a session' picker": **7-9s**

Total >15s on a 27-session bucket. Root cause: `aliyun-oss-client = 0.13` constructs a fresh
`reqwest::Client` per API call (verified at `object.rs:249,285,374` and
`bucket.rs:346`), so every OSS call pays full TLS handshake + proxy hop
even though our async layer is concurrent. With ~250ms per call and 25+
calls per resume, total walltime stays in the multi-second range.

| # | Item | Effort | Expected gain |
|---|---|---|---|
| **P1** | **Encrypted local meta cache (full design — does NOT skip OSS list)** — every resume still calls `storage.list(prefix)` once (~700ms) to get the truth-source `(key, mtime, size)` snapshot. Compare to `~/.cache/sessync/meta-cache.age` (age-encrypted under the same key as remote, so passphrase change auto-invalidates). Per object: cache hit (same mtime+size) → reuse decrypted SessionMeta from cache, skip GET; cache miss / stale mtime / new key → GET + decrypt + write back to cache. Tombstone keys removed from cache when remote no longer lists them. Cache file carries a `schema_version` so future structure changes auto-invalidate. **Permanent fallback if cache load fails or decryption fails: full re-fetch (today's behavior).** Net: cold or invalidated 5-10s; steady-state with no remote changes ~1s; with K new sessions added by another device ~1s + K×200ms. Never serves stale data because list is the truth. | M | 5-10s → ~1s steady, ~2-3s incremental |
| **P2** | **`argon2id` params one notch lower** — m=32MiB t=2 p=2 instead of m=64MiB t=3 p=4. Halves KDF wall time (~200ms saved). Security still well above OWASP minimums. | XS | -200ms first run |
| **P3** | **Connection pooling — likely fork-and-patch path (SDK swap considered, rejected)** — investigated alternatives (xt-oss 0.5.7 was the only crate with a reused reqwest::Client field, but flagged immature: 14 stars, ~1 year since last crates.io release). Remaining options: (a) fork `aliyun-oss-client` and patch internals to reuse a single `reqwest::Client` (1-2 days, must maintain fork); (b) hand-write OSS v4 signing + thin reqwest layer (2-3 days, ~200-300 LoC, zero dep risk). Defer until P1 is shipped — P1 may obviate the need (steady-state is dominated by list latency, which one TLS handshake can't help much beyond a few hundred ms). | L | only matters cold-start, ~5-10s → ~1s |
| **P4** | **Eager parallel prefetch** — kick off the per-project meta fetch loop in background while user is choosing project; have session metas warm by the time they pick. Hides remaining latency behind the human's selection time. Largely subsumed by P1. | S | -1-2s perceived (only without P1) |
| **P5** | **`buffered(8)` → `buffered(32)` or unbounded** — current cap is conservative; would only help after P3 lands. | XS | depends on P3 |

Priority order: **P1 first by a wide margin** (covers 95% of real-world UX);
P3 deferred unless cold-start really hurts after P1 ships; P2/P4/P5 are
micro-optimizations with diminishing returns.

### Auto push (urgent — promoted from M2 backlog)

Today: user must run `sessync push` manually after every Claude Code
conversation, otherwise the other Mac sees stale state. PRD Q6 had
already specified the answer (D: Stop-hook immediate + periodic safety net),
but it was scoped into M2; surfaced as urgent now during real two-Mac use.

| # | Item | Effort | Notes |
|---|---|---|---|
| **A1** | **Claude Code Stop-hook integration** — `sessync hook install / uninstall` writes/removes a Stop hook config in `~/.claude/settings.json` (or `~/.config/claude/...`, whichever Claude reads from). The hook spawns `sessync push --quiet` after every conversation ends. Quiet flag suppresses normal output so Claude's terminal stays clean; errors still surface to log. Idempotent install (re-run upgrades the hook script in place). | M | Most user-visible win — push truly disappears from the workflow. |
| **A2** | **launchd periodic safety net** — `sessync daemon install / uninstall` writes a LaunchAgent plist that runs `sessync push --retry-pending` every N minutes (default 5). Runs in the background, no UI. Catches the case where Stop hook didn't fire (Claude crashed, machine slept mid-conversation, etc.). | M | Belt-and-braces backup for A1. |
| **A3** | **Pending queue (SQLite)** — when push fails (network down, OSS auth flap, lock conflict), enqueue rather than just logging an error. Next push attempt drains the queue first. `sessync status` surfaces queue depth. Plays well with C1/C2 divergence-detection (a deferred push respecting newer remote can fork on retry). | M | Eliminates silent push loss when network is flaky. |
| **A4** | **macOS notification on N consecutive failures** — `osascript -e 'display notification ...'` after 3 failed pushes in a row. Keeps user aware without spamming. | S | Closes the loop on A3 — if the queue grows, user knows. |
| **A5** | **Incremental push** — `storage.list(prefix)` once per push to get remote `(key, mtime, size)`, compare to local `SessionMeta`, only PUT new/changed sessions. Mirror of P1's resume cache logic but inverted (local → remote). v0.2.x always re-uploads all N sessions; with auto-push (A1) firing after every conversation, that's 2N OSS PUT calls + tens of seconds every time. Steady-state with no new sessions: 1 list, 0 PUTs, ~700ms. Was originally registered as M2-H; promoted to v0.3.0 to address user-visible auto-push slowness. | M | Promoted from M2-H. |
| **A6** | **Selective `sessync push <session-id>`** — pass one or more session ids and only push those. Useful for "I just want to share this one without pushing other in-progress noise." Was originally registered as polish row U5; promoted to v0.3.0 alongside A5 (both touch the push command body). | S | Promoted from U5. |

Order: A1 unlocks "push is automatic" → A2 makes it reliable → A3 makes
failures recoverable → A4 makes failures visible → A5 makes it fast (essential
once A1 fires after every conversation). A1 alone solves 80% of the
manual-push annoyance, A5 makes the auto-push tolerable.

### Polish — surfaced via PM-style audit 2026-05-05

Smaller UX / safety / observability items found while reviewing the daily
workflow. Most are XS-S; bundle into whichever release has spare cycles.

#### Resume UX

| # | Item | Effort |
|---|---|---|
| **U1** | Selector sorted by **most-recent activity** (not alphabetical project_key) — most recently touched session shows first; saves scrolling on a 27-session bucket | XS |
| **U2** | Bump preview from 80 → 200 chars (or `--full-preview` flag) — current 80 too short to recognize what was discussed | XS |
| **U3** | After resume, prompt `Launch claude --resume now? [Y/n]` and spawn claude in current shell — saves manual cd + paste | S |
| **U4** | `sessync ls` command — non-interactive list of local + remote sessions, grep/awk friendly | S |
| ~~U5~~ | ~~`sessync push <session-id>` — selectively push one session~~ — **promoted to v0.3.0 as A6** (touches the push command, batches with A5 incremental) | — |

#### Diagnostics

| # | Item | Effort |
|---|---|---|
| **D1** | `sessync doctor` — self-test: OSS reachability, Keychain access, hook install state, launchd state, cache health | M |
| **D2** | `sessync logs` — tail recent log without making user dig in `~/Library/Logs/sessync/` | XS |
| **D3** | `sessync status` enhancement — last 5 push timestamps + sizes, cache hit rate, pending queue depth | S |

#### CLI ergonomics

| # | Item | Effort |
|---|---|---|
| **L1** | Shell completion (`sessync completion zsh / bash / fish`) — tab-complete subcommands and flags | S |
| **L2** | Default action when invoked without subcommand: print `sessync status` summary instead of help | XS |
| **L3** | **`sessync init` UI redesign** — current is a flat list of 7 plain-text prompts. Replace with: (a) sectioned headers (`OSS Backend`, `Credentials`, `Encryption`); (b) `Endpoint` becomes a Select with the 5-7 common Aliyun regions + `Custom...`; (c) colored hints (e.g. dim grey example values); (d) success/failure check marks (✓/✗) on each step. Optionally consider replacing `dialoguer` with `inquire` for a modern look out of the box. | M |
| **L4** | **`sessync status` UI redesign** — current is plain key:value lines. Replace with: (a) sectioned output (`Device`, `Sessions`, `Health`); (b) colored OK/WARN/FAIL markers; (c) relative-time formatting (`2 hours ago` instead of UTC ISO); (d) health checks (passphrase ✓, hook ✗ if not installed, cache hit rate %). | S |
| **L5** | **`sessync help` enhancement** — clap auto-generates the current plain output. Add a small ASCII banner, an `EXAMPLES:` section per subcommand, color the subcommand names. Use clap's `before_help` / `after_help` / `help_template` features so we don't ship custom help-rendering code. | S |
| **L6** | **Project-wide colored output crate** — pull `owo-colors` (lightweight, no_std-friendly) and define a small style guide module (`crate::ui::style`). Every println! that prints status / errors / hints uses the styles. Auto-disable on non-TTY (piped stdout). | S |

#### Safety / mistake-prevention

| # | Item | Effort |
|---|---|---|
| **S1** | `sessync uninstall --purge-remote` — require user to type bucket name to confirm (1Password-style irrevocable-action gate) | XS |
| **S2** | `sessync push --dry-run` — print which sessions would be pushed without actually uploading | XS |

---

## v1.x — quick wins worth doing before M2

These were caught in code reviews during M1 and explicitly deferred. None block functionality.

| # | Item | Source | Effort |
|---|---|---|---|
| Q1 | Replace `age::Encryptor::with_user_passphrase` with raw symmetric AEAD (chacha20-poly1305 directly) | Task 2/3 review — current path runs scrypt internally on top of our argon2id, ~200ms wasted per session | M |
| Q2 | Migrate `load_passphrase` and `derive_key` signatures to `secrecy::SecretString` | Task 1/2 review — keys/passphrases currently land in plain `String`s, not zeroized on drop | M |
| Q3 | Hard byte-cap on `first_user_message_preview` (e.g. 50 lines / 1MB) | Task 10 review — prevents stuck preview on a malformed huge jsonl | S |
| Q4 | Per-dir `tracing::warn!` + skip on permission errors in `list_local_sessions` | Task 10 review — currently a single bad project dir kills the whole list | S |
| Q5 | `tokio::time::timeout` wrapper around OSS calls | Task 7 review — SDK has no built-in timeout, stalled bucket hangs push/resume forever | S |
| Q6 | Atomic init wrapper covers both write paths | Task 11 review — current rollback only handles config.save failure, not partial keychain corruption | S |
| Q7 | Confirm-before-overwrite when `sessync resume` would overwrite an existing local session | Task 13 review — currently silent overwrite of any in-progress local-only work | S |
| Q8 | Edge-case tests for `path_codec` (empty string, trailing slash, literal `-`, Windows-style paths) | Task 9 review — properties locked into docs but not asserted | XS |

## M2 — automation tier

Originally scoped out of M1 per PRD. Each becomes its own implementation plan in `docs/superpowers/plans/`.

| # | Item | PRD Must |
|---|---|---|
| M2-A | **Stop hook integration** — shell script that spawns `sessync push` after each Claude Code conversation; `sessync hook install/uninstall` | M5, M6 |
| M2-B | **launchd periodic task** — plist generator + `sessync daemon install/uninstall`, runs `sessync push --retry-pending` every 5 minutes | M7 |
| M2-C | **Pending queue** — SQLite-backed retry: failed pushes go in a queue, ack on success, surface count in `status` | M8 |
| M2-D | **Failure notifications** — osascript wrapper, threshold counter (notify after 3 consecutive failures) | M12 |
| M2-E | **Log rotation** — `~/Library/Logs/sessync/` with daily rotation | M13 |
| M2-F | **`sessync doctor`** — self-test (OSS connectivity, Keychain probe, hook install state, launchd status) | S2 |
| M2-G | **Concurrent push** — `tokio::join_all` chunked at, say, 16 in flight, once retry queue exists for failure isolation | Q5/perf |
| ~~M2-H~~ | ~~Incremental push~~ — **promoted to v0.3.0 as A5** (auto-push made the all-N-sessions cost too visible) | — |

## v2 — feature expansion

Conditional on v1 actually getting use. Don't pre-pay any of these.

| # | Item | Trigger |
|---|---|---|
| V2-1 | **Codex `ToolAdapter` implementation** | User starts using Codex regularly OR ≥2 GitHub issues asking for it |
| V2-2 | **Cursor `ToolAdapter` implementation** | Same trigger as V2-1 |
| V2-3 | **Cloudflare R2 `StorageAdapter`** | If Aliyun OSS becomes a real pain (account issues, latency from outside CN) |
| V2-4 | **MinIO / self-hosted `StorageAdapter`** | If "I don't want to depend on any cloud" complaints surface |
| V2-5 | **Multi-user / open-source release** | When community echo around session sync is real (PRD risk R5 — currently unverified) |
| V2-6 | **macOS menu-bar status icon** | If CLI `status` becomes annoying to invoke; one-click resume from the menu |
| V2-7 | **Web UI** | Probably never. Local-only HTTP server has all the auth headaches without obvious UX wins over CLI |
| V2-8 | **Mobile (iOS) viewer** | Browse-only; probably never given the security model |
| V2-9 | **Linux / Windows builds** | When the user actually has a Linux dev box + Linux Claude Code workflow |

## Why some things are NOT here

- **Claude Code ↔ Codex format conversion**: hard PRD boundary, deliberate. Don't entertain.
- **Real-time collaborative session editing**: PRD scope is series (one device finishes → another starts), not parallel. Don't entertain.
- **Encrypting OSS keys in config.toml**: chmod 0600 + macOS user isolation is the right v1 trust boundary. If we ever support shared-machine scenarios, revisit.
