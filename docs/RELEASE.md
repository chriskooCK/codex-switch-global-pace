# Release process

The quality gate is defined once by `.github/workflows/ci.yml`. Pushes to `dev` and pull requests targeting `dev` or `master` run tests, Clippy, and a locked build on Linux, macOS, and Windows. The Linux quality job also runs formatting, `cargo audit`, and installer syntax checks. `.github/workflows/release.yml` calls that same workflow for the exact tag commit before any release build can start, then builds only for `v*` and `dev` tag events. Stable tags are published by the workflow. A rolling `dev` run uploads one verified, attested release bundle, and `scripts/publish-dev.ps1` publishes that exact bundle from the maintainer's authenticated GitHub CLI.

This document is for maintainers. Users should follow the installation and update instructions in the README and do not need to manage Git tags.

## Release eligibility

All of the following must be true before any release push:

- The local branch is `dev`, the worktree is clean, and all intended changes are committed.
- `VERSION` contains the target base version, `Cargo.toml` matches it, and the top of `docs/CHANGELOG.md` contains the matching release section. Ordinary dev builds may keep it Unreleased; allocate the final dev candidate's stable base from the real local date before publishing it so promotion requires no edit.
- Independent code review has no CRITICAL or HIGH findings. Authentication, update, or user-data changes also require security review.
- The local quality gate and a real CLI smoke test pass.
- `gh auth status` shows the intended maintainer account. Development publication needs repository push access and workflow authorization because the `dev` commit can contain workflow changes relative to the default branch.
- `git push` has explicit authorization and the commit to publish is recorded.

A development release has two gates: push the branch and wait for all three CI hosts to pass, then move the `dev` tag to trigger the Release workflow. Never move the tag while branch CI is failing.

The final development release before a stable release has an additional acceptance gate:

- Finish code, tests, changelog, README, and repository documentation before publishing the final `dev` build.
- Record the exact commit SHA and ask the maintainer to test that build.
- After acceptance, make no code, documentation, formatting, lockfile, or metadata changes.
- Fast-forward `master` to that exact commit and create the stable tag on the same commit.
- If any change is needed, publish and test a new `dev` build; the previous acceptance no longer qualifies.

## Version policy

Base versions use the SemVer-compatible `YYYYMMDD.N.0` format:

- `YYYYMMDD` is the version-allocation date captured with `date` immediately before publishing the candidate; 2026-07-12 becomes `20260712`. A stable promotion may happen on a later calendar date and keeps the accepted candidate's version.
- `N` is the release sequence allocated on that date, starting at `1`; the second candidate allocated that day is `20260712.2.0`.
- The final component is always `0` because Cargo and SemVer require `major.minor.patch`. Do not use the invalid two-component form `20260712.1`.
- Keep the date in `YYYYMMDD` order; `YYYYDDMM` breaks chronological sorting.
- Migrating from `0.0.x` to the calendar version is an upgrade. Never publish a smaller `0.x` version afterward because self-update will treat it as a downgrade.

| Pushed tag | Version produced by CI | GitHub Release name | Self-update channel |
|---|---|---|---|
| `dev` (rolling, overwritten) | `YYYYMMDD.N.0-dev` | `dev (YYYYMMDD.N.0-dev)` | `--dev` |
| `vYYYYMMDD.N.0-<suffix>` (permanent prerelease) | `YYYYMMDD.N.0-<suffix>` | Same as tag | Unavailable to the hardcoded `dev` channel |
| `vYYYYMMDD.N.0` (stable) | `YYYYMMDD.N.0` | Same as tag | Default channel |

> The root `VERSION` file is the release source of truth. The `version` field in `Cargo.toml` mirrors it and never includes `-dev`; CI validates the match and adds the suffix during version injection.
> The final dev and stable builds come from the same commit. The Release workflow adds `-dev` for the rolling `dev` tag and leaves the manifest base unchanged for the stable tag; this version display difference does not require a source edit.
>
> The `--dev` path in `src/update.rs` calls `fetch_release(Some("dev"))`, so self-update cannot discover an independently named prerelease tag.

## ⚠ `dev` is both a branch and a tag

This repository uses `dev` as both the development branch and the rolling release tag. **Use full refspecs for every push, delete, and lookup** or Git can report:

