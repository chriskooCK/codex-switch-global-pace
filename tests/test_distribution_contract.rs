use std::fs;
use std::path::{Path, PathBuf};

fn repo_file(path: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    normalize_line_endings(&text)
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn assert_before(text: &str, first: &str, second: &str) {
    let first_pos = text
        .find(first)
        .unwrap_or_else(|| panic!("missing required content: {first}"));
    let second_pos = text
        .find(second)
        .unwrap_or_else(|| panic!("missing required content: {second}"));
    assert!(
        first_pos < second_pos,
        "expected `{first}` to appear before `{second}`"
    );
}

#[cfg(unix)]
fn unix_uninstall_harness(script: &str) -> String {
    let definitions = script
        .split("# Parse arguments")
        .next()
        .expect("Unix installer function definitions");
    let uninstall_start = script.find("run_uninstall() {").unwrap();
    let uninstall_end = script[uninstall_start..].find("# ── Install").unwrap() + uninstall_start;
    format!(
        "{definitions}\n{}\n",
        &script[uninstall_start..uninstall_end]
    )
}

#[cfg(unix)]
fn unix_installer_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

fn markdown_links(text: &str) -> Vec<&str> {
    let mut links = Vec::new();
    let mut remaining = text;
    while let Some(start) = remaining.find("](") {
        remaining = &remaining[start + 2..];
        let Some(end) = remaining.find(')') else {
            break;
        };
        links.push(remaining[..end].trim());
        remaining = &remaining[end + 1..];
    }
    links
}

fn github_heading_slug(heading: &str) -> String {
    heading
        .trim()
        .trim_end_matches('#')
        .trim()
        .chars()
        .filter_map(|character| {
            if character.is_alphanumeric() || character == '-' || character == '_' {
                Some(character.to_ascii_lowercase())
            } else if character.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

fn assert_markdown_anchor_exists(path: &Path, anchor: &str) {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    assert!(
        text.lines()
            .filter_map(|line| line.strip_prefix('#'))
            .any(|line| {
                let heading = line.trim_start_matches('#').trim();
                github_heading_slug(heading) == anchor
            }),
        "missing anchor `#{anchor}` in {}",
        path.display()
    );
}

#[test]
fn repository_text_normalizes_windows_line_endings() {
    assert_eq!(
        normalize_line_endings("first\r\nsecond\r\n"),
        "first\nsecond\n"
    );
}

#[test]
fn version_file_is_the_release_source_of_truth() {
    let version = repo_file("VERSION").trim().to_string();
    let manifest = repo_file("Cargo.toml");
    let release = repo_file(".github/workflows/release.yml");

    // Pin the documented YYYYMMDD.N.0 shape rather than one literal version, which
    // every release had to edit. This also catches the two forms RELEASE.md warns
    // about: the two-component `20260712.1`, and YYYYDDMM, which sorts wrongly.
    let (date, rest) = version.split_once('.').expect("version needs a date part");
    let (sequence, patch) = rest.split_once('.').expect("version must be YYYYMMDD.N.0");
    assert!(
        date.len() == 8 && date.chars().all(|c| c.is_ascii_digit()),
        "version must start with an 8-digit YYYYMMDD date, got {date:?}"
    );
    let month: u32 = date[4..6].parse().expect("month must be numeric");
    let day: u32 = date[6..8].parse().expect("day must be numeric");
    assert!(
        (1..=12).contains(&month) && (1..=31).contains(&day),
        "version date must be YYYYMMDD, got month {month} day {day} in {date:?}"
    );
    assert!(
        !sequence.is_empty()
            && sequence.chars().all(|c| c.is_ascii_digit())
            && !sequence.starts_with('0'),
        "release sequence must be a positive integer starting at 1, got {sequence:?}"
    );
    assert_eq!(patch, "0", "the third component is always 0 for SemVer");
    assert!(manifest.contains(&format!("version = \"{version}\"")));
    assert!(release.contains("BASE=$(cat VERSION)"));
    assert!(!release.contains("BASE=$(grep '^version' Cargo.toml"));
}

#[test]
fn release_docs_preserve_zero_drift_across_calendar_days() {
    let release = repo_file("docs/RELEASE.md");
    let updating = repo_file("docs/wiki/Updating.md");
    let readme_cn = repo_file("README_CN.md");

    for required in [
        "`YYYYMMDD` is the version-allocation date",
        "A stable promotion may happen on a later calendar date",
        "Do not bump or edit `VERSION`, `Cargo.toml`, or `docs/CHANGELOG.md` after acceptance",
    ] {
        assert!(
            release.contains(required),
            "release docs must preserve the cross-day zero-drift contract: `{required}`"
        );
    }
    assert!(
        updating.contains("version-allocation date"),
        "user update docs must not promise that a cross-day stable tag date is encoded"
    );
    assert!(
        readme_cn.contains("版本分配日期"),
        "the Chinese README must describe the calendar component as the allocation date"
    );
}

#[test]
fn stable_release_docs_use_full_branch_refspecs() {
    let release = repo_file("docs/RELEASE.md");

    assert!(release.contains("git push origin refs/heads/master:refs/heads/master"));
    assert!(
        !release.contains("git push origin master"),
        "stable release instructions must not contradict the full-refspec rule"
    );
}

#[test]
fn ci_covers_dev_and_all_supported_hosts() {
    let workflow = repo_file(".github/workflows/ci.yml");

    for required in [
        "workflow_call:",
        "push:",
        "pull_request:",
        "workflow_dispatch:",
        "dev",
        "master",
        "ubuntu-latest",
        "macos-latest",
        "windows-latest",
    ] {
        assert!(
            workflow.contains(required),
            "CI workflow must contain `{required}`"
        );
    }
}

#[test]
fn ci_runs_build_test_lint_format_audit_and_script_parsers() {
    let workflow = repo_file(".github/workflows/ci.yml");

    for command in [
        "cargo test --all --locked",
        "cargo clippy --all-targets --locked -- -D warnings",
        "cargo build --locked",
        "cargo fmt --check",
        "cargo audit",
        "bash -n scripts/install.sh",
    ] {
        assert!(
            workflow.contains(command),
            "CI workflow must execute `{command}`"
        );
    }
    assert!(workflow.contains("Parser]::ParseFile"));
    for script in ["scripts/install.ps1", "scripts/publish-dev.ps1"] {
        assert!(
            workflow.contains(script),
            "Windows CI must parse {script} with the PowerShell parser"
        );
    }
}

#[test]
fn ci_actions_are_pinned_to_full_commit_shas() {
    let workflow = repo_file(".github/workflows/ci.yml");

    for line in workflow
        .lines()
        .filter(|line| line.trim().starts_with("uses:"))
    {
        let reference = line
            .split_once('@')
            .map(|(_, reference)| reference.split_whitespace().next().unwrap_or(""))
            .unwrap_or("");
        assert!(
            reference.len() == 40 && reference.chars().all(|ch| ch.is_ascii_hexdigit()),
            "CI action must be pinned to a full commit SHA: {line}"
        );
    }
}

#[test]
fn self_update_provenance_requirement_is_documented() {
    let readme = repo_file("README.md");
    let readme_cn = repo_file("README_CN.md");
    let updating = repo_file("docs/wiki/Updating.md");
    let release = repo_file("docs/RELEASE.md");

    assert!(readme.contains("gh attestation verify"));
    assert!(readme_cn.contains("gh attestation verify"));
    assert!(updating.contains("codex-switch-global-pace-build-provenance.json"));
    assert!(updating.contains("gh attestation verify"));
    assert!(release.contains("codex-switch-global-pace-build-provenance.json"));
}

#[test]
fn documentation_links_resolve_to_reviewed_pages_and_sources() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let docs_dir = root.join("docs/wiki");
    let repository_prefix = "https://github.com/chriskooCK/codex-switch-global-pace/";

    for entry in fs::read_dir(&docs_dir).expect("failed to list documentation pages") {
        let path = entry.expect("failed to read documentation entry").path();
        if path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        for link in markdown_links(&text) {
            if let Some(target) = link.strip_prefix(repository_prefix) {
                let Some(target) = target
                    .strip_prefix("blob/dev/")
                    .or_else(|| target.strip_prefix("tree/dev/"))
                else {
                    if target.starts_with("blob/") || target.starts_with("tree/") {
                        panic!(
                            "{} links to an unreviewed repository branch: {link}",
                            path.display()
                        );
                    }
                    continue;
                };
                let (target_path, anchor) = target
                    .split_once('#')
                    .map_or((target, None), |(file, anchor)| (file, Some(anchor)));
                let local_path = root.join(target_path);
                assert!(
                    local_path.exists(),
                    "{} links to missing repository source: {link}",
                    path.display()
                );
                if let Some(anchor) = anchor {
                    assert_markdown_anchor_exists(&local_path, anchor);
                }
                continue;
            }
            if let Some(anchor) = link.strip_prefix('#') {
                assert_markdown_anchor_exists(&path, anchor);
                continue;
            }
            if link.contains("://") {
                continue;
            }

            let (target, anchor) = link
                .split_once('#')
                .map_or((link, None), |(target, anchor)| (target, Some(anchor)));
            assert!(
                target.ends_with(".md"),
                "{} uses an extensionless documentation link: {link}",
                path.display()
            );
            let target_path = path
                .parent()
                .expect("documentation page must have a parent directory")
                .join(target);
            assert!(
                target_path.exists(),
                "{} links to missing documentation page: {link}",
                path.display()
            );
            if let Some(anchor) = anchor {
                assert_markdown_anchor_exists(&target_path, anchor);
            }
        }
    }
}

#[test]
fn documentation_navigation_is_task_oriented_and_progressive() {
    let home = repo_file("docs/wiki/Home.md");

    for required in ["## Start here", "## Choose your task", "## Contribute"] {
        assert!(
            home.contains(required),
            "documentation Home must contain `{required}`"
        );
    }

    for page in [
        "Architecture-Overview.md",
        "Chinese-Guide.md",
        "Command-Reference.md",
        "Configuration.md",
        "Contributing.md",
        "Developer-Onboarding.md",
        "Development-Releases.md",
        "FAQ.md",
        "Feature-Guide.md",
        "Getting-Started.md",
        "Troubleshooting.md",
        "Updating.md",
    ] {
        assert!(
            repo_file(&format!("docs/wiki/{page}")).contains("## Next steps"),
            "documentation page {page} must end with explicit next steps"
        );
    }
}

#[test]
fn release_rejects_shell_metacharacters_in_tags_and_uses_env_in_scripts() {
    let workflow = repo_file(".github/workflows/release.yml");

    assert!(workflow.contains("TAG_PATTERN="));
    assert!(workflow.contains("BASE_PATTERN=\"${BASE//./\\\\.}\""));
    assert!(workflow.contains("[[ ! \"$TAG\" =~ $TAG_PATTERN ]]"));
    assert!(!workflow.contains("VERSION=\"${{ needs.meta.outputs.version }}\""));
    assert!(!workflow.contains("${{ github.ref }}` at ${{ github.sha }}"));
    assert!(workflow.contains("RELEASE_VERSION: ${{ needs.meta.outputs.version }}"));
    assert!(workflow.contains("persist-credentials: false"));
}

#[test]
fn release_reuses_the_exact_source_quality_gate_and_builds_locked() {
    let workflow = repo_file(".github/workflows/release.yml");

    for required in [
        "quality:\n    uses: ./.github/workflows/ci.yml",
        "needs: [quality, meta]",
        "cargo metadata --locked --no-deps --format-version 1",
        "cross build --release --locked --target",
        "cargo build --release --locked --target",
        "Cargo.lock > Cargo.lock.release",
        "sub(/\\r$/, \"\")",
    ] {
        assert!(
            workflow.contains(required),
            "release must preserve exact-source quality/lock gate `{required}`"
        );
    }
    assert_before(&workflow, "quality:", "build:");
    assert_before(&workflow, "cargo metadata --locked", "Build release binary");
}

#[test]
fn release_preserves_an_exact_dev_bundle_without_mutating_github_releases() {
    let workflow = repo_file(".github/workflows/release.yml");

    for required in [
        "concurrency:",
        "group: release-${{ github.ref }}",
        "cancel-in-progress: false",
        "Prepare release verifiers",
        "Verify exact local release asset set",
        "Release bundle must contain exactly the 16 expected assets.",
        "Confirm exact dev tag before artifact upload",
        "if: needs.meta.outputs.is_dev == 'true'",
        "Preserve verified dev release bundle",
        "name: dev-release-${{ github.sha }}",
        "retention-days: 7",
        "if-no-files-found: error",
        "path: dev-bundle/",
        "cp -- artifacts/* release_body.md dev-bundle/",
        "Development artifact must contain exactly the 16 release assets and release_body.md.",
    ] {
        assert!(
            workflow.contains(required),
            "dev bundle contract must contain `{required}`"
        );
    }
    assert_before(
        &workflow,
        "Verify exact local release asset set",
        "Confirm exact dev tag before artifact upload",
    );
    assert_before(
        &workflow,
        "Confirm exact dev tag before artifact upload",
        "Preserve verified dev release bundle",
    );
    let dev_path = workflow
        .split("- name: Confirm exact dev tag before artifact upload")
        .nth(1)
        .and_then(|tail| {
            tail.split("- name: Confirm release tag still targets this source before publish")
                .next()
        })
        .expect("isolated development bundle path");
    for forbidden in [
        "--method POST \"repos/${GITHUB_REPOSITORY}/releases\"",
        "--method PATCH",
        "--method DELETE",
        "gh release upload",
        "secrets.RELEASE_TOKEN",
    ] {
        assert!(
            !dev_path.contains(forbidden),
            "dev Actions path must not mutate GitHub Releases through `{forbidden}`"
        );
    }
    assert!(!workflow.contains("Delete existing dev release"));
    assert!(!workflow.contains("gh release delete dev"));
    assert!(!workflow.contains("dev-archive-"));
    assert!(!workflow.contains("dev-park-"));
    assert!(!workflow.contains("--clobber"));
}

#[test]
fn stable_release_stages_isolated_candidates_and_fails_closed_on_drift() {
    let workflow = repo_file(".github/workflows/release.yml");

    for required in [
        "if: needs.meta.outputs.is_dev != 'true'",
        "Confirm release tag still targets this source before publish",
        "Inspect an existing exact-tag release",
        "Create isolated candidate draft",
        "candidate_tag=\"release-candidate-${final_tag}\"",
        "Removed verified interrupted candidate release ${prior_release_id}.",
        "Existing candidate ${candidate_tag} does not exactly belong to this release; it was preserved.",
        "Candidate ref ${candidate_tag} exists without its verified draft; refusing to delete or reuse it.",
        "Upload and verify isolated candidate assets",
        "Confirm exact tag still targets this source before cutover",
        "Publish verified candidate on the exact tag",
        "Remove temporary cutover state after verified publication",
        "Remove only this run's incomplete candidate",
        "releases/tags/${tag}",
        "Existing stable release ${release_id} metadata differs from this exact source.",
        "verify-release-assets.sh",
        "existing-release-assets\" attest",
        "existing-release-candidate-ref-after-delete-error",
        "Only the fully verified final release",
        "gh attestation verify",
        "--bundle \"$provenance_bundle\"",
        "--signer-workflow \"$GITHUB_REPOSITORY/.github/workflows/release.yml\"",
        "--source-digest \"$GITHUB_SHA\"",
        "--source-ref \"$GITHUB_REF\"",
        "--deny-self-hosted-runners",
        "Existing checksum $(basename \"$checksum\") must contain exactly one line.",
        "[[ ! \"$recorded_digest\" =~ ^[0-9a-fA-F]{64}$",
        "actual_digest=$(sha256sum -- \"$archive\")",
        "externalParameters.workflow.path == \".github/workflows/release.yml\"",
        ".digest.gitCommit == $sha",
        "echo \"skip=true\" >> \"$GITHUB_OUTPUT\"",
        "'{tag_name:$tag,name:$name,draft:false,prerelease:$prerelease}'",
        "Release ${RELEASE_ID} no longer matches this run; refusing cleanup.",
        "prior-candidate-release-assets\" subset",
        "candidate-release-body.json",
        "pre-cutover-candidate-assets\" exact",
        "is already published on ${final_tag}; it was verified and preserved for rerun recovery.",
    ] {
        assert!(
            workflow.contains(required),
            "stable release transaction must contain `{required}`"
        );
    }
    assert_before(
        &workflow,
        "existing-release-assets\" attest",
        "echo \"skip=true\" >> \"$GITHUB_OUTPUT\"",
    );
    assert_before(
        &workflow,
        "gh attestation verify \"$archive\"",
        "tar xzf \"$download_dir/codex-switch-global-pace-linux-amd64.tar.gz\"",
    );
    assert_before(
        &workflow,
        "Create isolated candidate draft",
        "Upload and verify isolated candidate assets",
    );
    let verified_cleanup = workflow
        .split("- name: Remove temporary cutover state after verified publication")
        .nth(1)
        .and_then(|tail| {
            tail.split("- name: Remove only this run's incomplete candidate")
                .next()
        })
        .expect("verified stable candidate cleanup step");
    for required in [
        "needs.meta.outputs.is_dev != 'true'",
        "steps.publish.outputs.complete == 'true'",
        "cleanup-verified-release-assets",
        "Verified release %s remains published, but temporary state cleanup failed:",
    ] {
        assert!(
            verified_cleanup.contains(required),
            "verified cleanup contract must contain `{required}`"
        );
    }
    assert!(!verified_cleanup.contains("OLD_RELEASE_ID"));

    let incomplete_cleanup = workflow
        .split("- name: Remove only this run's incomplete candidate")
        .nth(1)
        .expect("incomplete candidate cleanup step");
    for required in [
        "incomplete-candidate-ref-error",
        "incomplete-candidate-ref-after-delete-error",
        "incomplete-candidate-release-error",
        "incomplete-candidate-release-after-delete-error",
        "elif ! grep -Eq 'HTTP 404|Not Found' \"$candidate_ref_error\"",
        "elif grep -Eq 'HTTP 404|Not Found' \"$candidate_release_error\"",
        "Candidate ref ${CANDIDATE_TAG} deletion state is ambiguous for release ${RELEASE_ID}",
        "Candidate release ${RELEASE_ID} (${CANDIDATE_TAG}) deletion state is ambiguous",
        "${target,,}",
        "if [[ \"$tag\" == \"$final_tag\" && \"$draft\" == false ]]",
        "if [[ \"$tag\" != \"$CANDIDATE_TAG\" || \"$draft\" != true ]]",
        "recovered-published-release-assets\" exact",
        "incomplete-candidate-assets\" subset",
        "pre-delete-candidate-assets\" subset",
    ] {
        assert!(
            incomplete_cleanup.contains(required),
            "incomplete cleanup contract must contain `{required}`"
        );
    }
    assert!(!incomplete_cleanup.contains("2>/dev/null"));
    assert_before(
        incomplete_cleanup,
        "Candidate ref ${CANDIDATE_TAG} for release ${RELEASE_ID} no longer belongs",
        "if ! gh api --method DELETE \\",
    );
    let release_cleanup = incomplete_cleanup
        .split("candidate_release_error=")
        .nth(1)
        .expect("incomplete candidate release cleanup branch");
    assert_before(
        release_cleanup,
        "Release ${RELEASE_ID} no longer matches this run; refusing cleanup.",
        "if ! gh api --method DELETE \\",
    );
    assert_before(
        incomplete_cleanup,
        "is already published on ${final_tag}; it was verified and preserved for rerun recovery.",
        "candidate_ref_error=",
    );
    assert_before(
        incomplete_cleanup,
        "if [[ \"$tag\" != \"$CANDIDATE_TAG\" || \"$draft\" != true ]]",
        "candidate_ref_error=",
    );
    let published_guard = incomplete_cleanup
        .split("if [[ \"$tag\" == \"$final_tag\" && \"$draft\" == false ]]")
        .nth(1)
        .and_then(|section| section.split("candidate_ref_error=").next())
        .expect("published stable recovery guard");
    assert!(published_guard.contains("exit 0"));
    assert!(
        !published_guard.contains("--method DELETE"),
        "an ambiguously completed stable publication must never be deleted"
    );
    assert!(!workflow.contains("Roll back an incomplete dev cutover"));
    assert!(!workflow.contains("release-${RELEASE_ID}.removed"));
    assert!(!workflow.contains("release-candidate-${GITHUB_RUN_ID}"));
    assert_before(
        &workflow,
        "git/refs/tags/${candidate_tag}",
        "releases/${prior_release_id}",
    );
}

#[test]
fn dev_publisher_verifies_one_exact_bundle_and_owns_every_remote_mutation() {
    let publisher = repo_file("scripts/publish-dev.ps1");
    let release_docs = repo_file("docs/RELEASE.md");

    for required in [
        "$Repo = 'chriskooCK/codex-switch-global-pace'",
        "[long]$RunId",
        "Expected exactly one successful Release run for refs/tags/dev",
        "Pass -RunId only when more than one exact run exists.",
        "dev-release-$sha",
        "ExactFiles $bundle @($Assets + 'release_body.md')",
        "RepoBytes 'VERSION' $sha",
        "RepoBytes 'Cargo.toml' $sha",
        "RepoBytes $spec[0] $sha",
        "--bundle",
        "--signer-workflow",
        "--source-digest",
        "--source-ref",
        "--deny-self-hosted-runners",
        "verificationResult.statement.subject",
        "Unsupported Windows host architecture",
        "$entries.Count -ne 1",
        "codex-switch-global-pace.exe') --version",
        "DownloadExact $candidateTag $remote $local",
        "Global\\codex-switch-global-pace-publish-dev-v1",
        "$PublisherMutex.WaitOne(0, $false)",
        "catch [System.Threading.AbandonedMutexException]",
        "Another publish-dev transaction is already running on this computer.",
        "$RemoteLockTag = 'codex-switch-publish-dev-lock'",
        "function AcquireRemotePublicationLock",
        "function AssertRemotePublicationLock",
        "function ReleaseRemotePublicationLock",
        "Create remote development-publication lock object",
        "Acquire remote development-publication lock",
        "the lock was not claimed and will not be removed automatically",
        "$RemoteLock = AcquireRemotePublicationLock $sha",
        "$RemoteLockOwned = $true",
        "$tx = [string]$RemoteLock.Transaction",
        "ReleaseRemotePublicationLock $RemoteLock",
        "The exact remote publication lock could not be released",
        "function DiscoverJournal",
        "dev-candidate-([1-9][0-9]*)-([0-9a-f]{64})-([0-9a-f]{32})",
        "dev-park-([1-9][0-9]*)-([1-9][0-9]*)-([0-9a-f]{64})-([0-9a-f]{32})",
        "Multiple development publication journals exist",
        "Park journal '$tag' does not identify its own release ID.",
        "function RecoverJournal",
        "CandidateProjection",
        "CandidateExact",
        "-Exact:([bool]$ExactAssets)",
        "DownloadProjection",
        "rollback-assets-",
        "exact local-bundle subset member",
        "Recovered interrupted development publication",
        "Prior dev release is not a mutable SHA-bound prerelease.",
        "Prior dev release drifted before candidate creation.",
        "function AssertCurrentPublicExact",
        "is already exact at $sha",
        "dev-candidate-$oldId-$oldFingerprint-$tx",
        "dev-park-$oldId-$($Context.CandidateId)-$oldFingerprint-$tx",
        "OldFingerprint",
        "codex-switch-old-release-v1;",
        "AppendFingerprintField",
        "function Rollback",
        "Rollback was not safe",
        "Refusing unsafe temporary cleanup",
        "Temporary publisher files were preserved",
        "function SafeWarning",
        "requires the locally authenticated gh user",
    ] {
        assert!(
            publisher.contains(required),
            "dev publisher contract must contain `{required}`"
        );
    }
    for forbidden in ["target/release", "--clobber", "secrets.RELEASE_TOKEN"] {
        assert!(
            !publisher.contains(forbidden),
            "dev publisher must not use `{forbidden}`"
        );
    }
    assert!(publisher.contains("$PSBoundParameters.ContainsKey('RunId') -and $RunId -le 0"));
    let remote_lock = publisher
        .split("function AcquireRemotePublicationLock")
        .nth(1)
        .and_then(|section| section.split("function Pages").next())
        .expect("remote development-publication lock functions");
    for required in [
        "repos/$script:Repo/git/tags",
        "refs/tags/$script:RemoteLockTag",
        "repos/$script:Repo/git/refs",
        "$refResult.Code -ne 0",
        "AssertRemotePublicationLock $lock",
        "repos/$script:Repo/git/refs/tags/$script:RemoteLockTag",
        "$after = Ref $script:RemoteLockTag",
    ] {
        assert!(
            remote_lock.contains(required),
            "remote publication lock must contain `{required}`"
        );
    }
    assert_before(remote_lock, "$refResult.Code -ne 0", "return $lock");
    assert_before(
        remote_lock,
        "AssertRemotePublicationLock $Lock",
        "'Release remote development-publication lock'",
    );
    assert_before(
        remote_lock,
        "'Release remote development-publication lock'",
        "$after = Ref $script:RemoteLockTag",
    );
    assert!(!publisher.contains("$C.Local $ExactAssets $C.CandidateProjection"));
    let fingerprint = publisher
        .split("function Fingerprint")
        .nth(1)
        .and_then(|section| section.split("function AssertState").next())
        .expect("canonical old-release fingerprint function");
    for required in [
        "target_commitish",
        "name",
        "body",
        "prerelease",
        "immutable",
        "content_type",
        "digest",
    ] {
        assert!(
            fingerprint.contains(required),
            "old-release fingerprint must include `{required}`"
        );
    }
    assert!(!fingerprint.contains("ConvertTo-Json"));
    let journal_discovery = publisher
        .split("function DiscoverJournal")
        .nth(1)
        .and_then(|section| section.split("function RecoverJournal").next())
        .expect("dev journal discovery function");
    for required in [
        "$candidate.OldId -ne $park.OldId",
        "$candidate.CandidateId -ne $park.CandidateId",
        "HasCandidateJournal",
        "HasParkJournal",
    ] {
        assert!(
            journal_discovery.contains(required),
            "journal pairing must contain `{required}`"
        );
    }
    let recovery = publisher
        .split("function RecoverJournal")
        .nth(1)
        .and_then(|section| section.split("function AssertCurrentPublicExact").next())
        .expect("dev journal recovery function");
    for required in [
        "$oldOriginal",
        "$oldParked",
        "if ($owned.Published)",
        "elseif (-not $Journal.HasParkJournal)",
        "AssertCandidate $context $candidateAgain.Value",
        "Rollback $context",
    ] {
        assert!(
            recovery.contains(required),
            "journal recovery must contain `{required}`"
        );
    }
    assert_before(recovery, "DownloadProjection", "Rollback $context");
    let rollback = publisher
        .split("function Rollback")
        .nth(1)
        .and_then(|section| section.split("$Context = $null").next())
        .expect("dev rollback function");
    assert_before(
        rollback,
        "$C.CandidateProjection = $owned.Assets",
        "DownloadProjection",
    );
    assert_before(rollback, "DownloadProjection", "RemoveRelease $owned.Id");
    let idempotent = publisher
        .split("function AssertCurrentPublicExact")
        .nth(1)
        .and_then(|section| section.split("function Rollback").next())
        .expect("exact-current verification function");
    assert_before(idempotent, "DownloadExact 'dev'", "ReleaseTag 'dev'");
    assert_before(&publisher, "ExactFiles $bundle", "Create candidate draft");
    assert_before(
        &publisher,
        "ExactFiles $bundle",
        "$journal = DiscoverJournal",
    );
    assert_before(
        &publisher,
        "$RemoteLock = AcquireRemotePublicationLock $sha",
        "$journal = DiscoverJournal",
    );
    assert_before(
        &publisher,
        "$RemoteLockOwned = $true",
        "$journal = DiscoverJournal",
    );
    assert_before(
        &publisher,
        "$RemoteLock = AcquireRemotePublicationLock $sha",
        "Create candidate draft",
    );
    assert_before(
        &publisher,
        "$journal = DiscoverJournal",
        "$oldByTag = ReleaseTag 'dev'",
    );
    assert_before(
        &publisher,
        "$Context.CandidateId = [long](Prop $candidate 'id')",
        "$parkTag = \"dev-park-$oldId-$($Context.CandidateId)-$oldFingerprint-$tx\"",
    );
    assert_before(
        &publisher,
        "DownloadExact $candidateTag $remote $local",
        "Park old dev release",
    );
    assert_before(&publisher, "Park old dev release", "Publish candidate");
    assert_before(&publisher, "Publish candidate", "RemoveRelease $oldId");
    assert_before(&publisher, "RemoveRelease $oldId", "$Published = $true");
    assert_before(
        &publisher,
        "Final dev release ID is not the published candidate.",
        "$Published = $true",
    );
    let cleanup = publisher
        .rsplit("finally {")
        .next()
        .expect("publisher temporary cleanup boundary");
    assert!(cleanup.contains("catch {"));
    assert!(cleanup.contains("SafeWarning"));
    assert!(cleanup.contains("$PublisherMutex.ReleaseMutex()"));
    assert!(cleanup.contains("$PublisherMutex.Dispose()"));
    assert!(cleanup.contains("ReleaseRemotePublicationLock $RemoteLock"));
    assert!(cleanup.contains("$LockCleanupFailure"));
    assert_before(
        cleanup,
        "Temporary publisher files were preserved",
        "$PublisherMutex.ReleaseMutex()",
    );
    assert!(release_docs.contains("pwsh -NoProfile -File ./scripts/publish-dev.ps1"));
}

#[test]
fn unix_installer_verifies_checksum_before_extracting() {
    let script = repo_file("scripts/install.sh");

    assert!(script.contains("${DOWNLOAD_URL}.sha256"));
    assert!(script.contains("EXPECTED_SHA256"));
    assert!(script.contains("sha256sum") && script.contains("shasum -a 256"));
    assert_before(&script, "EXPECTED_SHA256", "tar xzf");
    for required in [
        "USER_INSTALL_DIR=\"${HOME}/.local/bin\"",
        "SYSTEM_INSTALL_DIR=\"/usr/local/bin\"",
        "--system",
        "LEGACY_BIN",
        "install -m 0755",
        "stage_and_replace_binary",
        "rollback_installed_binary",
    ] {
        assert!(
            script.contains(required),
            "Unix installer must contain `{required}`"
        );
    }
}

#[test]
fn direct_installers_are_release_bound_and_preflight_exact_candidate_versions() {
    let unix = repo_file("scripts/install.sh");
    let windows = repo_file("scripts/install.ps1");
    let release = repo_file(".github/workflows/release.yml");
    let unix_install = unix
        .split("# Download, verify, and extract")
        .nth(1)
        .expect("Unix install transaction section");

    for required in [
        "PACKAGED_RELEASE_VERSION=\"\"",
        "EXPECTED_RELEASE_VERSION",
        "verify_candidate_version",
        "stage_and_replace_binary",
        "rollback_installed_binary",
        "commit_installed_binary",
    ] {
        assert!(
            unix.contains(required),
            "missing Unix contract `{required}`"
        );
    }
    assert_before(
        unix_install,
        "verify_candidate_version",
        "stage_and_replace_binary",
    );
    assert_before(
        unix_install,
        "stage_and_replace_binary",
        "commit_installed_binary",
    );
    assert_before(
        unix_install,
        "commit_installed_binary",
        "commit_held_legacy_install",
    );

    for required in [
        "$PackagedReleaseVersion = \"\"",
        "$ExpectedReleaseVersion",
        "$CandidateVersionLine -cne $ExpectedVersionLine",
    ] {
        assert!(
            windows.contains(required),
            "missing Windows contract `{required}`"
        );
    }
    assert!(release.contains("PACKAGED_RELEASE_VERSION=\\\"${RELEASE_VERSION}\\\""));
    assert!(release.contains("$PackagedReleaseVersion = \\\"${RELEASE_VERSION}\\\""));
}

#[test]
fn unix_installer_checks_homebrew_ownership_for_every_install_mode() {
    let script = repo_file("scripts/install.sh");
    let install = script
        .split("# ── Install")
        .nth(1)
        .expect("Unix install section");

    for required in [
        "classify_binary_ownership \"$INSTALL_DEST\"",
        "find_homebrew_managed_binary",
        "command -v \"$BINARY_NAME\"",
        "/opt/homebrew/bin/${BINARY_NAME}",
        "/home/linuxbrew/.linuxbrew/bin/${BINARY_NAME}",
    ] {
        assert!(
            script.contains(required),
            "missing ownership guard `{required}`"
        );
    }
    assert_before(install, "find_homebrew_managed_binary", "ASSET_NAME=");
}

#[test]
fn unix_installer_preserves_daemon_state_for_every_direct_upgrade() {
    let script = repo_file("scripts/install.sh");
    let install = script
        .split("# Download, verify, and extract")
        .nth(1)
        .expect("Unix install transaction section");
    for required in [
        "prepare_daemon_upgrade",
        "read_checked_daemon_status",
        "stop_and_confirm_daemon_absent",
        "restart_daemon_after_upgrade",
        "abort_install_upgrade",
        "ensure_previous_daemon_running",
        "preserve_install_backup",
    ] {
        assert!(
            script.contains(required),
            "missing service migration contract `{required}`"
        );
    }
    assert_before(
        install,
        "prepare_daemon_upgrade",
        "stage_and_replace_binary",
    );
    assert_before(
        install,
        "restart_daemon_after_upgrade",
        "commit_installed_binary",
    );
    assert_before(
        install,
        "hold_legacy_install_for_commit",
        "commit_installed_binary",
    );
    assert!(
        !script.contains("service_definition_references_binary"),
        "the shell must not parse launchd or systemd definitions"
    );
    for required in [
        "check_candidate_uninstall_owner \"$INSTALL_DEST\"",
        "check_candidate_uninstall_owner \"$LEGACY_BIN\"",
        "check_candidate_uninstall_owner \"$DAEMON_PREVIOUS_BIN\"",
        "--expected-existing-executable \"$LEGACY_BIN\"",
        "--expected-existing-executable \"$INSTALL_DEST\"",
    ] {
        assert!(
            script.contains(required),
            "missing exact Rust service-owner boundary `{required}`"
        );
    }
}

#[test]
fn unix_installer_accepts_only_the_candidate_exact_state_tuple() {
    let script = repo_file("scripts/install.sh");
    let parser = script
        .split("read_checked_daemon_status() {")
        .nth(1)
        .and_then(|section| section.split("stop_and_confirm_daemon_absent() {").next())
        .expect("Unix installer exact daemon-state parser");
    assert!(parser.contains("\"$CANDIDATE_BIN\" daemon status --installer-state 8>&- 9>&- 2>&1"));
    for exact in [
        "'running=true service_installed=true')",
        "'running=true service_installed=false')",
        "'running=false service_installed=true')",
        "'running=false service_installed=false')",
    ] {
        assert!(
            parser.contains(exact),
            "missing exact state tuple `{exact}`"
        );
    }
    assert!(!parser.contains("--json"));
    assert!(!parser.contains("*'\"running\":"));
}

#[test]
fn unix_installer_holds_the_shared_update_lock_across_the_transaction() {
    let script = repo_file("scripts/install.sh");
    let transaction = script
        .split("\nSYSTEM_MARKER_CREATED=false\nBINARY_REPLACED=false\n")
        .nth(1)
        .expect("Unix install transaction section");
    for required in [
        "CS_UPDATE_LOCK_TARGET=\"$target\"",
        "__hold-update-lock 8>&- 9>&-",
        "codex-switch-global-pace update lock ready",
        "mkfifo \"$control\" \"$ready\"",
        "start_install_update_locks",
        "release_update_locks",
    ] {
        assert!(
            script.contains(required),
            "missing update-lock contract `{required}`"
        );
    }
    assert!(
        !script.contains("read -r -t"),
        "a concurrent installer must wait for the shared lock instead of timing out"
    );
    assert_before(
        transaction,
        "start_install_update_locks",
        "validate_locked_direct_binary \"$INSTALL_DEST\"",
    );
    assert_before(
        transaction,
        "validate_locked_direct_binary \"$INSTALL_DEST\"",
        "MARKER_WAS_PRESENT=false",
    );
    assert_before(
        transaction,
        "MARKER_WAS_PRESENT=false",
        "prepare_daemon_upgrade",
    );
    assert!(
        transaction.rfind("commit_installed_binary").unwrap()
            < transaction.rfind("release_update_locks").unwrap(),
        "the success path must release the lock only after committing the executable"
    );
    assert!(
        transaction.rfind("cleanup_install_artifacts").unwrap()
            < transaction.rfind("release_update_locks").unwrap(),
        "the success path must clean transaction backups before releasing the lock"
    );
    assert!(
        transaction
            .rfind("Added ${USER_INSTALL_DIR} to PATH")
            .unwrap()
            < transaction.rfind("release_update_locks").unwrap(),
        "the install lock must cover the managed PATH mutation"
    );

    let multi_lock = script
        .split("start_install_update_locks() {")
        .nth(1)
        .and_then(|section| section.split("release_update_locks() {").next())
        .expect("Unix multi-target lock function");
    assert_before(
        multi_lock,
        "start_update_lock \"$candidate\" \"$LEGACY_BIN\" 8",
        "start_update_lock \"$candidate\" \"$INSTALL_DEST\" 9",
    );
}

#[test]
fn unix_installer_uses_fixed_fail_closed_transaction_residue() {
    let script = repo_file("scripts/install.sh");
    for required in [
        "INSTALL_STAGE_NAME=\".${BINARY_NAME}.install\"",
        "INSTALL_BACKUP_NAME=\".${BINARY_NAME}.rollback\"",
        "UNINSTALL_HOLD_NAME=\".${BINARY_NAME}.uninstall\"",
        "LEGACY_HOLD_NAME=\".${BINARY_NAME}.legacy\"",
        "assert_no_install_transaction_residue \"$INSTALL_DIR\"",
        "assert_no_install_transaction_residue \"$BIN_DIR\"",
        "fixed recovery path",
    ] {
        assert!(
            script.contains(required),
            "missing fixed residue contract `{required}`"
        );
    }
    for forbidden in [
        "mktemp \"${INSTALL_DIR}/.${BINARY_NAME}.install.",
        "mktemp \"${INSTALL_DIR}/.${BINARY_NAME}.backup.",
        "mktemp \"${SYSTEM_INSTALL_DIR}/.${BINARY_NAME}.legacy.",
    ] {
        assert!(
            !script.contains(forbidden),
            "transaction residue must not use an undiscoverable random path `{forbidden}`"
        );
    }
}

#[test]
fn unix_uninstaller_uses_the_shared_lock_and_refuses_an_unlocked_service_fallback() {
    let script = repo_file("scripts/install.sh");
    let uninstall = script
        .split("run_uninstall() {")
        .nth(1)
        .and_then(|section| section.split("# ── Install").next())
        .expect("Unix uninstall section");
    for required in [
        "start_update_lock \"$CANDIDATE_BIN\" \"$BIN_PATH\" 8",
        "check_candidate_uninstall_owner \"$BIN_PATH\"",
        "prepare_managed_path_removals",
        "capture_uninstall_daemon_state",
        "begin_uninstall_file_transaction",
        "commit_managed_path_removals",
        "hold_uninstall_binary_for_commit",
        "--expected-executable \"$BIN_PATH\" 8>&- 9>&-",
        "commit_uninstall_file_transaction",
        "UNINSTALL_SYSTEM_MARKER_PRESENT=true",
        "target parent ${BIN_DIR} does not exist",
        "release_update_locks",
    ] {
        assert!(
            uninstall.contains(required),
            "missing locked uninstall contract `{required}`"
        );
    }
    assert_before(
        uninstall,
        "start_update_lock",
        "classify_binary_ownership \"$BIN_PATH\"",
    );
    assert_before(
        uninstall,
        "classify_binary_ownership \"$BIN_PATH\"",
        "check_candidate_uninstall_owner \"$BIN_PATH\"",
    );
    assert_before(
        uninstall,
        "prepare_managed_path_removals",
        "capture_uninstall_daemon_state",
    );
    assert_before(
        uninstall,
        "commit_managed_path_removals",
        "hold_uninstall_binary_for_commit",
    );
    assert_before(
        uninstall,
        "No direct install, daemon service, PID state, marker, managed PATH block, or transaction residue was found; already uninstalled.",
        "target parent ${BIN_DIR} does not exist",
    );
    assert!(
        !uninstall.contains("systemctl --user daemon-reload || warn"),
        "manual systemd cleanup must not turn a failed reload into success"
    );
    assert!(
        uninstall.rfind("commit_managed_path_removals").unwrap()
            < uninstall.rfind("release_update_locks").unwrap(),
        "the successful uninstall must hold its lock through PATH cleanup"
    );
    let candidate_flow = script
        .split("CANDIDATE_BIN=\"")
        .nth(1)
        .expect("verified candidate execution flow");
    assert_before(
        candidate_flow,
        "verify_candidate_version \"$CANDIDATE_BIN\"",
        "\n  run_uninstall",
    );
    assert!(script.contains("This uninstaller is not bound to a GitHub Release"));
    assert!(script.contains("--expected-executable \"$1\" --check-owner"));
    assert!(script.contains("Kept shared update lock:"));
}

#[cfg(unix)]
#[test]
fn unix_legacy_migration_acquires_system_then_user_target_locks() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let start = script.find("cleanup_update_locks_on_exit() {").unwrap();
    let end = script[start..].find("prepare_daemon_upgrade() {").unwrap() + start;
    let helpers = &script[start..end];
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("lock-helper");
    let log = dir.path().join("lock-order");
    fs::write(
        &binary,
        r#"#!/bin/sh
[ "$1" = __hold-update-lock ] || exit 64
printf '%s\n' "$CS_UPDATE_LOCK_TARGET" >> "$LOCK_ORDER_LOG"
printf 'codex-switch-global-pace update lock ready\n'
cat >/dev/null
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&binary).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&binary, permissions).unwrap();
    let system_target = dir.path().join("system/codex-switch-global-pace");
    let user_target = dir.path().join("user/codex-switch-global-pace");
    fs::create_dir_all(system_target.parent().unwrap()).unwrap();
    fs::create_dir_all(user_target.parent().unwrap()).unwrap();
    let harness = format!(
        "set -eu\n{helpers}\nUPDATE_LOCK_PID_8=\nUPDATE_LOCK_PID_9=\nUPDATE_LOCK_ERROR=\nMIGRATE_LEGACY=true\nLEGACY_NEEDS_SUDO=false\nINSTALL_WITH_SUDO=false\nstart_install_update_locks \"$BIN\"\nrelease_update_locks\n"
    );
    let output = Command::new("bash")
        .args(["-c", &harness])
        .env("BIN", &binary)
        .env("TMP_DIR", dir.path())
        .env("LEGACY_BIN", &system_target)
        .env("INSTALL_DEST", &user_target)
        .env("LOCK_ORDER_LOG", &log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(log).unwrap(),
        format!("{}\n{}\n", system_target.display(), user_target.display())
    );
}

#[cfg(unix)]
#[test]
fn unix_uninstall_keeps_its_lock_holder_alive_through_daemon_and_binary_removal() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let home = tempfile::tempdir().unwrap();
    let install_dir = home.path().join(".local/bin");
    fs::create_dir_all(&install_dir).unwrap();
    let binary = install_dir.join("codex-switch-global-pace");
    fs::write(&binary, "#!/bin/sh\nexit 70\n").unwrap();
    let mut old_permissions = fs::metadata(&binary).unwrap().permissions();
    old_permissions.set_mode(0o755);
    fs::set_permissions(&binary, old_permissions).unwrap();

    let candidate = home.path().join("verified-candidate");
    let held = home.path().join("lock-held");
    let log = home.path().join("uninstall-log");
    fs::write(
        &candidate,
        r#"#!/bin/sh
case "$1" in
  __hold-update-lock)
    [ "$CS_UPDATE_LOCK_TARGET" = "$UNINSTALL_TARGET" ] || exit 60
    : > "$(dirname "$CS_UPDATE_LOCK_TARGET")/.codex-switch-global-pace.self-update.lock"
    : > "$LOCK_HELD"
    printf 'codex-switch-global-pace update lock ready\n'
    cat >/dev/null
    rm -f "$LOCK_HELD"
    ;;
  daemon)
    if [ "$2" = status ]; then
      [ "$3" = --installer-state ] || exit 61
      printf 'running=false service_installed=false\n'
      exit 0
    fi
    [ "$2" = uninstall ] || exit 62
    [ -f "$LOCK_HELD" ] || exit 63
    [ "$3" = --expected-executable ] || exit 64
    [ "$4" = "$UNINSTALL_TARGET" ] || exit 65
    if [ "${5:-}" != --check-owner ]; then
      [ ! -e "$UNINSTALL_TARGET" ] || exit 66
      printf 'daemon-uninstalled\n' > "$UNINSTALL_LOG"
    fi
    ;;
  *) exit 67 ;;
