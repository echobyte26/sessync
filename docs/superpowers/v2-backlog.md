# sessync v2 / v1.x Backlog

> Captured at the M1 merge (2026-05-04). Update as items move to v2 plans.

## Pre-v2 — must do before tagging M1 done

| # | Item | Notes |
|---|---|---|
| ~~P1~~ | ~~Task 16 real-hardware smoke test~~ | **Done 2026-05-04 via local-fs backend.** PRD R2 (cross-path resume side-effects) validated: a session originally taken in `/Users/james/Project/ai/coding/project/azoth` was push'd, resumed into `/private/tmp/sessync-resume-test`, and `claude --resume <sid>` opened the full prior history. Real OSS still untested but the protocol/crypto/path-codec layer is proven. |
| P2 | Decide license | Required before opensourcing. MIT or Apache-2.0 are the obvious choices. |
| P3 | Real-OSS smoke test (deferred) | Same flow over Aliyun OSS instead of local-fs, on two Macs. Optional now that the algorithm is validated; required before opensourcing. |

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