```
error: src refspec dev matches more than one
```

or operate on the wrong ref.

## Publish a development release

Prerequisite: `dev` contains every intended commit and the local worktree is clean.

```bash
# 1) Run the local gate. This is a preflight, not the source of release artifacts.
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all --locked
cargo audit
bash -n scripts/install.sh

# 2) Push the dev branch with a full refspec.
git push origin refs/heads/dev:refs/heads/dev

# 3) Wait for branch CI and confirm the remote branch points to this commit.
gh run list --branch dev --workflow CI --limit 1
git rev-parse refs/remotes/origin/dev

# 4) Delete the old remote dev tag before moving it.
git push origin :refs/tags/dev

# 5) Recreate the local dev tag at HEAD.
git tag -d dev && git tag dev

# 6) Push the tag to rerun the exact-source quality gate and build the six
#    locked targets into one verified Actions artifact.
git push origin refs/tags/dev:refs/tags/dev

# 7) Wait for that Release run to succeed, then publish its exact artifact.
#    The script resolves the successful run by the current remote dev-tag SHA.
pwsh -NoProfile -File ./scripts/publish-dev.ps1
```

> Step 6 **must not use `git push origin dev`** because the branch and tag names are ambiguous. Use `refs/tags/dev:refs/tags/dev`.
>
> Step 2 likewise requires `refs/heads/dev:refs/heads/dev`.

If the same commit intentionally has more than one successful Release run,
choose the intended run explicitly with `-RunId <run-id>`; the publisher never
guesses between them.

GitHub Actions Release builds are the only distribution source of truth; `publish-dev.ps1` downloads the Actions artifact and never publishes from local `target/release`. The Release job verifies every archive against its `.sha256` and generates the Sigstore provenance bundle before uploading the development bundle. The publisher independently checks the remote tag, workflow run, exact file set, checksums, provenance, packaged version, and uploaded bytes before it exposes the new release.

Release runs are serialized per tag. The workflow resolves lightweight or
annotated tags to their commit before producing any distribution bundle. Stable
publication keeps the isolated-draft flow inside Actions. Its candidate name is
deterministic for the stable tag, so a rerun can remove a draft left by a lost
runner only after matching its source SHA, tag, release metadata, body marker,
and every uploaded candidate asset; any mismatched release or ref is preserved
and blocks publication. If the publish request commits but its response is lost,
cleanup verifies and preserves the published release. The serialized rerun then
recognizes that exact final release instead of deleting it as an incomplete draft.
Development
publication is deliberately local because GitHub's built-in Actions token cannot
modify a Release whose target changes workflow files relative to the default
branch. The publisher uses the maintainer's existing `gh` authentication; no
personal token is copied into repository secrets and no permission fallback is
attempted. A local mutex prevents overlap on one machine. Before its first
Release mutation, the publisher also atomically creates the single remote
`refs/tags/codex-switch-publish-dev-lock` lock as a uniquely identified annotated
tag. Only the process that receives and verifies the successful create response
owns that lock. An existing lock or an ambiguous create response is never stolen
or removed automatically. While the lock is held, candidate/park release tags
form the crash-recovery journal. Each journal tag explicitly records whether the
prior `dev` release was a draft or public, so recovery never guesses visibility
from a temporarily parked release. That value is also bound into the v2 prior-
release fingerprint. After exact verification, a successful replacement always
finalizes the new candidate as public (`draft=false`). If replacement fails or is
interrupted, rollback instead restores the prior release to its original tag and
exact draft/public state without temporarily publishing a prior draft. Here,
exact recovery covers release metadata, asset identity and bytes, tag, and draft
flag; it does not claim to restore GitHub's
server-generated timestamps.
Rerunning the publisher verifies that journal byte-for-byte before it restores an
interrupted cutover or continues with a new one. Normal exit removes only the
exact lock object it created and confirms the ref is absent.

- Linux / macOS: `.tar.gz` archives named `codex-switch-global-pace-{linux,darwin}-{amd64,arm64}.tar.gz` plus `.sha256`
- Windows: `.zip` archives named `codex-switch-global-pace-windows-{amd64,arm64}.zip` plus `.sha256`
- Build provenance: `codex-switch-global-pace-build-provenance.json`, covering every release archive
- `install.sh` / `install.ps1`
- User update path: `codex-switch-global-pace self-update --dev`