esac
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&candidate).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&candidate, permissions).unwrap();

    let harness = format!(
        "{}\nSYSTEM_INSTALL=false\nINSTALL_DIR=\"$USER_INSTALL_DIR\"\nINSTALL_DEST=\"$INSTALL_DIR/$BINARY_NAME\"\nOS=\"$TEST_OS\"\nTMP_DIR=\"$HOME/uninstall-tmp\"\nmkdir -p \"$TMP_DIR\"\nINSTALL_STAGE=\nINSTALL_BACKUP=\nINSTALL_WITH_SUDO=false\nUPDATE_LOCK_PID_8=\nUPDATE_LOCK_PID_9=\nUPDATE_LOCK_ERROR=\nCANDIDATE_BIN=\"$CANDIDATE\"\nrun_uninstall\n",
        unix_uninstall_harness(&script)
    );

    let output = Command::new("bash")
        .args(["-c", &harness])
        .env("HOME", home.path())
        .env("CANDIDATE", &candidate)
        .env("UNINSTALL_TARGET", &binary)
        .env("LOCK_HELD", &held)
        .env("UNINSTALL_LOG", &log)
        .env("TEST_OS", unix_installer_os())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!binary.exists());
    assert!(!held.exists());
    assert_eq!(fs::read_to_string(log).unwrap(), "daemon-uninstalled\n");
    assert!(
        install_dir
            .join(".codex-switch-global-pace.self-update.lock")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn unix_uninstall_restores_binary_and_path_when_service_commit_fails() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let home = tempfile::tempdir().unwrap();
    let install_dir = home.path().join(".local/bin");
    fs::create_dir_all(&install_dir).unwrap();
    let binary = install_dir.join("codex-switch-global-pace");
    let binary_contents = "#!/bin/sh\nexit 70\n";
    fs::write(&binary, binary_contents).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let profile = home.path().join(".profile");
    let profile_contents = concat!(
        "before\n",
        "# >>> codex-switch-global-pace PATH >>>\n",
        "export PATH=\"$HOME/.local/bin:$PATH\"\n",
        "# <<< codex-switch-global-pace PATH <<<\n",
        "after\n"
    );
    fs::write(&profile, profile_contents).unwrap();

    let candidate = home.path().join("verified-candidate");
    let attempted = home.path().join("service-attempted");
    fs::write(
        &candidate,
        r#"#!/bin/sh
case "$1" in
  __hold-update-lock)
    : > "$(dirname "$CS_UPDATE_LOCK_TARGET")/.codex-switch-global-pace.self-update.lock"
    printf 'codex-switch-global-pace update lock ready\n'
    cat >/dev/null
    ;;
  daemon)
    if [ "$2" = status ]; then
      [ "$3" = --installer-state ] || exit 71
      printf 'running=false service_installed=false\n'
      exit 0
    fi
    [ "$2" = uninstall ] || exit 72
    [ "$3" = --expected-executable ] || exit 73
    [ "$4" = "$UNINSTALL_TARGET" ] || exit 74
    if [ "${5:-}" != --check-owner ]; then
      [ ! -e "$UNINSTALL_TARGET" ] || exit 75
      : > "$SERVICE_ATTEMPTED"
      exit 76
    fi
    ;;
  *) exit 77 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
    let harness = format!(
        "{}\nSYSTEM_INSTALL=false\nINSTALL_DIR=\"$USER_INSTALL_DIR\"\nINSTALL_DEST=\"$INSTALL_DIR/$BINARY_NAME\"\nOS=\"$TEST_OS\"\nTMP_DIR=\"$HOME/uninstall-tmp\"\nmkdir -p \"$TMP_DIR\"\nINSTALL_STAGE=\nINSTALL_BACKUP=\nINSTALL_WITH_SUDO=false\nUPDATE_LOCK_PID_8=\nUPDATE_LOCK_PID_9=\nUPDATE_LOCK_ERROR=\nCANDIDATE_BIN=\"$CANDIDATE\"\nrun_uninstall\n",
        unix_uninstall_harness(&script)
    );
    let output = Command::new("bash")
        .args(["-c", &harness])
        .env("HOME", home.path())
        .env("CANDIDATE", &candidate)
        .env("UNINSTALL_TARGET", &binary)
        .env("SERVICE_ATTEMPTED", &attempted)
        .env("TEST_OS", unix_installer_os())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        attempted.exists(),
        "the final service boundary was not reached"
    );
    assert_eq!(fs::read_to_string(&binary).unwrap(), binary_contents);
    assert_eq!(fs::read_to_string(&profile).unwrap(), profile_contents);
    assert!(
        !install_dir
            .join(".codex-switch-global-pace.uninstall")
            .exists()
    );
    assert!(
        !home
            .path()
            .join(".profile.codex-switch-global-pace.install")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn unix_raw_repository_uninstaller_refuses_to_mutate_an_existing_install() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let home = tempfile::tempdir().unwrap();
    let install_dir = home.path().join(".local/bin");
    fs::create_dir_all(&install_dir).unwrap();
    let binary = install_dir.join("codex-switch-global-pace");
    fs::write(&binary, "old install without the hidden lock command\n").unwrap();
    let mut permissions = fs::metadata(&binary).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&binary, permissions).unwrap();

    let output = Command::new("bash")
        .arg(root.join("scripts/install.sh"))
        .arg("--uninstall")
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success());
    assert!(
        diagnostic.contains("not bound to a GitHub Release"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("Downloading:"), "{diagnostic}");
    assert_eq!(
        fs::read_to_string(binary).unwrap(),
        "old install without the hidden lock command\n"
    );
}

