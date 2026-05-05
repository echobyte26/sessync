# Releasing

End-to-end automated. From "I want to ship a new version" to "users can `brew upgrade sessync`" is **3 commands + 8 minutes wait**.

## Quick recipe

```bash
# 1. Bump the version in Cargo.toml. Pick a semver (patch/minor/major).
sed -i '' 's/^version = "[^"]*"/version = "0.2.0"/' Cargo.toml

# 2. Commit + tag + push
git commit -am "chore: bump v0.2.0"
git tag v0.2.0
git push && git push --tags
```

That's it. Within 8-10 minutes:
- GitHub Actions builds a universal macOS binary on a fresh runner.
- The release shows up at `https://github.com/echobyte26/sessync/releases/tag/v0.2.0`.
- The action then bumps `echobyte26/homebrew-sessync` Formula automatically
  (commit appears under `sessync-release-bot` author).
- Anyone running `brew upgrade sessync` immediately gets the new version.

## Verify

```bash
# Watch the workflow
gh run watch --repo echobyte26/sessync

# After it finishes, on this Mac:
brew update && brew upgrade sessync
sessync --version    # should match the new tag
```

## What if I push an early tag by mistake

```bash
git tag -d v0.2.0           # remove local tag
git push --delete origin v0.2.0   # remove remote tag

# Then on GitHub web, delete the failed Release at:
# https://github.com/echobyte26/sessync/releases
```

Then re-tag with a fresh patch number (e.g. `v0.2.1`). Don't reuse the deleted
number — Homebrew may have cached it.

## What the automation requires

Already set up; documented for future-you:

- **`TAP_REPO_TOKEN` secret** in `echobyte26/sessync` repo settings → Actions secrets.
  Fine-grained PAT with `Contents: Read and write` permission on
  `echobyte26/homebrew-sessync` only. Expires (default) — rotate before then.

- **`.github/workflows/release.yml`** in this repo. Triggered by `push` of any
  `v*` tag. Builds → uploads release → bumps the tap formula.

- **`echobyte26/homebrew-sessync`** repo with `Formula/sessync.rb` already
  committed. The auto-bump step assumes it exists and only changes the
  `version` and `sha256` fields.

## When NOT to bump version

If you only changed docs / CI / non-shipped files, no need for a release. Just
`git push` to main — no tag, no Actions run, no new binary.

The version tag is for "users will notice if they upgrade" changes only.