Post-release verification must confirm at least:

- The GitHub Actions Release run succeeds, including all six builds and the release job; for `dev`, `publish-dev.ps1` also completes successfully.
- A platform archive downloaded from GitHub Releases matches its `.sha256`.
- A current GitHub CLI verifies that archive against `codex-switch-global-pace-build-provenance.json` with the repository, `.github/workflows/release.yml`, exact tag ref, the full commit digest reached by that tag, and self-hosted runners denied.
- The unpacked release binary reports the CI-injected version with `codex-switch-global-pace --version`.
- After `publish-dev.ps1` succeeds, the release is public and the original release path works, for example `codex-switch-global-pace self-update --check --dev`.

## Publish a stable release

Do not run these commands until the maintainer has explicitly accepted the final `dev` build. First verify that the tested development tag, `dev`, and the local `dev` branch all resolve to the same commit.

```bash
# 1) Record and compare the accepted commit before changing master.
git rev-parse refs/heads/dev
git rev-parse refs/tags/dev

# 2) After explicit user acceptance, fast-forward master without edits.
git checkout master
git merge --ff-only refs/heads/dev
git push origin refs/heads/master:refs/heads/master

# 3) Tag that exact commit. This example is the first release on 2026-07-12.
git tag v20260712.1.0
git push origin refs/tags/v20260712.1.0:refs/tags/v20260712.1.0

# 4) CI builds six targets and creates the verified GitHub Release.
```

After tagging, confirm `refs/heads/master`, `refs/tags/dev`, and the stable tag still point to the accepted SHA. A mismatch is a release blocker.

A stable promotion may happen on a later calendar date. Do not bump or edit `VERSION`, `Cargo.toml`, or `docs/CHANGELOG.md` after acceptance; doing so would invalidate the tested candidate.

Before publishing the final dev candidate:

- Run `date` to obtain the real local date, then bump `VERSION` and the synchronized `Cargo.toml` to that day's `YYYYMMDD.N.0`.
- Add the matching `## vYYYYMMDD.N.0 — YYYY-MM-DD` section at the top of `docs/CHANGELOG.md`.

## Troubleshooting

**`error: src refspec dev matches more than one`**
Use `refs/heads/dev:refs/heads/dev` for the branch or `refs/tags/dev:refs/tags/dev` for the tag.

**The dev tag was pushed but CI did not run**
Check whether the Release workflow was triggered and whether `on.push.tags` still includes `"dev"`.

**The Release workflow succeeded but the dev GitHub Release did not change**
The workflow intentionally stops after creating the verified development bundle. Run `pwsh -NoProfile -File ./scripts/publish-dev.ps1` from the repository root; it selects only a successful Release run whose source SHA still equals the remote `dev` tag.

An existing mutable SHA-bound `dev` prerelease may be either draft or public.
Do not publish a prior draft manually as a workaround. The publisher records that
visibility in its recovery journal, parks the old release as an isolated draft,
and restores the exact original visibility if replacement fails. A successful
replacement assigns `dev` to only the newly verified candidate and makes that
candidate public. The prior visibility is used only for exact rollback.

**The remote development-publication lock already exists**
Do not rerun with a different token or delete the lock speculatively. First make
sure no publisher process is active and inspect both the lock and any
`dev-candidate-*` / `dev-park-*` journal releases. The lock is intentionally not
auto-recovered after a lost create response. Once its exact annotated-tag object
and the journal state have been reviewed, remove that one ref explicitly with
`gh api --method DELETE repos/chriskooCK/codex-switch-global-pace/git/refs/tags/codex-switch-publish-dev-lock`,
then rerun the publisher so journal recovery happens under a newly acquired lock.

**`self-update --dev` cannot find the new build**
The GitHub Release tag must be the lowercase literal `dev` and the release must be public (`draft=false`). A separate tag such as `v20260712.1.0-dev` creates an independent prerelease that the client channel cannot see, and a `dev` draft is intentionally unavailable to unauthenticated clients.

**Should the Cargo.toml version contain `-dev`?**
No. CI appends `-dev`; the local manifest keeps the clean `YYYYMMDD.N.0` base. Increment `N` before another candidate on the same date or clients will treat it as the version they already have.