#[cfg(unix)]
#[test]
fn unix_installer_rejects_non_executable_direct_binaries_before_network_or_mutation() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let home = tempfile::tempdir().unwrap();
    let install_dir = home.path().join(".local/bin");
    fs::create_dir_all(&install_dir).unwrap();
    let binary = install_dir.join("codex-switch-global-pace");
    fs::write(&binary, "not executable\n").unwrap();
    let mut permissions = fs::metadata(&binary).unwrap().permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&binary, permissions).unwrap();

    let output = Command::new("bash")
        .arg(root.join("scripts/install.sh"))
        .env("HOME", home.path())
        .env("CS_VERSION", "1.2.3")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success());
    assert!(diagnostic.contains("is not executable"), "{diagnostic}");
    assert!(!diagnostic.contains("Downloading:"), "{diagnostic}");
    assert_eq!(fs::read_to_string(binary).unwrap(), "not executable\n");
}

#[cfg(unix)]
#[test]
fn unix_uninstall_true_noop_does_not_create_a_lock_parent() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let home = tempfile::tempdir().unwrap();
    let candidate = home.path().join("verified-candidate");
    fs::write(
        &candidate,
        r#"#!/bin/sh
case "$1 $2" in
  "daemon uninstall") [ "${5:-}" = --check-owner ] ;;
  "daemon status")
    [ "$3" = --installer-state ] || exit 60
    printf 'running=false service_installed=false\n'
    ;;
  *) exit 61 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
    let harness = format!(
        "{}\nSYSTEM_INSTALL=false\nINSTALL_DIR=\"$USER_INSTALL_DIR\"\nINSTALL_DEST=\"$INSTALL_DIR/$BINARY_NAME\"\nOS=\"$TEST_OS\"\nCANDIDATE_BIN=\"$CANDIDATE\"\nrun_uninstall\n",
        unix_uninstall_harness(&script)
    );
    let output = Command::new("bash")
        .args(["-c", &harness])
        .env("HOME", home.path())
        .env("CANDIDATE", &candidate)
        .env("TEST_OS", unix_installer_os())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("already uninstalled"));
    assert!(!home.path().join(".local/bin").exists());
}

