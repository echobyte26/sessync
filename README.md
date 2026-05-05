# sessync

Cross-device sync for AI coding agent sessions. v1 ships Claude Code support over Aliyun OSS with client-side age encryption. Designed so the same project on two Macs can share session history without manually copying files.

> Status: **v1 M1 complete** (init / push / resume / status). Real-hardware end-to-end smoke test pending.

## How it works

```
Mac A                          Aliyun OSS                    Mac B
~/.claude/projects/...   →     bucket/sessync/                →   ~/.claude/projects/...
                               claude-code/<pk>/<sid>.age          (current cwd)
                                                .age.meta.json
       │                              │                              │
       ↑ argon2id+age encrypt         │            decrypt+rewrite ↓
       └─ sessync push                │            sessync resume ─┘
```

- **`sessync init`** — one-time setup: OSS endpoint/bucket/AK/SK + a passphrase. Salt is generated once and saved to `~/.config/sessync/config.toml` (chmod 0600). Passphrase goes to macOS Keychain.
- **`sessync push`** — encrypts every local Claude Code session jsonl + a `SessionMeta` sidecar and uploads to OSS.
- **`sessync resume`** — interactive picker: list remote projects → list sessions in the picked project → download + decrypt + drop into the current cwd. Outputs a `claude --resume <id>` hint.
- **`sessync status`** — read-only summary: device, local/remote session counts, last upload, passphrase state, OSS bucket.

## Requirements

- macOS (M1 only ships Keychain backend)
- Rust 1.75+ to build
- Aliyun OSS bucket + an AccessKey (recommended: a RAM sub-account scoped to that bucket)
- Claude Code installed and at least one session already produced

## Build

```bash
cargo build --release
sudo cp target/release/sessync /usr/local/bin/
sessync --version
```

## First-time setup

```bash
sessync init
```

The wizard asks for OSS endpoint, bucket, AccessKeyId, AccessKeySecret, an object key prefix (default `sessync/`), and a passphrase (≥12 chars recommended).

**The passphrase is the only thing standing between your sessions and OSS access keys leaking.** Save it in 1Password or equivalent — losing it means everything in OSS is unrecoverable, even if the salt and AK/SK are still around.

## Daily usage

On the source machine:

```bash
sessync push        # uploads all local Claude Code sessions
sessync status      # confirms what's where
```

On the second machine (after running `sessync init` with the **same passphrase + same OSS creds**):

```bash
cd /path/to/your/project
sessync resume      # pick project → pick session → it lands in cwd
claude --resume <session-id>   # the previous command prints this for you
```

The local cwd on machine B does not need to match the original cwd on machine A — sessync rewrites Claude Code's project directory encoding to match the target.

## Architecture

Two trait-based adapters keep things pluggable:

- `ToolAdapter` (`src/adapter/tool.rs`) — only `ClaudeCodeAdapter` ships in v1
- `StorageAdapter` (`src/adapter/storage.rs`) — `OssStorage` (production) + `InMemoryStorage` (tests)

Encryption is argon2id (m=64MiB, t=3, p=4) → 32-byte key → age passphrase mode. The hex-encoded key is passed to age as a passphrase — yes, this incurs an extra scrypt round inside age; a known v1.x optimization opportunity.

## What's NOT in v1

See `docs/superpowers/v2-backlog.md`. Major absences:

- **No automatic push** — Stop hook + launchd周期 task land in M2
- **No retry queue** — push failures surface immediately
- **No Codex / Cursor support** — `ToolAdapter` interface is ready, no implants
- **No web UI** — pure CLI by design
- **No alternative storage backends** — `StorageAdapter` interface is ready, only OSS implants

## Development

```bash
cargo test                              # 23 unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt
```

The Keychain test is `#[ignore]`'d by default (it touches the real keychain via a test-only service name). Run manually:

```bash
cargo test --lib keychain::tests::roundtrip -- --ignored
```

## Documentation

- [PRD](docs/prd/2026-05-04-sessync-v1.md) — what we're building and why
- [M1 Implementation Plan](docs/superpowers/plans/2026-05-04-sessync-v1-m1-core.md) — 16-task breakdown of how it got built
- [v2 Backlog](docs/superpowers/v2-backlog.md) — what's deferred and why

## License

TBD (open-sourcing is on the v2 path; license decision deferred until then).
