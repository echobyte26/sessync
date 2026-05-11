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

## v0.3.0 — shipped 2026-05-06

> Combined v0.3.0 + v0.4.0 scope. 15 of 16 items shipped (C3 deferred to v0.4.0).

### Concurrency / divergence handling

Today: `sessync push` blindly overwrites the remote object, so a stale local
session pushed after the remote diverged silently destroys remote progress
(last-writer-wins). PRD Q4 deferred this by assuming users would behave
strictly serially; real usage immediately hit the failure mode.

| # | Item | Effort |
|---|---|---|
| ~~**C1**~~ | ~~**Stale-check on push**~~ — **Shipped v0.3.0**. C1 ended up as a stderr warning + `--no-stale-warn` flag rather than an interactive prompt (more hook-friendly). Last-writer-wins still applies; C2/C3 give the user better escape hatches. | S |
| ~~**C2**~~ | ~~**Fork-on-conflict UI**~~ — **Shipped v0.3.0**. Implemented as `sessync push --fork-on-conflict` (opt-in flag). On stale: writes local under `{session_id}.fork-{hash}.age` with a fork-suffixed session_id, original remote untouched, `sessync resume` shows both side by side. | M |
| **C3** | **Session lease via OSS conditional put** — **Deferred to v0.4.0**. Implementation requires bypassing aliyun-oss-client SDK (no header customization on `upload()`) — must hand-craft signed PUT via reqwest with `x-oss-forbid-overwrite: true`. ~150 LOC + OSS auth signing risk. C2 + C1 + queue cover the majority of practical race scenarios; C3 closes the truly-concurrent narrow window, lower marginal value once C2 ships. | M-L |
| ~~**C-etag**~~ | ~~Per-session ETag tracking for true stale detection~~ — **Shipped v0.4.0**. session_etags table in queue, record after PUT via head(), compare on next push. Restores --fork-on-conflict and stale-warn (no-op since v0.3.2). | M |

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
| ~~**A1**~~ | ~~**Claude Code Stop-hook integration**~~ — **Shipped v0.2.0**. | M | |
| ~~**A2**~~ | ~~**launchd periodic safety net**~~ — **Shipped v0.3.0**. `sessync launchd install/uninstall/status` writes `~/Library/LaunchAgents/com.sessync.push.plist` with StartInterval=1800s. | M | |
| ~~**A3**~~ | ~~**Pending queue (SQLite)**~~ — **Shipped v0.3.0**. `~/.local/share/sessync/queue.db` with `pending_pushes` + `push_outcomes` tables. push_all drains queue at start with 60s cooldown. | M | |
| ~~**A4**~~ | ~~**macOS notification on N consecutive failures**~~ — **Shipped v0.3.0**. Fires exactly at N==3 (not >=3) to avoid spamming. | S | |
| ~~**A5**~~ | ~~**Incremental push**~~ — **Shipped v0.3.0**. mtime-based skip; output `pushed N (skipped M unchanged)`. | M | |
| ~~**A6**~~ | ~~**Selective `sessync push <session-id>`**~~ — **Shipped v0.3.0**. Multi-arg positional, unknown ids fail hard. | S | |

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
| ~~**U1**~~ | ~~Selector sorted by **most-recent activity**~~ — **Shipped v0.2.0**. | XS |
| ~~**U2**~~ | ~~Bump preview from 80 → 200 chars~~ — **Shipped v0.2.0**. | XS |
| ~~**U3**~~ | ~~Auto-launch claude after resume~~ — **Shipped v0.3.0**. Default ON, opt-out via `--no-launch`. | S |
| ~~**U4**~~ | ~~`sessync ls` command~~ — **Shipped v0.3.0**. Group by project, sorted by recency. `--project <key>` filter, `--json` for scripting. | S |
| ~~U5~~ | ~~`sessync push <session-id>`~~ — **Shipped v0.3.0 as A6**. | — |

#### Diagnostics

| # | Item | Effort |
|---|---|---|
| ~~**D1**~~ | ~~`sessync doctor`~~ — **Shipped v0.3.0**. Sections: Config / Storage / Hook / launchd (mac) / Queue / Cache / PATH. Pure classifiers (auth vs network) unit-tested. | M |
| ~~**D2**~~ | ~~`sessync logs`~~ — **Shipped v0.3.0**. Reads queue's `push_outcomes` table; relative time + ✓/✗ marker. | XS |
| ~~**D3**~~ | ~~`sessync status` enhancement~~ — **Shipped v0.3.0**. New "Auto-push" section: hook / launchd / queue pending / last push outcome. | S |

#### CLI ergonomics

| # | Item | Effort |
|---|---|---|
| ~~**L1**~~ | ~~Shell completion~~ — **Shipped v0.3.0** as `sessync completions <shell>` (zsh/bash/fish/powershell/elvish). | S |
| ~~**L2**~~ | ~~Default action prints status~~ — **Shipped v0.2.0**. | XS |
| ~~**L3**~~ | ~~`sessync init` UI redesign~~ — **Shipped v0.2.0**. | M |
| ~~**L4**~~ | ~~`sessync status` UI redesign~~ — **Shipped v0.2.0**. | S |
| ~~**L5**~~ | ~~`sessync help` enhancement~~ — **Shipped v0.3.0**. clap `after_help` block with EXAMPLES / DOCS / CONFIG sections. | S |
| ~~**L6**~~ | ~~Project-wide colored output crate (`owo-colors`)~~ — **Shipped v0.2.0**. | S |

#### Safety / mistake-prevention

| # | Item | Effort |
|---|---|---|
| ~~**S1**~~ | ~~`sessync uninstall --purge-remote` confirm~~ — **Shipped v0.2.0**. | XS |
| ~~**S2**~~ | ~~`sessync push --dry-run`~~ — **Shipped v0.3.0**. Pure plan builder, prints `would push/skip/fork` per session + summary. | XS |

---

## v1.x — quick wins worth doing before M2

These were caught in code reviews during M1 and explicitly deferred. None block functionality.

| # | Item | Source | Effort |
|---|---|---|---|
| ~~Q1~~ | ~~Replace age internal scrypt with chacha20-poly1305~~ — **Shipped v0.5.0** as XChaCha20-Poly1305. SSC1 magic prefix, age fallback for backward compat. | — | M |
| Q2 | Migrate `load_passphrase` and `derive_key` signatures to `secrecy::SecretString` | Task 1/2 review — keys/passphrases currently land in plain `String`s, not zeroized on drop | M |
| ~~Q3~~ | ~~Hard byte-cap on `first_user_message_preview`~~ — **Shipped v0.4.0** (1 MiB per-line cap, oversize lines skipped) | — | S |
| ~~Q4~~ | ~~Per-dir warn + skip in `list_local_sessions`~~ — **Shipped v0.5.0** | — | S |
| ~~Q5~~ | ~~`tokio::time::timeout` wrapper around OSS calls~~ — **Shipped v0.4.0** (30s on put/get/list/delete/head) | — | S |
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
| ~~V2-1~~ | ~~Codex `ToolAdapter` implementation~~ — **Shipped v0.6.0**. Reads `~/.codex/state_*.sqlite` + `~/.codex/sessions/.../rollout-*.jsonl`. Multi-tool dispatch refactor + hook --tool flag (TOML) + auto-push setup loops adapters. | — |
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