#[cfg(unix)]
#[test]
fn unix_uninstall_preserves_a_service_when_the_lock_holder_binary_is_missing() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let home = tempfile::tempdir().unwrap();
    let target = home.path().join(".local/bin/codex-switch-global-pace");
    let service = if cfg!(target_os = "macos") {
        home.path()
            .join("Library/LaunchAgents/com.codex-switch-global-pace.daemon.plist")
    } else {
        home.path()
            .join(".config/systemd/user/codex-switch-global-pace-daemon.service")
    };
    fs::create_dir_all(service.parent().unwrap()).unwrap();
    let service_contents = if cfg!(target_os = "macos") {
        format!("<string>{}</string>\n", target.display())
    } else {
        format!(
            "ExecStart=\"{}\" daemon start --foreground\n",
            target.display()
        )
    };
    fs::write(&service, &service_contents).unwrap();
    let candidate = home.path().join("verified-candidate");
    fs::write(
        &candidate,
        r#"#!/bin/sh
case "$1 $2" in
  "daemon uninstall") [ "${5:-}" = --check-owner ] ;;
  "daemon status")
    [ "$3" = --installer-state ] || exit 60
    printf 'running=false service_installed=true\n'
    ;;
  *) exit 61 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
    let harness = format!(
        "{}\nSYSTEM_INSTALL=false\nINSTALL_DIR=\"$USER_INSTALL_DIR\"\nINSTALL_DEST=\"$INSTALL_DIR/$BINARY_NAME\"\nOS=\"$TEST_OS\"\nCANDIDATE_BIN=\"$CANDIDATE\"\nrun_uninstall\n",
        unix_uninstall_harness(&script)
    );

    let output = Command::new("bash")
        .args(["-c", &harness])
        .env("HOME", home.path())
        .env("CANDIDATE", &candidate)
        .env("TEST_OS", unix_installer_os())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success());
    assert!(diagnostic.contains("target parent"), "{diagnostic}");
    assert!(diagnostic.contains("does not exist"), "{diagnostic}");
    assert!(!target.parent().unwrap().exists());
    assert_eq!(fs::read_to_string(service).unwrap(), service_contents);
}

#[cfg(unix)]
#[test]
fn unix_uninstall_preserves_a_stale_path_block_without_a_lock_holder_binary() {
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let home = tempfile::tempdir().unwrap();
    let profile = home.path().join(".profile");
    let contents = concat!(
        "before\n",
        "# >>> codex-switch-global-pace PATH >>>\n",
        "export PATH=\"$HOME/.local/bin:$PATH\"\n",
        "# <<< codex-switch-global-pace PATH <<<\n",
        "after\n"
    );
    fs::write(&profile, contents).unwrap();
    let harness = format!(
        "{}\nSYSTEM_INSTALL=false\nINSTALL_DIR=\"$USER_INSTALL_DIR\"\nINSTALL_DEST=\"$INSTALL_DIR/$BINARY_NAME\"\nOS=\"$TEST_OS\"\nCANDIDATE_BIN=\"$HOME/missing-candidate\"\nrun_uninstall\n",
        unix_uninstall_harness(&script)
    );

    let output = Command::new("bash")
        .args(["-c", &harness])
        .env("HOME", home.path())
        .env("TEST_OS", unix_installer_os())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success());
    assert!(
        diagnostic.contains("No directory, lock residue"),
        "{diagnostic}"
    );
    assert!(!home.path().join(".local/bin").exists());
    assert_eq!(fs::read_to_string(profile).unwrap(), contents);
}

#[cfg(unix)]
#[test]
fn unix_release_candidate_cleans_stale_marker_and_path_when_the_lock_parent_exists() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let home = tempfile::tempdir().unwrap();
    let system_dir = home.path().join("system-bin");
    fs::create_dir_all(&system_dir).unwrap();
    let system_marker = system_dir.join(".codex-switch-global-pace-system-install-v1");
    fs::write(&system_marker, "").unwrap();
    let profile = home.path().join(".profile");
    fs::write(
        &profile,
        concat!(
            "before\n",
            "# >>> codex-switch-global-pace PATH >>>\n",
            "export PATH=\"$HOME/.local/bin:$PATH\"\n",
            "# <<< codex-switch-global-pace PATH <<<\n",
            "after\n"
        ),
    )
    .unwrap();
    let candidate = home.path().join("verified-candidate");
    fs::write(
        &candidate,
        r#"#!/bin/sh
case "$1" in
  __hold-update-lock)
    : > "$(dirname "$CS_UPDATE_LOCK_TARGET")/.codex-switch-global-pace.self-update.lock"
    printf 'codex-switch-global-pace update lock ready\n'
    cat >/dev/null
    ;;
  daemon)
    if [ "$2" = status ]; then
      [ "$3" = --installer-state ] || exit 63
      printf 'running=false service_installed=false\n'
    else
      [ "$2" = uninstall ]
    fi
    ;;
  *) exit 64 ;;
esac
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&candidate).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&candidate, permissions).unwrap();
    let harness = format!(
        "{}\nSYSTEM_INSTALL=false\nINSTALL_DIR=\"$USER_INSTALL_DIR\"\nINSTALL_DEST=\"$INSTALL_DIR/$BINARY_NAME\"\nSYSTEM_INSTALL_DIR=\"$TEST_SYSTEM_DIR\"\nLEGACY_BIN=\"$SYSTEM_INSTALL_DIR/$BINARY_NAME\"\nSYSTEM_INSTALL_MARKER=\"$SYSTEM_INSTALL_DIR/.codex-switch-global-pace-system-install-v1\"\nOS=\"$TEST_OS\"\nTMP_DIR=\"$HOME/uninstall-tmp\"\nmkdir -p \"$TMP_DIR\"\nINSTALL_STAGE=\nINSTALL_BACKUP=\nINSTALL_WITH_SUDO=false\nUPDATE_LOCK_PID_8=\nUPDATE_LOCK_PID_9=\nUPDATE_LOCK_ERROR=\nCANDIDATE_BIN=\"$CANDIDATE\"\nrun_uninstall\n",
        unix_uninstall_harness(&script)
    );
    let output = Command::new("bash")
        .args(["-c", &harness])
        .env("HOME", home.path())
        .env("CANDIDATE", &candidate)
        .env("TEST_SYSTEM_DIR", &system_dir)
        .env("TEST_OS", unix_installer_os())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(profile).unwrap(), "before\nafter\n");
    assert!(!system_marker.exists());
    assert!(
        system_dir
            .join(".codex-switch-global-pace.self-update.lock")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn unix_release_candidate_stops_a_detached_daemon_without_an_installed_binary() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let home = tempfile::tempdir().unwrap();
    let install_dir = home.path().join(".local/bin");
    fs::create_dir_all(&install_dir).unwrap();
    let data_dir = home.path().join(".codex-switch");
    fs::create_dir_all(&data_dir).unwrap();
    let pidfile = data_dir.join("daemon.pid");
    fs::write(&pidfile, "4242\n").unwrap();
    let state = home.path().join("daemon-state");
    fs::write(&state, "true").unwrap();
    let log = home.path().join("uninstall-log");

    let candidate = home.path().join("verified-candidate");
    fs::write(
        &candidate,
        r#"#!/bin/sh
case "$1 $2" in
  "__hold-update-lock ")
    : > "$(dirname "$CS_UPDATE_LOCK_TARGET")/.codex-switch-global-pace.self-update.lock"
    : > "$LOCK_HELD"
    printf 'codex-switch-global-pace update lock ready\n'
    cat >/dev/null
    rm -f "$LOCK_HELD"
    ;;
  "daemon status")
    [ "$3" = --installer-state ] || exit 60
    printf 'running=%s service_installed=false\n' "$(cat "$DAEMON_STATE")"
    ;;
  "daemon stop")
    [ -f "$LOCK_HELD" ] || exit 61
    printf false > "$DAEMON_STATE"
    rm -f "$DAEMON_PIDFILE"
    ;;
  "daemon uninstall")
    [ -f "$LOCK_HELD" ] || exit 62
    [ "$3" = --expected-executable ] || exit 63
    [ "${5:-}" = --check-owner ] || exit 64
    ;;
  *) exit 65 ;;
esac
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&candidate).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&candidate, permissions).unwrap();
    let harness = format!(
        "{}\nSYSTEM_INSTALL=false\nINSTALL_DIR=\"$USER_INSTALL_DIR\"\nINSTALL_DEST=\"$INSTALL_DIR/$BINARY_NAME\"\nOS=\"$TEST_OS\"\nTMP_DIR=\"$HOME/uninstall-tmp\"\nmkdir -p \"$TMP_DIR\"\nINSTALL_STAGE=\nINSTALL_BACKUP=\nINSTALL_WITH_SUDO=false\nUPDATE_LOCK_PID_8=\nUPDATE_LOCK_PID_9=\nUPDATE_LOCK_ERROR=\nCANDIDATE_BIN=\"$CANDIDATE\"\nrun_uninstall\n",
        unix_uninstall_harness(&script)
    );
    let held = home.path().join("lock-held");
    let output = Command::new("bash")
        .args(["-c", &harness])
        .env("HOME", home.path())
        .env("CANDIDATE", &candidate)
        .env("DAEMON_PIDFILE", &pidfile)
        .env("DAEMON_STATE", &state)
        .env("LOCK_HELD", &held)
        .env("UNINSTALL_LOG", &log)
        .env("TEST_OS", unix_installer_os())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(state).unwrap(), "false");
    assert!(!pidfile.exists());
    assert!(!held.exists());
    assert!(
        !log.exists(),
        "orphan recovery must stop the detached daemon without a later service mutation"
    );
    assert!(
        install_dir
            .join(".codex-switch-global-pace.self-update.lock")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn unix_daemon_upgrade_helpers_stop_and_restore_a_running_service() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let start = script.find("read_checked_daemon_status() {").unwrap();
    let end = script[start..]
        .find("verify_candidate_version() {")
        .unwrap()
        + start;
    let helpers = &script[start..end];
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("daemon-fixture");
    let state = dir.path().join("state");
    fs::write(&state, "true").unwrap();
    fs::write(
        &binary,
        r#"#!/bin/sh
case "$1 $2 $3" in
  "daemon status --installer-state") printf 'running=%s service_installed=true\n' "$(cat "$DAEMON_FIXTURE_STATE")" ;;
  "daemon stop ") printf false > "$DAEMON_FIXTURE_STATE" ;;
  "daemon start ") printf true > "$DAEMON_FIXTURE_STATE" ;;
  *) exit 64 ;;
esac
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&binary).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&binary, permissions).unwrap();
    let harness = format!(
        "set -eu\n{helpers}\nCANDIDATE_BIN=\"$BIN\"\nread_checked_daemon_status\n[ \"$DAEMON_STATUS_RUNNING\" = true ]\nstop_and_confirm_daemon_absent \"$BIN\"\n[ \"$DAEMON_STATUS_RUNNING\" = false ]\n\"$BIN\" daemon start 8>&- 9>&-\nconfirm_daemon_running\nprintf 'transaction-ok\\n'\n"
    );
    let output = Command::new("bash")
        .args(["-c", &harness])
        .env("BIN", &binary)
        .env("DAEMON_FIXTURE_STATE", &state)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "transaction-ok\n");
    assert_eq!(fs::read_to_string(state).unwrap(), "true");
}

