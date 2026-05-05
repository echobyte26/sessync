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
| **K1** | **Stable codesign identifier** — change the `codesign` step in `.github/workflows/release.yml` from bare `--sign -` to `--sign - --identifier "com.echobyte26.sessync"`. macOS Keychain trust may follow the identifier (designated requirement) instead of the binary hash, making "Always Allow" persist across `brew upgrade`. **Needs empirical verification** — Apple's docs are vague on whether ad-hoc-signed binaries with explicit identifier get a stable DR. Cheap to try (one-line workflow change + ship a release + test on Mac Pro). If it works, the upgrade UX becomes truly silent. | XS |
| **K2** | **(only if K1 fails)** — explore `SecKeychainItemSetAccess` to widen the trust list from "this exact binary" to "any binary signed by ad-hoc cert with sessync identifier". More invasive (requires native Security.framework calls, probably via the `security-framework` crate). | M |
| **K3** | **(out of scope unless we go pro)** — pay $99/year for Apple Developer ID, sign with a real cert. Trust is then anchored on the developer cert which never changes. Not worth it for a personal tool. | — |

### Other v0.2.0 items

> Add to this list as they come up. Collecting before opening the
> implementation branch.

| # | Item | Effort |
|---|---|---|
| _(reserved)_ | _(your next request goes here)_ | _ |

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
| M2-H | **Incremental push** — by-mtime/size delta vs OSS object list to skip unchanged sessions | S3 (PRD) |

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