#[test]
fn installers_validate_exact_versions_before_building_download_urls() {
    let unix = repo_file("scripts/install.sh");
    let windows = repo_file("scripts/install.ps1");

    for required in [
        "SEMVER_PATTERN=",
        "validate_version()",
        "validate_version \"$VERSION\"",
        "Invalid CS_VERSION",
    ] {
        assert!(
            unix.contains(required),
            "Unix installer must contain `{required}`"
        );
    }
    assert_before(
        &unix,
        "validate_version \"$VERSION\"",
        "releases/download/v${VERSION}/${ASSET_NAME}",
    );

    for required in [
        "$SemVerPattern =",
        "function Assert-SupportedVersion",
        "Assert-SupportedVersion $Version",
        "Invalid CS_VERSION",
    ] {
        assert!(
            windows.contains(required),
            "Windows installer must contain `{required}`"
        );
    }
    assert_before(
        &windows,
        "Assert-SupportedVersion $Version",
        "releases/download/v$Version/$AssetName",
    );
    assert!(
        windows.contains("$SemVerPattern = '\\A") && windows.contains("\\z'"),
        "PowerShell validation must anchor to the absolute start and end of the value"
    );
}

#[test]
fn unix_pinned_install_example_sets_the_variable_on_bash() {
    let script = repo_file("scripts/install.sh");

    assert!(
        script.contains("| CS_VERSION=20260712.1.0 bash"),
        "the pinned-install example must pass CS_VERSION to bash, not curl"
    );
    assert!(
        !script.contains("CS_VERSION=20260712.1.0 curl"),
        "the pinned-install example must not scope CS_VERSION to curl"
    );
}

#[cfg(unix)]
#[test]
fn unix_installer_rejects_a_repository_escape_version_before_network_access() {
    use std::process::Command;

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let home = tempfile::tempdir().unwrap();
    let output = Command::new("bash")
        .arg(root.join("scripts/install.sh"))
        .env("HOME", home.path())
        .env(
            "CS_VERSION",
            "/../../../../../attacker/evil/releases/download/v9.9.9",
        )
        .env_remove("CS_UNINSTALL")
        .output()
        .unwrap();
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!output.status.success());
    assert!(diagnostic.contains("Invalid CS_VERSION"), "{diagnostic}");
    assert!(!diagnostic.contains("Downloading:"), "{diagnostic}");
}

#[cfg(windows)]
#[test]
fn windows_installer_rejects_a_repository_escape_version_before_network_access() {
    use std::process::Command;

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-File"])
        .arg(root.join("scripts/install.ps1"))
        .env(
            "CS_VERSION",
            "/../../../../../attacker/evil/releases/download/v9.9.9",
        )
        .env_remove("CS_DEV")
        .env_remove("CS_UNINSTALL")
        .output()
        .unwrap();
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!output.status.success());
    assert!(diagnostic.contains("Invalid CS_VERSION"), "{diagnostic}");
    assert!(!diagnostic.contains("Downloading:"), "{diagnostic}");
}

#[test]
fn unix_installer_refuses_to_migrate_a_homebrew_cellar_symlink() {
    let script = repo_file("scripts/install.sh");

    for required in [
        "is_homebrew_cellar_path()",
        "classify_binary_ownership()",
        "BINARY_RESOLVED=\"$(resolve_path_target \"$candidate\")\"",
        "find_homebrew_managed_binary()",
        "Homebrew-managed install detected",
        "brew uninstall codex-switch-global-pace",
        "no Homebrew files were changed",
    ] {
        assert!(
            script.contains(required),
            "Unix installer must preserve Homebrew ownership guard `{required}`"
        );
    }
    assert_before(
        &script,
        "if [ \"$BINARY_KIND\" = \"homebrew\" ]; then",
        "MIGRATE_LEGACY=true",
    );

    let uninstall = script
        .split("# ── Uninstall")
        .nth(1)
        .and_then(|section| section.split("# ── Install").next())
        .expect("Unix installer must retain distinct uninstall/install sections");
    assert!(uninstall.contains("classify_binary_ownership"));
    assert!(uninstall.contains("the direct uninstaller did not change Homebrew files"));
    assert!(uninstall.contains("[ \"$BINARY_KIND\" = \"direct\" ]"));
    assert!(!uninstall.contains("DAEMON_BIN="));
    assert_before(
        uninstall,
        "if [ \"$BINARY_KIND\" = \"homebrew\" ]",
        "start_update_lock \"$CANDIDATE_BIN\" \"$BIN_PATH\" 8",
    );
}

#[cfg(unix)]
#[test]
fn unix_homebrew_classifier_recognizes_only_supported_cellar_roots() {
    use std::process::Command;

    fn section<'a>(script: &'a str, start: &str, end: &str) -> &'a str {
        let start_index = script
            .find(start)
            .unwrap_or_else(|| panic!("missing shell function `{start}`"));
        let tail = &script[start_index..];
        let end_index = tail
            .find(end)
            .unwrap_or_else(|| panic!("missing shell function boundary `{end}`"));
        &tail[..end_index]
    }

    let script = repo_file("scripts/install.sh");
    let cellar_matcher = section(
        &script,
        "is_homebrew_cellar_path() {",
        "classify_binary_ownership() {",
    );
    let harness = format!(
        "set -eu\n{cellar_matcher}\nfor root in /usr/local /opt/homebrew /home/linuxbrew/.linuxbrew; do is_homebrew_cellar_path \"$root/Cellar/codex-switch-global-pace/1/bin/codex-switch-global-pace\"; done\n! is_homebrew_cellar_path /tmp/Cellar/codex-switch-global-pace/1/bin/codex-switch-global-pace\nprintf 'ownership-ok\\n'\n"
    );

    let output = Command::new("bash")
        .args(["-c", &harness])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "stdout={stdout}\nstderr={stderr}");
    assert_eq!(stdout, "ownership-ok\n");
}

#[test]
fn unix_installer_preserves_migration_and_path_lifecycle() {
    let script = repo_file("scripts/install.sh");
    let install = script
        .split("# Install\n")
        .nth(1)
        .expect("Unix install execution section");

    for required in [
        "*/fish)",
        "PROFILE_FILE=\"${HOME}/.config/fish/config.fish\"",
        "# >>> codex-switch-global-pace PATH >>>",
        "# <<< codex-switch-global-pace PATH <<<",
        "prepare_managed_path_removals",
        "commit_managed_path_removals",
        "rollback_managed_path_removals",
        "${profile_target}.${BINARY_NAME}.install",
        "!seen_begin || !seen_end || inside",
    ] {
        assert!(
            script.contains(required),
            "Unix installer must contain `{required}`"
        );
    }

    let download_and_install = script
        .split("# Download, verify, and extract")
        .nth(1)
        .expect("Unix download and install section");
    assert_before(download_and_install, "tar xzf", "sudo -v");
    assert_before(
        install,
        "mkdir -p \"$INSTALL_DIR\"",
        "hold_legacy_install_for_commit",
    );
    assert!(
        script.contains(
            "if [ \"$SYSTEM_INSTALL\" = false ] && ! prepare_managed_path_removals; then"
        )
    );
}

#[test]
fn unix_installer_rewrites_shell_profiles_atomically() {
    let script = repo_file("scripts/install.sh");

    for required in [
        "prepare_path_block_removal() {",
        "commit_managed_path_removals() {",
        "rollback_managed_path_removals() {",
        "resolve_path_target() (",
        "file_identity() (",
        "while [ -L \"$profile_target\" ]",
        "link_target=\"$(readlink \"$profile_target\")\"",
        "cd -P \"$(dirname \"$profile_target\")\" && pwd -P",
        "profile_stage=\"${profile_target}.${BINARY_NAME}.install\"",
        "cp -p \"$profile_target\" \"$original\"",
        "current_target=\"$(resolve_path_target \"$logical\")\"",
        "current_identity=\"$(file_identity \"$current_target\")\"",
        "mv -f \"$stage\" \"$target\"",
    ] {
        assert!(
            script.contains(required),
            "Unix installer must preserve the atomic profile rewrite step `{required}`"
        );
    }
    assert!(
        !script.contains("cat \"$tmp_file\" > \"$profile_file\""),
        "Unix installer must not truncate a live shell profile in place"
    );
}

#[cfg(unix)]
#[test]
fn unix_installer_preserves_multi_level_profile_symlinks() {
    use std::os::unix::fs::symlink;
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let function_prefix = script
        .split("\nmanaged_path_block_exists() {")
        .next()
        .expect("installer must define managed_path_block_exists");
    let temp = tempfile::tempdir().unwrap();
    let real_profile = temp.path().join("real-profile");
    let middle_link = temp.path().join("middle-profile");
    let profile_link = temp.path().join(".zprofile");
    fs::write(
        &real_profile,
        "export KEEP=1\n# >>> codex-switch-global-pace PATH >>>\nexport PATH=/tmp/cs:$PATH\n# <<< codex-switch-global-pace PATH <<<\n",
    )
    .unwrap();
    symlink("real-profile", &middle_link).unwrap();
    symlink("middle-profile", &profile_link).unwrap();

    let harness = temp.path().join("remove-path-block.sh");
    fs::write(
        &harness,
        format!(
            "{function_prefix}\nTMP_DIR=\"$(mktemp -d)\"\ntrap 'rm -rf \"$TMP_DIR\"' EXIT\nreset_managed_path_transaction\nprepare_path_block_removal \"$1\"\ncommit_managed_path_removals\n"
        ),
    )
    .unwrap();
    let output = Command::new("bash")
        .arg(&harness)
        .arg(&profile_link)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "PATH transaction failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::symlink_metadata(&profile_link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::symlink_metadata(&middle_link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(&real_profile).unwrap(),
        "export KEEP=1\n"
    );
}

#[cfg(unix)]
#[test]
fn unix_installer_aborts_if_profile_symlink_changes_during_rewrite() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let function_prefix = script
        .split("\nmanaged_path_block_exists() {")
        .next()
        .expect("installer must define managed_path_block_exists");
    let temp = tempfile::tempdir().unwrap();
    let original_profile = temp.path().join("original-profile");
    let replacement_profile = temp.path().join("replacement-profile");
    let profile_link = temp.path().join(".zprofile");
    let managed = "# >>> codex-switch-global-pace PATH >>>\nexport PATH=/tmp/cs:$PATH\n# <<< codex-switch-global-pace PATH <<<\n";
    let original_contents = format!("export ORIGINAL=1\n{managed}");
    let replacement_contents = format!("export REPLACEMENT=1\n{managed}");
    fs::write(&original_profile, &original_contents).unwrap();
    fs::write(&replacement_profile, &replacement_contents).unwrap();
    symlink("original-profile", &profile_link).unwrap();

    let fake_bin = temp.path().join("fake-bin");
    fs::create_dir(&fake_bin).unwrap();
    let fake_cp = fake_bin.join("cp");
    fs::write(
        &fake_cp,
        "#!/bin/sh\nrm -f \"$PROFILE_LINK\"\nln -s \"$REPLACEMENT_PROFILE\" \"$PROFILE_LINK\"\nexec /bin/cp \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_cp, fs::Permissions::from_mode(0o755)).unwrap();

    let harness = temp.path().join("remove-path-block.sh");
    fs::write(
        &harness,
        format!(
            "{function_prefix}\nTMP_DIR=\"$(mktemp -d)\"\ntrap 'rm -rf \"$TMP_DIR\"' EXIT\nreset_managed_path_transaction\nprepare_path_block_removal \"$1\"\ncommit_managed_path_removals\n"
        ),
    )
    .unwrap();
    let output = Command::new("bash")
        .arg(&harness)
        .arg(&profile_link)
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .env("PROFILE_LINK", &profile_link)
        .env("REPLACEMENT_PROFILE", &replacement_profile)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "a changed profile symlink must abort the rewrite"
    );
    assert_eq!(
        fs::read_to_string(&original_profile).unwrap(),
        original_contents
    );
    assert_eq!(
        fs::read_to_string(&replacement_profile).unwrap(),
        replacement_contents
    );
}

#[cfg(unix)]
#[test]
fn unix_installer_aborts_if_profile_parent_symlink_changes() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let function_prefix = script
        .split("\nmanaged_path_block_exists() {")
        .next()
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let dir_a = temp.path().join("dir-a");
    let dir_b = temp.path().join("dir-b");
    let current = temp.path().join("current");
    fs::create_dir(&dir_a).unwrap();
    fs::create_dir(&dir_b).unwrap();
    let managed = "# >>> codex-switch-global-pace PATH >>>\nexport PATH=/tmp/cs:$PATH\n# <<< codex-switch-global-pace PATH <<<\n";
    let contents_a = format!("export A=1\n{managed}");
    let contents_b = format!("export B=1\n{managed}");
    fs::write(dir_a.join("profile"), &contents_a).unwrap();
    fs::write(dir_b.join("profile"), &contents_b).unwrap();
    symlink("dir-a", &current).unwrap();

    let fake_bin = temp.path().join("fake-bin");
    fs::create_dir(&fake_bin).unwrap();
    let fake_cp = fake_bin.join("cp");
    fs::write(
        &fake_cp,
        "#!/bin/sh\nrm -f \"$CURRENT_LINK\"\nln -s \"$NEW_DIR\" \"$CURRENT_LINK\"\nexec /bin/cp \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_cp, fs::Permissions::from_mode(0o755)).unwrap();
    let harness = temp.path().join("remove-path-block.sh");
    fs::write(
        &harness,
        format!(
            "{function_prefix}\nTMP_DIR=\"$(mktemp -d)\"\ntrap 'rm -rf \"$TMP_DIR\"' EXIT\nreset_managed_path_transaction\nprepare_path_block_removal \"$1\"\ncommit_managed_path_removals\n"
        ),
    )
    .unwrap();

    let output = Command::new("bash")
        .arg(&harness)
        .arg(current.join("profile"))
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .env("CURRENT_LINK", &current)
        .env("NEW_DIR", &dir_b)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(dir_a.join("profile")).unwrap(),
        contents_a
    );
    assert_eq!(
        fs::read_to_string(dir_b.join("profile")).unwrap(),
        contents_b
    );
}

#[cfg(unix)]
#[test]
fn unix_installer_aborts_if_profile_inode_changes() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let function_prefix = script
        .split("\nmanaged_path_block_exists() {")
        .next()
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let profile = temp.path().join("profile");
    let replacement = temp.path().join("replacement");
    let managed = "# >>> codex-switch-global-pace PATH >>>\nexport PATH=/tmp/cs:$PATH\n# <<< codex-switch-global-pace PATH <<<\n";
    fs::write(&profile, format!("export OLD=1\n{managed}")).unwrap();
    let replacement_contents = format!("export NEW=1\n{managed}");
    fs::write(&replacement, &replacement_contents).unwrap();

    let fake_bin = temp.path().join("fake-bin");
    fs::create_dir(&fake_bin).unwrap();
    let fake_cp = fake_bin.join("cp");
    fs::write(
        &fake_cp,
        "#!/bin/sh\nmv -f \"$REPLACEMENT_PROFILE\" \"$PROFILE_FILE\"\nexec /bin/cp \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_cp, fs::Permissions::from_mode(0o755)).unwrap();
    let harness = temp.path().join("remove-path-block.sh");
    fs::write(
        &harness,
        format!(
            "{function_prefix}\nTMP_DIR=\"$(mktemp -d)\"\ntrap 'rm -rf \"$TMP_DIR\"' EXIT\nreset_managed_path_transaction\nprepare_path_block_removal \"$1\"\ncommit_managed_path_removals\n"
        ),
    )
    .unwrap();

    let output = Command::new("bash")
        .arg(&harness)
        .arg(&profile)
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .env("PROFILE_FILE", &profile)
        .env("REPLACEMENT_PROFILE", &replacement)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(&profile).unwrap(), replacement_contents);
}

#[test]
fn release_build_installs_cross_with_locked_dependencies() {
    let workflow = repo_file(".github/workflows/release.yml");

    assert!(workflow.contains(
        "cargo install cross --locked --git https://github.com/cross-rs/cross --rev 64b5bb4d3d34de062552b9a2093affe77b4ad16a"
    ));
}

#[test]
fn unix_installer_records_and_cleans_explicit_system_install_intent() {
    let script = repo_file("scripts/install.sh");

    for required in [
        "SYSTEM_INSTALL_MARKER",
        ".codex-switch-global-pace-system-install-v1",
        "run_install_fs install -m 0644 /dev/null \"$SYSTEM_INSTALL_MARKER\"",
        "commit_held_legacy_install",
        "run_legacy_fs rm -f \"$SYSTEM_INSTALL_MARKER\"",
    ] {
        assert!(
            script.contains(required),
            "Unix installer must preserve system-install marker lifecycle: `{required}`"
        );
    }

    let abort = script
        .split("abort_install_upgrade() {")
        .nth(1)
        .and_then(|section| section.split("restart_daemon_after_upgrade() {").next())
        .expect("Unix install rollback function");
    let created_marker = abort
        .split("if [ \"${SYSTEM_MARKER_CREATED:-false}\" = true ]; then")
        .nth(1)
        .expect("new system-marker rollback branch");
    assert_before(
        created_marker,
        "if [ \"${BINARY_REPLACED:-false}\" = false ]; then",
        "run_install_fs rm -f \"$SYSTEM_INSTALL_MARKER\"",
    );
    assert!(created_marker.contains(
        "the new system-install marker was preserved because the replacement system binary remains installed"
    ));
}

#[test]
fn windows_installer_verifies_checksum_before_extracting() {
    let script = repo_file("scripts/install.ps1");

    assert!(script.contains("$ChecksumUrl"));
    assert!(script.contains("Get-DirectFileSha256"));
    assert!(script.contains("SHA256"));
    assert_before(
        &script,
        "$ActualSha256 = (Get-DirectFileSha256 -Path $ZipPath)",
        "Expand-Archive",
    );
    assert!(
        script.contains("Checksum mismatch"),
        "Windows installer must fail clearly on checksum mismatch"
    );
    assert!(script.contains("$env:LOCALAPPDATA"));
    assert!(script.contains("SetEnvironmentVariable(\"Path\", $NewPath, \"User\")"));
}

#[test]
fn windows_installer_rejects_reparse_paths_and_incomplete_transactions() {
    let script = repo_file("scripts/install.ps1");

    for required in [
        "function Get-DirectPathItem",
        "Get-Item -LiteralPath $Path -Force -ErrorAction Stop",
        "function Test-DirectInstallDirectory",
        "function Test-DirectInstalledBinary",
        "[System.IO.FileAttributes]::ReparsePoint",
        "function Assert-NoInstallTransactionResidue",
        "An incomplete previous installer transaction was found",
        "$LegacyTransactionPattern = '^\\.' + [regex]::Escape($Stem) + '\\.(install|rollback|failed)-[0-9A-Fa-f]{32}\\.exe$'",
        "$_.Name -cmatch $LegacyTransactionPattern",
        "Assert-NoInstallTransactionResidue -Path $InstallDir -Binary $BinaryName",
        "$DevVersionPattern = '\\A[0-9]+\\.[0-9]+\\.[0-9]+-dev",
        "$PackagedReleaseVersion -cmatch $DevVersionPattern",
        "$ExpectedReleaseVersion -cnotmatch $DevVersionPattern",
    ] {
        assert!(
            script.contains(required),
            "Windows installer must contain fail-closed path contract `{required}`"
        );
    }
    assert_eq!(
        script
            .matches("Assert-NoInstallTransactionResidue -Path $InstallDir -Binary $BinaryName")
            .count(),
        2,
        "install and uninstall must both reject a prior fixed transaction residue"
    );
    assert!(
        script
            .matches("if (-not (Test-DirectInstallDirectory -Path $InstallDir))")
            .count()
            >= 2,
        "install and uninstall must both revalidate their directory after acquiring the lock"
    );
    assert!(!script.contains("[Guid]::NewGuid"));
    assert!(!script.contains("Move-Item -LiteralPath $InstalledBin -Destination $BackupBin"));
    assert!(
        !script
            .contains("Remove-Item -LiteralPath $StagedBin -Force -ErrorAction SilentlyContinue")
    );
}

#[cfg(windows)]
#[test]
fn windows_installer_transaction_helpers_execute_fail_closed() {
    use std::process::Command;

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let temp = tempfile::tempdir().unwrap();
    let harness = temp.path().join("installer-transaction-harness.ps1");
    fs::write(
        &harness,
        r##"param(
    [Parameter(Mandatory = $true)][string]$InstallerPath,
    [Parameter(Mandatory = $true)][string]$FixtureDir
)

$ErrorActionPreference = "Stop"
Import-Module Microsoft.PowerShell.Utility -ErrorAction Stop
$Source = Get-Content -LiteralPath $InstallerPath -Raw -Encoding UTF8
$Marker = "# Detect architecture"
$Entrypoint = $Source.IndexOf($Marker, [System.StringComparison]::Ordinal)
if ($Entrypoint -lt 0) {
    throw "installer entrypoint marker was not found"
}
Invoke-Expression $Source.Substring(0, $Entrypoint)

function Write-FixtureFile {
    param([string]$Path, [string]$Value)
    [System.IO.File]::WriteAllBytes($Path, [System.Text.Encoding]::UTF8.GetBytes($Value))
}

$Installed = Join-Path $FixtureDir "installed.exe"
$Staged = Join-Path $FixtureDir "staged.exe"
$Backup = Join-Path $FixtureDir "rollback.exe"
$Failed = Join-Path $FixtureDir "failed.exe"
Write-FixtureFile $Installed "previous"
Write-FixtureFile $Staged "candidate"
$PreviousSha = Get-DirectFileSha256 -Path $Installed
$StagedSha = Get-DirectFileSha256 -Path $Staged

$Publication = Invoke-AtomicUpgradePublication `
    -StagedPath $Staged `
    -InstalledPath $Installed `
    -BackupPath $Backup `
    -FailedPath $Failed `
    -StagedSha256 $StagedSha `
    -PreviousSha256 $PreviousSha
if ($Publication.State -cne "Published" -or $null -ne $Publication.OperationError) {
    throw "exact publication did not succeed: $($Publication | ConvertTo-Json -Compress)"
}

$MissingFailed = Join-Path (Join-Path $FixtureDir "missing") "failed.exe"
$UnchangedRollback = Invoke-AtomicUpgradeRollback `
    -InstalledPath $Installed `
    -BackupPath $Backup `
    -FailedPath $MissingFailed `
    -StagedSha256 $StagedSha `
    -PreviousSha256 $PreviousSha
if ($UnchangedRollback.State -cne "Unchanged" -or $null -eq $UnchangedRollback.OperationError) {
    throw "failed rollback was not classified as unchanged: $($UnchangedRollback | ConvertTo-Json -Compress)"
}

$Rollback = Invoke-AtomicUpgradeRollback `
    -InstalledPath $Installed `
    -BackupPath $Backup `
    -FailedPath $Failed `
    -StagedSha256 $StagedSha `
    -PreviousSha256 $PreviousSha
if ($Rollback.State -cne "Restored" -or $null -ne $Rollback.OperationError) {
    throw "exact rollback did not succeed: $($Rollback | ConvertTo-Json -Compress)"
}
if ((Get-DirectFileSha256 -Path $Installed) -cne $PreviousSha -or
    (Get-DirectFileSha256 -Path $Failed) -cne $StagedSha) {
    throw "rollback bytes were not preserved exactly"
}

$UninstallInstalled = Join-Path $FixtureDir "uninstall-installed.exe"
$UninstallBackup = Join-Path $FixtureDir "uninstall-backup.exe"
Write-FixtureFile $UninstallInstalled "uninstall-original"
$UninstallSha = Get-DirectFileSha256 -Path $UninstallInstalled
$UninstallStaging = Invoke-AtomicUninstallStaging `
    -InstalledPath $UninstallInstalled `
    -BackupPath $UninstallBackup `
    -InstalledSha256 $UninstallSha
if ($UninstallStaging.State -cne "Staged" -or
    $null -ne (Get-DirectFileSha256 -Path $UninstallInstalled) -or
    (Get-DirectFileSha256 -Path $UninstallBackup) -cne $UninstallSha) {
    throw "uninstall staging did not preserve the exact bytes: $($UninstallStaging | ConvertTo-Json -Compress)"
}

$UninstallBackupHandle = [System.IO.File]::Open(
    $UninstallBackup,
    [System.IO.FileMode]::Open,
    [System.IO.FileAccess]::Read,
    [System.IO.FileShare]::Read
)
try {
    $BlockedUninstallCommit = Invoke-AtomicUninstallCommit `
        -InstalledPath $UninstallInstalled `
        -BackupPath $UninstallBackup `
        -InstalledSha256 $UninstallSha
    if ($BlockedUninstallCommit.State -cne "Unchanged" -or
        $null -eq $BlockedUninstallCommit.OperationError -or
        (Get-DirectFileSha256 -Path $UninstallBackup) -cne $UninstallSha) {
        throw "blocked uninstall commit did not remain exactly recoverable: $($BlockedUninstallCommit | ConvertTo-Json -Compress)"
    }
} finally {
    $UninstallBackupHandle.Dispose()
}

$UninstallRestore = Invoke-AtomicUninstallRestore `
    -InstalledPath $UninstallInstalled `
    -BackupPath $UninstallBackup `
    -InstalledSha256 $UninstallSha
if ($UninstallRestore.State -cne "Restored" -or
    (Get-DirectFileSha256 -Path $UninstallInstalled) -cne $UninstallSha -or
    $null -ne (Get-DirectFileSha256 -Path $UninstallBackup)) {
    throw "blocked uninstall commit could not be rolled back exactly: $($UninstallRestore | ConvertTo-Json -Compress)"
}

$CommittedUninstallStaging = Invoke-AtomicUninstallStaging `
    -InstalledPath $UninstallInstalled `
    -BackupPath $UninstallBackup `
    -InstalledSha256 $UninstallSha
$CommittedUninstall = Invoke-AtomicUninstallCommit `
    -InstalledPath $UninstallInstalled `
    -BackupPath $UninstallBackup `
    -InstalledSha256 $UninstallSha
if ($CommittedUninstallStaging.State -cne "Staged" -or
    $CommittedUninstall.State -cne "Committed" -or
    $null -ne $CommittedUninstall.OperationError -or
    $null -ne (Get-DirectFileSha256 -Path $UninstallInstalled) -or
    $null -ne (Get-DirectFileSha256 -Path $UninstallBackup)) {
    throw "uninstall commit did not remove only the verified staged bytes: $($CommittedUninstall | ConvertTo-Json -Compress)"
}

$UnchangedInstalled = Join-Path $FixtureDir "unchanged-installed.exe"
$UnchangedStaged = Join-Path $FixtureDir "unchanged-staged.exe"
Write-FixtureFile $UnchangedInstalled "old"
Write-FixtureFile $UnchangedStaged "new"
$Unchanged = Invoke-AtomicUpgradePublication `
    -StagedPath $UnchangedStaged `
    -InstalledPath $UnchangedInstalled `
    -BackupPath (Join-Path (Join-Path $FixtureDir "missing") "backup.exe") `
    -FailedPath (Join-Path $FixtureDir "unchanged-failed.exe") `
    -StagedSha256 (Get-DirectFileSha256 -Path $UnchangedStaged) `
    -PreviousSha256 (Get-DirectFileSha256 -Path $UnchangedInstalled)
if ($Unchanged.State -cne "Unchanged" -or $null -eq $Unchanged.OperationError) {
    throw "failed publication was not classified as unchanged: $($Unchanged | ConvertTo-Json -Compress)"
}

$AmbiguousInstalled = Join-Path $FixtureDir "ambiguous-installed.exe"
$AmbiguousStaged = Join-Path $FixtureDir "ambiguous-staged.exe"
$AmbiguousBackup = Join-Path $FixtureDir "ambiguous-backup.exe"
Write-FixtureFile $AmbiguousInstalled "old-ambiguous"
Write-FixtureFile $AmbiguousBackup "unexpected"
$Ambiguous = Invoke-AtomicUpgradePublication `
    -StagedPath $AmbiguousStaged `
    -InstalledPath $AmbiguousInstalled `
    -BackupPath $AmbiguousBackup `
    -FailedPath (Join-Path $FixtureDir "ambiguous-failed.exe") `
    -StagedSha256 "missing-candidate-sha" `
    -PreviousSha256 (Get-DirectFileSha256 -Path $AmbiguousInstalled)
if ($Ambiguous.State -cne "Ambiguous" -or $null -eq $Ambiguous.InspectionError) {
    throw "mixed publication state was not classified as ambiguous: $($Ambiguous | ConvertTo-Json -Compress)"
}

$LockedStage = Join-Path $FixtureDir "locked-stage.exe"
Write-FixtureFile $LockedStage "locked"
$LockedHandle = [System.IO.File]::Open(
    $LockedStage,
    [System.IO.FileMode]::Open,
    [System.IO.FileAccess]::Read,
    [System.IO.FileShare]::Read
)
try {
    $LockedCleanupError = Remove-StagedCandidate -Path $LockedStage
    if ($null -eq $LockedCleanupError -or $null -eq (Get-DirectFileSha256 -Path $LockedStage)) {
        throw "locked staged candidate cleanup did not preserve and report the residue"
    }
} finally {
    $LockedHandle.Dispose()
}
$UnlockedCleanupError = Remove-StagedCandidate -Path $LockedStage
if ($null -ne $UnlockedCleanupError -or $null -ne (Get-DirectFileSha256 -Path $LockedStage)) {
    throw "unlocked staged candidate cleanup did not remove the residue"
}

$ResidueDir = Join-Path $FixtureDir "residue"
[void][System.IO.Directory]::CreateDirectory($ResidueDir)
Write-FixtureFile (Join-Path $ResidueDir ".tool.rollback.exe") "fixed"
$FixedRejected = $false
try {
    Assert-NoInstallTransactionResidue -Path $ResidueDir -Binary "tool.exe"
} catch {
    $FixedRejected = $true
}
if (-not $FixedRejected) {
    throw "fixed transaction residue was accepted"
}
Remove-Item -LiteralPath (Join-Path $ResidueDir ".tool.rollback.exe") -Force
Write-FixtureFile (Join-Path $ResidueDir ".tool.uninstall.exe") "fixed-uninstall"
$UninstallResidueRejected = $false
try {
    Assert-NoInstallTransactionResidue -Path $ResidueDir -Binary "tool.exe"
} catch {
    $UninstallResidueRejected = $true
}
if (-not $UninstallResidueRejected) {
    throw "fixed uninstall transaction residue was accepted"
}
Remove-Item -LiteralPath (Join-Path $ResidueDir ".tool.uninstall.exe") -Force
Write-FixtureFile (Join-Path $ResidueDir ".tool.install-0123456789abcdef0123456789ABCDEF.exe") "legacy"
$LegacyRejected = $false
try {
    Assert-NoInstallTransactionResidue -Path $ResidueDir -Binary "tool.exe"
} catch {
    $LegacyRejected = $true
}
if (-not $LegacyRejected) {
    throw "legacy transaction residue was accepted"
}

$JunctionTarget = Join-Path $FixtureDir "junction-target"
$JunctionPath = Join-Path $FixtureDir "junction"
[void][System.IO.Directory]::CreateDirectory($JunctionTarget)
[void](New-Item -ItemType Junction -Path $JunctionPath -Target $JunctionTarget)
$JunctionRejected = $false
try {
    [void](Test-DirectInstallDirectory -Path $JunctionPath)
} catch {
    $JunctionRejected = $true
}
if (-not $JunctionRejected) {
    throw "install-directory junction was accepted"
}

if ("1.2.3-dev" -cnotmatch $DevVersionPattern -or
    "1.2.3-dev.4" -cnotmatch $DevVersionPattern -or
    "1.2.3-dev+build" -cnotmatch $DevVersionPattern -or
    "1.2.3-DEV" -cmatch $DevVersionPattern -or
    "1.2.3-alpha-dev" -cmatch $DevVersionPattern) {
    throw "development version classification is not case-sensitive and prefix-exact"
}
"installer transaction runtime checks passed"
"##,
    )
    .unwrap();

    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&harness)
        .args(["-InstallerPath"])
        .arg(root.join("scripts/install.ps1"))
        .args(["-FixtureDir"])
        .arg(temp.path())
        .output()
        .unwrap();
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(output.status.success(), "{diagnostic}");
    assert!(
        diagnostic.contains("installer transaction runtime checks passed"),
        "{diagnostic}"
    );
}

#[test]
fn windows_installer_preserves_a_running_daemon_across_upgrade() {
    let script = repo_file("scripts/install.ps1");
    let install_transaction = script
        .split("# Stage the verified candidate")
        .nth(1)
        .expect("Windows install transaction");

    for required in [
        "$DaemonWasRunning",
        "$DaemonServiceInstalled",
        "$StagedBin",
        "$BackupBin",
        "$FailedBin",
        "$OriginalUserPath",
        "$OldBinaryBackedUp",
        "$NewBinaryPublished",
        "$PathMutationAttempted",
        "$DaemonRestarted",
        "$DaemonRestartAttempted",
        "function Get-CheckedDaemonStatus",
        "daemon status --installer-state",
        "running=true service_installed=true",
        "running=false service_installed=false",
        "function Stop-And-ConfirmDaemonAbsent",
        "$After = Get-CheckedDaemonStatus -CandidatePath $CandidatePath",
        "$DaemonSafeForBinaryRollback",
        "automatic binary rollback was refused",
        ".$BinaryStem.install.exe",
        ".$BinaryStem.rollback.exe",
        ".$BinaryStem.failed.exe",
        "$AmbiguousBinaryState",
        "function Remove-StagedCandidate",
        "staged candidate remains preserved at",
        "function Invoke-AtomicUpgradePublication",
        "[System.IO.File]::Replace($StagedPath, $InstalledPath, $BackupPath, $true)",
        "$Publication = Invoke-AtomicUpgradePublication",
        "-FailedPath $FailedBin",
        "function Invoke-AtomicUpgradeRollback",
        "[System.IO.File]::Replace($BackupPath, $InstalledPath, $FailedPath, $true)",
        "$Rollback = Invoke-AtomicUpgradeRollback",
        "Stop-And-ConfirmDaemonAbsent -BinPath $InstalledBin -CandidatePath $CandidateBin",
        "& $InstalledBin daemon start",
        "& $CandidatePath daemon stop --expected-service-executable $BinPath",
        "& $CandidateBin daemon start --expected-executable $InstalledBin",
        "function Assert-CandidateServiceOwner",
        "The existing daemon could not be stopped safely",
        "$CandidateVersionOutput = & $CandidateBin --version",
        "$StagedVersionOutput = & $StagedBin --version",
        "the existing installation was not changed",
        "$RollbackErrors",
        "Restarting the previous daemon after rollback",
    ] {
        assert!(
            script.contains(required),
            "Windows installer must contain the daemon-upgrade safeguard `{required}`"
        );
    }
    assert_before(
        install_transaction,
        "$DaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin",
        "$Publication = Invoke-AtomicUpgradePublication",
    );
    assert_before(
        &script,
        "$CandidateVersionOutput = & $CandidateBin --version",
        "$StagedBin = Join-Path $InstallDir",
    );
    assert_before(
        install_transaction,
        "$StagedVersionOutput = & $StagedBin --version",
        "$DaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin",
    );
    assert_before(
        install_transaction,
        "$DaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin",
        "Stop-And-ConfirmDaemonAbsent -BinPath $InstalledBin -CandidatePath $CandidateBin",
    );
    assert_before(
        install_transaction,
        "Stop-And-ConfirmDaemonAbsent -BinPath $InstalledBin -CandidatePath $CandidateBin",
        "$Publication = Invoke-AtomicUpgradePublication",
    );
    assert!(
        script.contains("if ($DaemonWasRunning -or $DaemonServiceInstalled)"),
        "an installed but currently stopped task must still be ended before its executable is replaced"
    );

    let rollback_start = install_transaction
        .find("if ($null -ne $InstallError)")
        .expect("Windows installer must have an explicit rollback branch");
    let successful_transaction = &install_transaction[..rollback_start];
    assert_before(
        successful_transaction,
        "$InstalledVersionLine =",
        "Remove-Item -LiteralPath $BackupBin -Force",
    );
    assert_before(
        successful_transaction,
        "$DaemonRestarted = $true",
        "Remove-Item -LiteralPath $BackupBin -Force",
    );
    let rollback = &install_transaction[rollback_start..];
    assert_before(
        rollback,
        "Stop-And-ConfirmDaemonAbsent -BinPath $InstalledBin -CandidatePath $CandidateBin",
        "elseif ($NewBinaryPublished -and $OldBinaryBackedUp)",
    );
    assert!(rollback.contains(
        "$DaemonWasRunning -and $DaemonSafeForBinaryRollback -and $PreviousBinaryRestored"
    ));
    assert_before(
        rollback,
        "$Rollback = Invoke-AtomicUpgradeRollback",
        "Restarting the previous daemon after rollback",
    );
    assert_before(
        rollback,
        "SetEnvironmentVariable(\"Path\", $OriginalUserPath, \"User\")",
        "Restarting the previous daemon after rollback",
    );
}

#[test]
fn windows_installer_holds_the_shared_update_lock_for_the_whole_transaction() {
    let script = repo_file("scripts/install.ps1");
    let install_transaction = script
        .split("# Stage the verified candidate")
        .nth(1)
        .expect("Windows install transaction");

    for required in [
        "function Start-UpdateLockHolder",
        "$StartInfo.Arguments = \"__hold-update-lock\"",
        "$StartInfo.EnvironmentVariables[\"CS_UPDATE_LOCK_TARGET\"] = $DestinationPath",
        "$StartInfo.RedirectStandardInput = $true",
        "$StartInfo.RedirectStandardOutput = $true",
        "$StartInfo.RedirectStandardError = $true",
        "codex-switch-global-pace update lock ready",
        "does not support the required installer transaction lock",
        "[System.IO.Directory]::CreateDirectory($InstallDir)",
        "function Complete-UpdateLockHolder",
        "$LockProcess.StandardInput.Close()",
        "$LockProcess.WaitForExit(10000)",
        "$LockProcess.ExitCode -ne 0",
        "lock-holder PID $($LockProcess.Id) did not exit after stdin EOF",
        "$TransactionSucceeded = $true",
    ] {
        assert!(
            script.contains(required),
            "Windows installer must contain the shared-lock contract `{required}`"
        );
    }

    assert!(
        !script.contains("ArgumentList"),
        "PowerShell 5 compatibility must not add a destination quoting fallback"
    );
    assert_before(
        install_transaction,
        "$UpdateLockHolder = Start-UpdateLockHolder",
        "$OriginalUserPath = [Environment]::GetEnvironmentVariable(\"Path\", \"User\")",
    );
    assert_before(
        install_transaction,
        "$UpdateLockHolder = Start-UpdateLockHolder",
        "$DaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin",
    );
    assert_before(
        install_transaction,
        "$DaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin",
        "$Publication = Invoke-AtomicUpgradePublication",
    );
    assert_before(
        install_transaction,
        "if ($null -ne $InstallError)",
        "Complete-UpdateLockHolder -LockProcess $UpdateLockHolder",
    );
    assert_before(
        install_transaction,
        "Remove-Item -LiteralPath $BackupBin -Force",
        "Complete-UpdateLockHolder -LockProcess $UpdateLockHolder",
    );
    let transaction_finally = install_transaction
        .find("} finally {\n    $LockReleaseError = $null")
        .expect("Windows installer must release its lock from the transaction finally block");
    assert!(
        install_transaction[transaction_finally..]
            .contains("Complete-UpdateLockHolder -LockProcess $UpdateLockHolder")
    );
}

#[test]
fn windows_uninstaller_uses_the_verified_candidate_and_shared_update_lock() {
    let script = repo_file("scripts/install.ps1");
    let uninstall = script
        .split("# ── Uninstall")
        .nth(1)
        .and_then(|section| section.split("# Stage the verified candidate").next())
        .expect("Windows uninstall transaction");

    for required in [
        "function Invoke-AtomicUninstallStaging",
        "[System.IO.File]::Move($InstalledPath, $BackupPath)",
        "function Invoke-AtomicUninstallRestore",
        "[System.IO.File]::Move($BackupPath, $InstalledPath)",
        "function Invoke-AtomicUninstallCommit",
        "[System.IO.File]::Delete($BackupPath)",
        "function Restore-UninstallRunningState",
        ".$Stem.uninstall.exe",
    ] {
        assert!(
            script.contains(required),
            "Windows uninstaller must contain transaction helper `{required}`"
        );
    }

    for required in [
        "-CandidatePath $CandidateBin",
        "-DestinationPath $InstalledBin",
        "$UninstallBackupBin",
        "$UninstallIsNoOp",
        "codex-switch-global-pace is already uninstalled",
        "$OriginalBinarySha256",
        "$OriginalUserPath",
        "$PathMutationAttempted",
        "$DaemonStopAttempted",
        "$UninstallCommitted",
        "$PostCommitCleanupError",
        "Assert-CandidateServiceOwner",
        "& $CandidateBin daemon uninstall --expected-executable $InstalledBin",
        "& $CandidateBin daemon stop",
        "Invoke-AtomicUninstallStaging",
        "Invoke-AtomicUninstallCommit",
        "Invoke-AtomicUninstallRestore",
        "Restore-UninstallRunningState",
        "The uninstall did not commit, and the exact pre-uninstall binary, PATH, and running state were restored",
        "Uninstall committed, but post-commit cleanup could not be confirmed",
        "Recovery residue path: $UninstallBackupBin",
        "SetEnvironmentVariable(\"Path\", $RequestedUserPath, \"User\")",
        "SetEnvironmentVariable(\"Path\", $OriginalUserPath, \"User\")",
        "Complete-UpdateLockHolder -LockProcess $UninstallLockHolder",
    ] {
        assert!(
            uninstall.contains(required),
            "Windows uninstaller must contain locked candidate contract `{required}`"
        );
    }
    assert_before(
        uninstall,
        "if ($UninstallIsNoOp)",
        "[void][System.IO.Directory]::CreateDirectory($InstallDir)",
    );
    assert_before(
        uninstall,
        "$UninstallLockHolder = Start-UpdateLockHolder",
        "$OriginalBinarySha256 = if ($InstalledBinaryWasPresent)",
    );
    assert_before(
        uninstall,
        "$UninstallLockHolder = Start-UpdateLockHolder",
        "Assert-CandidateServiceOwner `",
    );
    assert_before(
        uninstall,
        "Assert-CandidateServiceOwner `",
        "$DaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin",
    );
    assert_before(
        uninstall,
        "Stop-And-ConfirmDaemonAbsent -BinPath $InstalledBin -CandidatePath $CandidateBin",
        "SetEnvironmentVariable(\"Path\", $RequestedUserPath, \"User\")",
    );
    assert_before(
        uninstall,
        "SetEnvironmentVariable(\"Path\", $RequestedUserPath, \"User\")",
        "$Staging = Invoke-AtomicUninstallStaging",
    );
    assert_before(
        uninstall,
        "$Staging = Invoke-AtomicUninstallStaging",
        "$DaemonCleanupOutput = (& $CandidateBin daemon uninstall --expected-executable $InstalledBin 2>&1",
    );
    assert_before(
        uninstall,
        "$DaemonCleanupOutput = (& $CandidateBin daemon uninstall --expected-executable $InstalledBin 2>&1",
        "$CommittedDaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin",
    );
    assert_before(
        uninstall,
        "$CommittedDaemonStatus = Get-CheckedDaemonStatus -CandidatePath $CandidateBin",
        "$UninstallCommitted = $true",
    );
    assert_before(
        uninstall,
        "$UninstallCommitted = $true",
        "$Commit = Invoke-AtomicUninstallCommit",
    );
    assert_before(
        uninstall,
        "$Commit = Invoke-AtomicUninstallCommit",
        "Complete-UpdateLockHolder -LockProcess $UninstallLockHolder",
    );
    let rollback = uninstall
        .split("} catch {\n        $UninstallFailure = $_")
        .nth(1)
        .expect("Windows uninstall rollback branch");
    assert_before(
        rollback,
        "$Restore = Invoke-AtomicUninstallRestore",
        "SetEnvironmentVariable(\"Path\", $OriginalUserPath, \"User\")",
    );
    assert_before(
        rollback,
        "SetEnvironmentVariable(\"Path\", $OriginalUserPath, \"User\")",
        "Restore-UninstallRunningState",
    );
    assert!(!uninstall.contains("Remove-Item -LiteralPath $InstalledBin -Force"));
    assert!(!uninstall.contains("daemon install"));
    assert!(!uninstall.contains("Get-ScheduledTask"));
    assert!(!uninstall.contains("schtasks.exe"));
}

#[test]
fn self_update_checks_replace_permission_before_archive_download() {
    let update = repo_file("src/update.rs");

    assert_before(
        &update,
        "ensure_replace_parent_writable(&executable, platform, &release.tag_name)?",
        "download_file(&client, &archive_asset.browser_download_url",
    );
    assert!(!update.contains("permission denied? try: sudo codex-switch-global-pace self-update"));
    assert!(!update.contains("retry from PowerShell as Administrator"));
}

#[test]
fn self_update_attestation_is_bound_to_the_current_tag_commit() {
    let update = repo_file("src/update.rs");

    assert!(update.contains("\"--source-digest\""));
    assert!(update.contains("fetch_tag_commit_sha(&client, &release.tag_name).await?"));
    assert!(update.contains("if confirmed_digest != source_digest"));
    assert_before(
        &update,
        "verify_build_provenance(",
        "if confirmed_digest != source_digest",
    );
    assert_before(
        &update,
        "verify_candidate_binary(&extracted_path",
        "let confirmed_digest = fetch_tag_commit_sha",
    );
    assert_before(
        &update,
        "if confirmed_digest != source_digest",
        "replace_candidate(",
    );
}

#[test]
fn daemon_service_installations_stage_validate_and_rollback() {
    let service = repo_file("src/daemon/service.rs");
    for required in [
        "staged_service_file",
        "plutil",
        "systemd-analyze",
        "rollback_systemd_install",
        "remove enablement for failed new systemd service",
        "export existing scheduled task",
        "codex-switch-global-pace-daemon-install-",
        "restore_scheduled_task",
        "wait_for_scheduled_daemon",
        "cmd.exe /D /V:OFF /S /C",
        "validate_uninstall_owner",
        "rollback_launchd_uninstall",
        "rollback_systemd_uninstall",
        "rollback_task_scheduler_uninstall",
        "acquire_service_operation_lease",
        "definition_snapshot_matches",
        "Global\\\\codex-switch-global-pace-daemon-service-operation-v1",
        "task_listing_contains_name",
        "&[\"/Query\", \"/FO\", \"CSV\", \"/NH\"]",
        "optional_scheduled_task_xml",
    ] {
        assert!(
            service.contains(required),
            "missing service transaction contract `{required}`"
        );
    }
    let lease = service
        .split("pub(crate) fn acquire_service_operation_lease()")
        .nth(1)
        .and_then(|section| section.split("pub fn install(").next())
        .expect("service operation lease implementation");
    assert!(
        !lease.contains("effective_app_home"),
        "the fixed service identity must not use a CODEX_SWITCH_HOME-scoped operation lease"
    );
    let launchd_install = service
        .split("fn install_launchd(expected_existing_executable: Option<&Path>)")
        .nth(1)
        .and_then(|section| section.split("fn start_launchd()").next())
        .expect("LaunchAgent install implementation");
    assert_before(
        launchd_install,
        "generated LaunchAgent failed plutil validation",
        "was_loaded",
    );
    assert!(
        launchd_install.contains("validate_launchd_definition_owner(")
            && launchd_install
                .matches("require_service_file_snapshot(")
                .count()
                >= 2,
        "LaunchAgent install must prove owner and revalidate its exact snapshot before replacement"
    );
    let systemd_install = service
        .split("fn install_systemd(expected_existing_executable: Option<&Path>)")
        .nth(1)
        .and_then(|section| section.split("fn start_systemd()").next())
        .expect("systemd install implementation");
    assert_before(
        systemd_install,
        "generated systemd user service failed validation",
        "was_active",
    );
    assert!(
        systemd_install.contains("validate_systemd_definition_owner(")
            && systemd_install
                .matches("require_service_file_snapshot(")
                .count()
                >= 2,
        "systemd install must prove owner and revalidate its exact snapshot before replacement"
    );
    let task_install = service
        .split("fn install_task_scheduler(expected_existing_executable: Option<&Path>)")
        .nth(1)
        .and_then(|section| section.split("fn create_scheduled_task(").next())
        .expect("Task Scheduler install implementation");
    assert_before(
        task_install,
        "create_scheduled_task(&stage_name",
        "stop_scheduled_daemon_for_rollback().context(",
    );
    assert!(
        task_install.contains("validate_task_scheduler_definition_owner(")
            && task_install.contains("require_task_definition_snapshot("),
        "Task Scheduler install must prove owner and guard its exact definition snapshot"
    );
    assert!(service.contains("reload systemd user units after uninstall"));
    assert!(service.contains("systemd service uninstall failed and rollback was incomplete"));
    assert!(
        !service.contains("path.exists()"),
        "service lifecycle must not collapse metadata errors into a missing definition"
    );
    assert_before(
        &service,
        "std::fs::remove_file(&path)",
        "reload systemd user units after uninstall",
    );
    let systemd_uninstall = service
        .split("fn uninstall_systemd(expected_executable: &Path)")
        .nth(1)
        .and_then(|section| section.split("// -- Windows Task Scheduler --").next())
        .expect("systemd uninstall implementation");
    assert!(systemd_uninstall.contains("let Some(previous) = optional_file_contents(&path)? else"));
    assert!(
        !systemd_uninstall.contains("path.exists()"),
        "systemd uninstall must not collapse metadata errors into a missing service"
    );
}

#[test]
fn installer_only_daemon_checks_run_before_normal_initialization() {
    let main = repo_file("src/main.rs");
    let cli = repo_file("src/cli.rs");
    assert_before(
        &main,
        "if let Some(expected_executable) = installer_owner_check_request(&cli.command)",
        "let use_json = cli.json || cli.json_pretty",
    );
    assert_before(
        &main,
        "Some(Commands::Daemon(cli::DaemonCommand::Status",
        "let use_json = cli.json || cli.json_pretty",
    );
    assert!(cli.contains("expected_existing_executable"));
    assert!(cli.contains("installer_state"));
}

#[test]
fn ci_pins_the_audit_executable_version() {
    let workflow = repo_file(".github/workflows/ci.yml");
    assert!(workflow.contains("cargo install cargo-audit --version 0.22.2 --locked"));
}

#[test]
fn self_update_gates_markerless_system_installs_before_network_checks() {
    let command = repo_file("src/commands/update.rs");

    assert_eq!(
        command
            .matches("ensure_system_install_migrated(use_dev, version, json)?;")
            .count(),
        2,
        "self-update must preflight the ownership marker and revalidate it under the update lease"
    );
    assert_before(
        &command,
        "ensure_system_install_migrated(use_dev, version, json)?;",
        "if check",
    );
    let locked = command
        .split("let update_lease = update::acquire_self_update_lease()")
        .nth(1)
        .expect("self-update lease acquisition");
    assert_before(
        locked,
        "ensure_system_install_migrated(use_dev, version, json)?;",
        "SelfUpdateDaemonRestart::capture()",
    );
}

#[test]
fn distribution_targets_only_the_independent_repository() {
    let workflow = repo_file(".github/workflows/release.yml");
    let unix = repo_file("scripts/install.sh");
    let windows = repo_file("scripts/install.ps1");

    for text in [&workflow, &unix, &windows] {
        assert!(text.contains("chriskooCK/codex-switch-global-pace"));
        assert!(!text.contains("xjoker/codex-switch"));
    }
    assert!(!workflow.contains("legacy-upgrade:"));
    assert!(!workflow.contains("homebrew:"));
    assert!(!workflow.contains("xjoker/homebrew-tap"));
}

#[test]
fn uninstallers_always_preserve_the_shared_profile_directory() {
    let unix = repo_file("scripts/install.sh");
    let windows = repo_file("scripts/install.ps1");

    assert!(unix.contains("DATA_DIR=\"${HOME}/.codex-switch\""));
    assert!(windows.contains("$DataDir = Join-Path $env:USERPROFILE \".codex-switch\""));
    assert!(!unix.contains("rm -rf \"$DATA_DIR\""));
    assert!(!windows.contains("Remove-Item -Recurse -Force $DataDir"));
    assert!(unix.contains("Kept shared profile data"));
    assert!(windows.contains("Kept shared profile data"));
}

#[test]
fn release_verifies_archives_before_creating_a_release() {
    let workflow = repo_file(".github/workflows/release.yml");

    assert!(workflow.contains("permissions:\n  contents: read"));
    assert!(workflow.contains("release:\n") && workflow.contains("contents: write"));
    for archive in [
        "codex-switch-global-pace-linux-amd64.tar.gz",
        "codex-switch-global-pace-linux-arm64.tar.gz",
        "codex-switch-global-pace-darwin-amd64.tar.gz",
        "codex-switch-global-pace-darwin-arm64.tar.gz",
        "codex-switch-global-pace-windows-amd64.zip",
        "codex-switch-global-pace-windows-arm64.zip",
    ] {
        assert!(
            workflow.contains(archive),
            "release verification must require `{archive}`"
        );
    }
    assert!(workflow.contains("sha256sum --check"));
    assert_before(
        &workflow,
        "Verify release checksums",
        "Create isolated candidate draft",
    );
}

#[test]
fn release_attests_archives_before_publishing_them() {
    let workflow = repo_file(".github/workflows/release.yml");

    for permission in [
        "id-token: write",
        "attestations: write",
        "artifact-metadata: write",
    ] {
        assert!(
            workflow.contains(permission),
            "release workflow must grant `{permission}` to the attestation step"
        );
    }
    assert!(workflow.contains("actions/attest@"));
    assert!(workflow.contains("subject-path:"));
    assert!(workflow.contains("artifacts/*.tar.gz"));
    assert!(workflow.contains("artifacts/*.zip"));
    assert!(workflow.contains("codex-switch-global-pace-build-provenance.json"));
    assert!(workflow.contains("target_commitish:$target"));
    assert!(workflow.contains("--arg target \"$GITHUB_SHA\""));
    assert!(workflow.contains("'.target_commitish'"));
    assert_before(
        &workflow,
        "Attest release archives",
        "Create isolated candidate draft",
    );
}

#[test]
fn windows_daemon_stop_never_force_kills_a_trusted_process() {
    let daemon = repo_file("src/daemon/mod.rs");
    let pidfile = repo_file("src/daemon/pidfile.rs");
    let service = repo_file("src/daemon/service.rs");
    assert!(
        !daemon.contains("pidfile::force_kill(pid)"),
        "a trusted daemon may be rotating credentials; a graceful-stop timeout must fail visibly \
         instead of force-killing it"
    );

    let uninstall_start = daemon.find("fn uninstall(expected_executable:").unwrap();
    let uninstall_end = daemon[uninstall_start..].find("async fn start").unwrap() + uninstall_start;
    let uninstall = &daemon[uninstall_start..uninstall_end];
    assert!(
        uninstall.matches("pidfile::running_pid_checked()?").count() >= 2,
        "Windows uninstall must check the PID-lock authority before graceful shutdown and again \
         immediately before Task Scheduler may force-stop the daemon"
    );

    let stop_start = daemon
        .find("fn stop(expected_service_executable:")
        .expect("daemon stop must keep one explicit service-executable authority boundary");
    let stop_end = daemon[stop_start..].find("fn stop_detached").unwrap() + stop_start;
    let stop = &daemon[stop_start..stop_end];
    assert!(
        stop.contains("pidfile::running_pid_checked()?"),
        "Windows stop must use the checked PID-lock authority before Task Scheduler may use /End"
    );
    assert!(
        !daemon.contains("service::is_installed()"),
        "daemon mutation paths must not fold scheduler or service-marker errors into detached mode"
    );
    let detached_start = daemon.find("fn stop_detached()").unwrap();
    let detached_end = daemon[detached_start..]
        .find("fn wait_until_stopped(")
        .unwrap()
        + detached_start;
    let detached = &daemon[detached_start..detached_end];
    assert!(
        detached.contains("pidfile::running_pid_checked()?")
            && detached.contains("pidfile::request_shutdown(pid)?"),
        "a live daemon must be selected by its held PID lock and stopped with its generation-bound request"
    );
    assert!(
        !detached.contains("let _ = pidfile::cleanup_pidfile();"),
        "Windows graceful-stop completion must propagate a locked PID-file cleanup failure"
    );
    assert!(
        detached.contains("wait_until_stopped(Some(pid))")
            && detached.contains("pidfile::cleanup_pidfile()?"),
        "detached stop must confirm that the selected PID generation exited before cleaning its PID file"
    );

    assert!(
        pidfile.contains("generation: identity.generation"),
        "the Windows shutdown request must be bound to the exact daemon generation"
    );
    assert!(
        pidfile.contains("fn legacy_pidfile_lock_is_held_checked"),
        "the one-version same-file lock migration must share an explicit checked authority probe"
    );
    assert!(
        !pidfile.contains("Command::new(\"tasklist\")")
            && !daemon.contains("Command::new(\"tasklist\")")
            && !service.contains("Command::new(\"tasklist\")"),
        "tasklist must not remain as a daemon transaction authority"
    );

    let scheduled_stop_start = service
        .find("fn stop_scheduled_daemon_for_rollback()")
        .unwrap();
    let scheduled_stop_end = service[scheduled_stop_start..]
        .find("fn uninstall_task_scheduler(")
        .unwrap()
        + scheduled_stop_start;
    let scheduled_stop = &service[scheduled_stop_start..scheduled_stop_end];
    assert!(
        scheduled_stop.contains("crate::daemon::pidfile::request_shutdown(pid)"),
        "scheduled-daemon rollback must request shutdown from the generation selected by the PID lock"
    );
    assert!(
        scheduled_stop
            .matches("crate::daemon::pidfile::running_pid_checked()")
            .count()
            >= 3,
        "scheduled-daemon rollback must wait for checked lock release and recheck immediately before /End"
    );
    assert_before(
        scheduled_stop,
        "crate::daemon::pidfile::request_shutdown(pid)",
        "\"/End\"",
    );

    let service_uninstall_start = service.find("fn uninstall_task_scheduler(").unwrap();
    let service_uninstall = &service[service_uninstall_start..];
    assert!(
        service_uninstall.contains("wait_for_daemon_absence_after_service_stop(")
            && service_uninstall.contains("validate_task_scheduler_definition_owner("),
        "service definition removal must require checked PID-lock absence after the scheduler stop"
    );
}

#[test]
fn dev_release_uses_the_short_calendar_prerelease_version() {
    let workflow = repo_file(".github/workflows/release.yml");

    assert!(workflow.contains("version=${BASE}-dev"));
    assert!(!workflow.contains("TIMESTAMP"));
    assert!(!workflow.contains("-dev.${TIMESTAMP}"));
}

#[test]
fn readmes_describe_current_cli_and_codex_requirements() {
    for path in ["README.md", "README_CN.md"] {
        let readme = repo_file(path);
        assert!(!readme.contains("use --force"), "stale command in {path}");
        assert!(!readme.contains("codex --quiet"), "stale command in {path}");
        for required in [
            "self-update --stable",
            "codex-switch-global-pace",
            "Global Weekly Pace",
            "cli_auth_credentials_store",
            "CODEX_HOME",
            ".codex-switch",
            "equal",
        ] {
            assert!(
                readme.contains(required),
                "{path} must document `{required}`"
            );
        }
    }
}

#[test]
fn installer_instructions_use_channel_matched_release_assets() {
    let stable_unix = "https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.sh";
    let stable_windows = "https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.ps1";
    let dev_unix =
        "https://github.com/chriskooCK/codex-switch-global-pace/releases/download/dev/install.sh";
    let dev_windows =
        "https://github.com/chriskooCK/codex-switch-global-pace/releases/download/dev/install.ps1";

    for path in [
        "README.md",
        "README_CN.md",
        "scripts/install.sh",
        "scripts/install.ps1",
        ".github/workflows/release.yml",
    ] {
        let text = repo_file(path);
        assert!(
            !text.contains("raw.githubusercontent.com/chriskooCK/codex-switch-global-pace/master/scripts/install"),
            "{path} must not direct users to the stale installer on the master branch"
        );
    }

    for path in ["README.md", "README_CN.md"] {
        let readme = repo_file(path);
        for required in [stable_unix, stable_windows] {
            assert!(
                readme.contains(required),
                "{path} must contain channel-matched installer URL `{required}`"
            );
        }
    }

    let development = repo_file("docs/wiki/Development-Releases.md");
    assert!(development.contains(dev_unix));
    assert!(development.contains(dev_windows));

    let unix_installer = repo_file("scripts/install.sh");
    assert!(unix_installer.contains(stable_unix));
    assert!(unix_installer.contains(dev_unix));

    let windows_installer = repo_file("scripts/install.ps1");
    assert!(windows_installer.contains(stable_windows));
    assert!(windows_installer.contains(dev_windows));

    let workflow = repo_file(".github/workflows/release.yml");
    assert!(workflow.contains(stable_unix));
    assert!(workflow.contains(stable_windows));
    assert!(workflow.contains(dev_unix));
    assert!(workflow.contains(dev_windows));
}

#[test]
fn self_update_help_limits_automatic_checks_to_tui_startup() {
    let cli = repo_file("src/cli.rs");

    assert!(cli.contains("Only the TUI checks automatically at startup"));
    assert!(cli.contains("Other commands never check automatically"));
}

#[test]
fn plain_self_update_keeps_dev_installs_on_the_dev_channel() {
    let command = repo_file("src/commands/update.rs");

    assert!(command.contains("update::is_dev_version(update::current_version())"));
    assert!(command.contains("update::check_for_dev_update().await?"));
    assert!(command.contains("update::self_update_dev(show_progress, update_lease.clone()).await"));
    assert!(
        command.contains("update::self_update(version, show_progress, update_lease.clone()).await")
    );
    assert!(command.contains("else if stable || version.is_some()"));
    assert_before(&command, "if dev", "else if stable || version.is_some()");
}

#[test]
fn release_docs_describe_platform_specific_archive_formats() {
    let release = repo_file("docs/RELEASE.md");

    assert!(
        release.contains("Linux / macOS") && release.contains("`.tar.gz`"),
        "release docs must describe Unix tar.gz artifacts"
    );
    assert!(
        release.contains("Windows") && release.contains("`.zip`"),
        "release docs must describe Windows zip artifacts"
    );
    assert!(
        !release.contains("6 平台 tarball"),
        "release docs must not call Windows zip artifacts tarballs"
    );
}

#[test]
fn changelog_tracks_the_calendar_version_development_cycle() {
    let changelog = repo_file("docs/CHANGELOG.md");
    assert!(
        changelog.contains("## v20260713.2.0 — 2026-07-13"),
        "the final dev candidate must carry the stable release heading before zero-drift acceptance"
    );
}
