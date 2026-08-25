mod support;

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

#[cfg(unix)]
fn unix_installer_test_command() -> std::process::Command {
    let helper = env!("CARGO_BIN_EXE_codex-switch-global-pace");
    let mut command = std::process::Command::new("bash");
    command
        .env("CANDIDATE_BIN", helper)
        .env("REAL_INSTALLER_HELPER", helper);
    command
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
        "group: ${{ github.workflow }}-publication",
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
            tail.split("- name: Acquire shared remote publication lock")
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
fn stable_and_local_publication_share_one_exact_remote_lock() {
    let workflow = repo_file(".github/workflows/release.yml");
    let publisher = repo_file("scripts/publish-dev.ps1");
    let release_docs = repo_file("docs/RELEASE.md");
    let lock_tag = "codex-switch-publish-dev-lock";

    assert!(workflow.contains(&format!("lock_tag=\"{lock_tag}\"")));
    assert!(publisher.contains(&format!("$RemoteLockTag = '{lock_tag}'")));
    assert_before(
        &workflow,
        "Acquire shared remote publication lock",
        "Confirm release tag still targets this source before publish",
    );
    assert_before(
        &workflow,
        "Acquire shared remote publication lock",
        "Inspect an existing exact-tag release",
    );
    assert_before(
        &workflow,
        "Acquire shared remote publication lock",
        "Create isolated candidate draft",
    );

    let acquire = workflow
        .split("- name: Acquire shared remote publication lock")
        .nth(1)
        .and_then(|tail| {
            tail.split("- name: Confirm release tag still targets this source before publish")
                .next()
        })
        .expect("shared stable publication lock acquisition step");
    for required in [
        "id: publication_lock",
        "if: needs.meta.outputs.is_dev != 'true'",
        "git/ref/tags/${lock_tag}",
        "Shared publication lock ${lock_ref} already exists; it was not acquired or removed.",
        "Shared publication lock absence could not be established; no ownership was claimed.",
        "od -An -N16 -tx1 /dev/urandom",
        "codex-switch-global-pace publish-dev lock v1|repo=",
        "repos/${GITHUB_REPOSITORY}/git/tags",
        "repos/${GITHUB_REPOSITORY}/git/refs",
        "acquisition failed or its response was lost; exact candidate cleanup will inspect the fixed ref before removing anything",
        "echo \"tag_object_sha=${tag_object_sha}\"",
        "echo \"source_sha=${source_sha}\"",
        "echo \"transaction=${transaction}\"",
        "echo \"message=${message}\"",
        "echo \"cleanup_candidate=true\"",
    ] {
        assert!(
            acquire.contains(required),
            "stable shared-lock acquisition must contain `{required}`"
        );
    }
    assert_before(
        acquire,
        "shared-publication-lock-existing-error",
        "shared-publication-lock-tag.json",
    );
    assert_before(
        acquire,
        "shared-publication-lock-tag-error",
        "shared-publication-lock-ref.json",
    );
    assert_before(
        acquire,
        "echo \"message=${message}\"",
        "echo \"cleanup_candidate=true\"",
    );
    assert_before(
        acquire,
        "echo \"cleanup_candidate=true\"",
        "shared-publication-lock-ref.json",
    );
    assert!(!acquire.contains("--method DELETE"));
    assert!(!acquire.contains("owned=true"));

    let release_lock = workflow
        .split("- name: Recover or release exact shared remote publication lock")
        .nth(1)
        .expect("shared stable publication lock release step");
    for required in [
        "if: always() && needs.meta.outputs.is_dev != 'true' && steps.publication_lock.outputs.cleanup_candidate == 'true'",
        "LOCK_TAG_OBJECT_SHA: ${{ steps.publication_lock.outputs.tag_object_sha }}",
        "LOCK_SOURCE_SHA: ${{ steps.publication_lock.outputs.source_sha }}",
        "LOCK_TRANSACTION: ${{ steps.publication_lock.outputs.transaction }}",
        "LOCK_MESSAGE: ${{ steps.publication_lock.outputs.message }}",
        "shared-publication-lock-release-ref-error",
        "shared-publication-lock-release-tag-error",
        "Shared publication lock ${expected_ref} was never created or is already absent.",
        "Shared publication lock ref changed identity; it was preserved.",
        "Shared publication lock object changed identity; it was preserved.",
        "--force-with-lease=${expected_ref}:${LOCK_TAG_OBJECT_SHA}",
        "shared-publication-lock-after-delete-error",
        "The shared publication lock changed identity during leased deletion; the new ref was preserved.",
        "Shared publication lock deletion state is ambiguous; deletion was not retried.",
        "Released exact shared publication lock ${expected_ref}.",
    ] {
        assert!(
            release_lock.contains(required),
            "stable shared-lock release must contain `{required}`"
        );
    }
    assert_before(
        release_lock,
        "shared-publication-lock-release-ref-error",
        "shared-publication-lock-release-tag-error",
    );
    assert_before(
        release_lock,
        "shared-publication-lock-release-tag-error",
        "--force-with-lease=${expected_ref}:${LOCK_TAG_OBJECT_SHA}",
    );
    assert_before(
        release_lock,
        "--force-with-lease=${expected_ref}:${LOCK_TAG_OBJECT_SHA}",
        "shared-publication-lock-after-delete-error",
    );
    assert!(!release_lock.contains("--method DELETE"));
    assert_before(
        &workflow,
        "Remove only this run's incomplete candidate",
        "Recover or release exact shared remote publication lock",
    );

    for required in [
        "historical name and v1 annotated-tag identity format are",
        "intentionally retained: renaming the tag would let stable publication overlook",
        "shared remote lease additionally serializes stable Actions publication",
        "with the local development publisher",
        "The stable job persists its",
        "before it asks",
        "Its final `always()` step",
        "also runs when",
        "request fails or its response is lost",
        "treats an absent ref as a",
        "exact Git `--force-with-lease`",
        "It does not retry,",
        "force-delete a different ref, or infer ownership from later visibility",
        "an ambiguous local",
        "ref-create response remains a manual-recovery case",
    ] {
        assert!(
            release_docs.contains(required),
            "release docs must state the shared lock contract: `{required}`"
        );
    }
}

#[test]
fn stable_release_stages_isolated_candidates_and_fails_closed_on_drift() {
    let workflow = repo_file(".github/workflows/release.yml");

    for required in [
        "if: needs.meta.outputs.is_dev != 'true'",
        "Acquire shared remote publication lock",
        "Confirm release tag still targets this source before publish",
        "Inspect an existing exact-tag release",
        "Create isolated candidate draft",
        "candidate_tag=\"release-candidate-${final_tag}\"",
        "gh api --paginate --slurp",
        "releases?per_page=100",
        "Found ${candidate_release_count} releases for candidate tag ${candidate_tag}; refusing ambiguous recovery.",
        "repos/${GITHUB_REPOSITORY}/releases/${prior_release_id}",
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
    assert!(workflow.contains("group: ${{ github.workflow }}-publication"));
    assert!(!workflow.contains("group: release-${{ github.ref }}"));
    let candidate_creation = workflow
        .split("- name: Create isolated candidate draft")
        .nth(1)
        .and_then(|tail| {
            tail.split("- name: Upload and verify isolated candidate assets")
                .next()
        })
        .expect("candidate draft creation and interrupted-run recovery step");
    assert!(
        !candidate_creation.contains("releases/tags/${candidate_tag}"),
        "draft recovery must use the authenticated paginated release list because the tag endpoint only returns published releases"
    );
    assert_before(
        candidate_creation,
        "releases?per_page=100",
        "releases/${prior_release_id}",
    );
    let release_page_validation = r#"if ! jq -e '
            (type == "array")
            and (length > 0)
            and all(.[];
              (type == "array")
              and all(.[];
                (type == "object")
                and (.id | (type == "number") and (. > 0) and (floor == .))
                and (.tag_name | type == "string")
                and (.tag_name | length > 0)
              )
            )
          ' "$candidate_releases" > /dev/null"#;
    assert!(
        candidate_creation.contains(release_page_validation),
        "candidate recovery must keep the complete fail-closed paginated release validator"
    );
    assert_before(
        candidate_creation,
        release_page_validation,
        "candidate_release_count=$(jq \\",
    );
    assert_before(
        candidate_creation,
        "releases/${prior_release_id}",
        "prior-candidate-release-assets\" subset",
    );
    assert_before(
        candidate_creation,
        "git/refs/tags/${candidate_tag}",
        "prior_release_again=$(gh api \\",
    );
}

#[cfg(target_os = "linux")]
#[test]
fn stable_release_pagination_validator_executes_fail_closed() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let workflow = repo_file(".github/workflows/release.yml");
    let filter = workflow
        .split("if ! jq -e '\n")
        .nth(1)
        .and_then(|tail| {
            tail.split("\n          ' \"$candidate_releases\" > /dev/null; then")
                .next()
        })
        .expect("paginated release jq filter");

    let accepts = |fixture: &str| {
        let mut child = Command::new("jq")
            .args(["-e", filter])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("release validation requires jq on its Linux runner");
        child
            .stdin
            .take()
            .expect("jq stdin")
            .write_all(fixture.as_bytes())
            .expect("write jq fixture");
        child.wait().expect("wait for jq fixture").success()
    };

    assert!(accepts(
        r#"[[],[{"id":1,"tag_name":"release-candidate-v1"}]]"#
    ));
    for malformed in [
        "[]",
        "{}",
        "[{}]",
        "[[null]]",
        r#"[[{"id":"1","tag_name":"candidate"}]]"#,
        r#"[[{"id":1.5,"tag_name":"candidate"}]]"#,
        r#"[[{"id":1}]]"#,
        r#"[[{"id":1,"tag_name":""}]]"#,
    ] {
        assert!(
            !accepts(malformed),
            "paginated release validator accepted malformed input: {malformed}"
        );
    }
}

#[test]
fn stable_release_recovery_states_the_external_writer_boundary() {
    let workflow = repo_file(".github/workflows/release.yml");
    let release_docs = repo_file("docs/RELEASE.md");

    for required in [
        "Release deletion APIs have no conditional version precondition",
        "atomic compare-and-delete against an external administrator",
    ] {
        assert!(
            workflow.contains(required),
            "workflow must state its non-cooperating writer boundary: `{required}`"
        );
    }
    for required in [
        "These guarantees serialize participating workflow runs.",
        "this is not an atomic compare-and-delete against a repository administrator",
        "Administrators must not manually change candidate refs or Releases",
    ] {
        assert!(
            release_docs.contains(required),
            "release docs must state their external-writer boundary: `{required}`"
        );
    }
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
        "dev-candidate-([1-9][0-9]*)-(draft|public)-([0-9a-f]{64})-([0-9a-f]{32})",
        "dev-park-([1-9][0-9]*)-([1-9][0-9]*)-(draft|public)-([0-9a-f]{64})-([0-9a-f]{32})",
        "Multiple development publication journals exist",
        "Park journal '$tag' does not identify its own release ID.",
        "function RecoverJournal",
        "CandidateProjection",
        "CandidateExact",
        "CandidateCreateAmbiguous",
        "CandidateCreated",
        "function AssertCandidateMetadata",
        "function AcceptCreatedCandidate",
        "-Exact:([bool]$ExactAssets)",
        "DownloadProjection",
        "[Parameter(Mandatory = $true)][AllowEmptyCollection()][string[]]$Names",
        "rollback-assets-",
        "exact local-bundle subset member",
        "Candidate creation response was ambiguous",
        "authoritatively created candidate is temporarily unavailable",
        "Recovered interrupted development publication",
        "Prior dev release is not a mutable SHA-bound prerelease.",
        "Prior dev release drifted before candidate creation.",
        "function AssertCurrentPublicExact",
        "function ReleaseAnyTag",
        "is already exact at $sha",
        "dev-candidate-$oldId-$oldVisibility-$oldFingerprint-$tx",
        "dev-park-$oldId-$($Context.CandidateId)-$oldVisibility-$oldFingerprint-$tx",
        "OldDraft",
        "OldFingerprint",
        "codex-switch-old-release-v2;",
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
        "[Parameter(Mandatory = $true)][bool]$OriginalDraft",
        "codex-switch-old-release-v2;",
        "if ($OriginalDraft) { 'draft' } else { 'public' }",
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
    assert!(!publisher.contains("codex-switch-old-release-v1;"));
    let pages = publisher
        .split("function Pages")
        .nth(1)
        .and_then(|section| section.split("function RepoBytes").next())
        .expect("release pagination function");
    for required in [
        "[object[]]$batch = @()",
        "if ($null -ne $value) { $batch = [object[]]@($value) }",
        "$batch.Length -eq 100",
    ] {
        assert!(
            pages.contains(required),
            "empty release pages must be represented explicitly with `{required}`"
        );
    }
    assert!(!pages.contains("$batch = if"));
    let candidate_state = publisher
        .split("function AssertCandidateMetadata")
        .nth(1)
        .and_then(|section| section.split("function AcceptCreatedCandidate").next())
        .expect("candidate staged/final state function");
    for required in [
        "$stagedState",
        "$finalState",
        "$finalState = ((Prop $R 'tag_name') -eq 'dev' -and -not [bool](Prop $R 'draft'))",
        "Staged = $stagedState",
        "Final = $finalState",
    ] {
        assert!(
            candidate_state.contains(required),
            "candidate state contract must contain `{required}`"
        );
    }
    let created_candidate = publisher
        .split("function AcceptCreatedCandidate")
        .nth(1)
        .and_then(|section| section.split("function AssertCandidate(").next())
        .expect("authoritative candidate-create response function");
    for required in [
        "$Result.Out | ConvertFrom-Json",
        "AssertCandidateMetadata $C $candidate",
        "$candidate.PSObject.Properties['assets']",
        "[object[]]$responseAssets = [object[]]@($assetsProperty.Value)",
        "$responseAssets.Length -ne 0",
        "$C.CandidateId = $state.Id",
        "$C.CandidateCreateAmbiguous = $false",
        "$C.CandidateCreated = $true",
    ] {
        assert!(
            created_candidate.contains(required),
            "candidate-create response contract must contain `{required}`"
        );
    }
    assert!(!created_candidate.contains("AllReleases"));
    assert!(!created_candidate.contains("FindCandidate"));
    assert_before(
        created_candidate,
        "AssertCandidateMetadata $C $candidate",
        "$C.CandidateId = $state.Id",
    );
    assert_before(
        created_candidate,
        "$responseAssets.Length -ne 0",
        "$C.CandidateCreateAmbiguous = $false",
    );
    let projection_download = publisher
        .split("function DownloadProjection")
        .nth(1)
        .and_then(|section| section.split("function RemoveRelease").next())
        .expect("candidate projection download function");
    for required in [
        "[object[]]$rows = @()",
        "[string[]]$names = [string[]]@(",
        "if ($names.Length -gt 0)",
        "ExactFiles -Dir $Dir -Names $names",
    ] {
        assert!(
            projection_download.contains(required),
            "empty candidate projection contract must contain `{required}`"
        );
    }
    let journal_discovery = publisher
        .split("function DiscoverJournal")
        .nth(1)
        .and_then(|section| section.split("function RecoverJournal").next())
        .expect("dev journal discovery function");
    for required in [
        "$candidate.OldId -ne $park.OldId",
        "$candidate.OldDraft -ne $park.OldDraft",
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
        "OldDraft = [bool]$Journal.OldDraft",
        "Fingerprint $old ([bool]$Journal.OldDraft)",
        "if ($owned.Final)",
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
    for required in [
        "Fingerprint $old ([bool]$C.OldDraft)",
        "$owned.Final",
        "$owned.Staged",
        "draft = [bool]$C.OldDraft",
        "AssertState $restored.Value $C.OldId 'dev' $C.OldTarget ([bool]$C.OldDraft)",
    ] {
        assert!(
            rollback.contains(required),
            "visibility-preserving rollback must contain `{required}`"
        );
    }
    let idempotent = publisher
        .split("function AssertCurrentPublicExact")
        .nth(1)
        .and_then(|section| section.split("function Rollback").next())
        .expect("exact-current verification function");
    assert_before(idempotent, "DownloadExact 'dev'", "ReleaseAnyTag 'dev'");
    let exact_current_guard = publisher
        .split("$current = ReleaseAnyTag 'dev'")
        .nth(1)
        .and_then(|section| section.split("$oldByTag = ReleaseAnyTag 'dev'").next())
        .expect("public exact-current guard");
    assert!(exact_current_guard.contains("AssertCurrentPublicExact $current.Value"));
    assert!(exact_current_guard.contains("-not [bool](Prop $current.Value 'draft')"));
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
        "$oldByTag = ReleaseAnyTag 'dev'",
    );
    let candidate_create_transaction = publisher
        .split("$createBody =")
        .nth(1)
        .and_then(|section| section.split("$parkTag =").next())
        .expect("candidate creation transaction");
    for required in [
        "$Context.CandidateCreateAmbiguous = $true",
        "Create candidate draft",
        "AcceptCreatedCandidate $Context $created",
    ] {
        assert!(
            candidate_create_transaction.contains(required),
            "candidate creation transaction must contain `{required}`"
        );
    }
    assert!(!candidate_create_transaction.contains("FindCandidate"));
    assert!(!candidate_create_transaction.contains("AllReleases"));
    assert_before(
        candidate_create_transaction,
        "$Context.CandidateCreateAmbiguous = $true",
        "Create candidate draft",
    );
    assert_before(
        candidate_create_transaction,
        "Create candidate draft",
        "AcceptCreatedCandidate $Context $created",
    );
    assert_before(
        &publisher,
        "DownloadExact $candidateTag $remote $local",
        "Park old dev release",
    );
    assert_before(&publisher, "Park old dev release", "Finalize candidate");
    assert_before(&publisher, "Finalize candidate", "RemoveRelease $oldId");
    assert_before(
        &publisher,
        "RemoveRelease $oldId",
        "$CutoverComplete = $true",
    );
    assert_before(
        &publisher,
        "Final dev release ID is not the replacement candidate.",
        "$CutoverComplete = $true",
    );
    for required in [
        "$oldVisibility = if ($oldDraft) { 'draft' } else { 'public' }",
        "$oldFingerprint = Fingerprint $old $oldDraft",
        "draft = $false",
        "AssertState $finalized $Context.CandidateId 'dev' $sha $false",
    ] {
        assert!(
            publisher.contains(required),
            "replacement transaction visibility contract must contain `{required}`"
        );
    }
    assert!(!publisher.contains("Existing dev release is unexpectedly a draft."));
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
    assert!(release_docs.contains("prior `dev` release was a draft or public"));
    assert!(release_docs.contains("successful replacement always"));
    assert!(release_docs.contains("exact draft/public state"));
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
        "capture_installer_file_copy",
        "move_installer_file_noreplace",
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
    let daemon = repo_file("src/daemon/mod.rs");
    let service = repo_file("src/daemon/service.rs");
    let install = script
        .split("# Download, verify, and extract")
        .nth(1)
        .expect("Unix install transaction section");
    for required in [
        "prepare_daemon_upgrade",
        "read_checked_daemon_status",
        "start_daemon_update_boundary",
        "request_daemon_update_boundary_new_state",
        "restore_daemon_update_boundary_old_state",
        "finish_daemon_update_boundary",
        "release_daemon_update_boundary",
        "abort_install_upgrade",
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
        "start_daemon_update_boundary",
        "stage_and_replace_binary",
    );
    assert_before(
        install,
        "request_daemon_update_boundary_new_state",
        "commit_installed_binary",
    );
    assert_before(
        install,
        "hold_legacy_install_for_commit",
        "request_daemon_update_boundary_new_state",
    );
    assert_before(
        install,
        "commit_managed_path_changes",
        "request_daemon_update_boundary_new_state",
    );
    let final_confirmation = install
        .rfind("! finish_daemon_update_boundary")
        .expect("final daemon confirmation");
    let executable_commit = install
        .rfind("! commit_installed_binary")
        .expect("executable recovery cleanup");
    let artifact_cleanup = install
        .rfind("! cleanup_install_artifacts")
        .expect("fixed transaction artifact cleanup");
    let authority_release = install
        .rfind("! release_daemon_update_boundary")
        .expect("daemon authority release");
    assert!(
        final_confirmation < executable_commit
            && executable_commit < artifact_cleanup
            && artifact_cleanup < authority_release,
        "success must verify the daemon, clean exact recovery material, then release lifecycle authority"
    );
    assert!(
        install.rfind("release_daemon_update_boundary").unwrap()
            < install.rfind("release_update_locks").unwrap(),
        "the success path must release update locks only after final daemon authority release"
    );
    assert!(
        !script.contains("service_definition_references_binary"),
        "the shell must not parse launchd or systemd definitions"
    );
    for required in [
        "check_candidate_uninstall_owner \"$INSTALL_DEST\"",
        "check_candidate_uninstall_owner \"$LEGACY_BIN\"",
    ] {
        assert!(
            script.contains(required),
            "missing exact Rust service-owner boundary `{required}`"
        );
    }
    for required in [
        "fn capture_for_executable(",
        "pidfile::running_identity_checked()?",
        "validate_running_daemon_executable(&executable",
        "crate::fs_ops::token_for_path(expected_executable)",
        "crate::fs_ops::token_for_path(running_executable)",
        "service::install_for_executable_locked(",
        "reacquire_absence_after_foreground_contenders",
        "DaemonAbsenceAcquireFor::Contended",
    ] {
        assert!(
            daemon.contains(required),
            "missing retained Rust lifecycle boundary `{required}`"
        );
    }
    assert!(service.contains("pub(crate) fn install_for_executable_locked("));
    for removed in [
        "restart_daemon_after_upgrade()",
        "ensure_previous_daemon_running()",
        "stop_restarted_daemon_for_rollback()",
    ] {
        assert!(
            !script.contains(removed),
            "obsolete split daemon lifecycle helper remains: `{removed}`"
        );
    }
}

#[test]
fn unix_installer_lifecycle_holder_covers_fresh_publish_and_exit_cleanup_order() {
    let script = repo_file("scripts/install.sh");
    let cli = repo_file("src/cli.rs");
    let main = repo_file("src/app.rs");
    let daemon = repo_file("src/daemon/mod.rs");
    let service = repo_file("src/daemon/service.rs");
    assert!(cli.contains("name = \"__hold-daemon-update-boundary\""));
    assert!(script.contains("\"$candidate\" __hold-daemon-update-boundary"));
    assert_before(
        &main,
        "Some(Commands::HoldDaemonUpdateBoundary",
        "// The release-verified direct installer uses this hidden boundary",
    );
    let hidden_dispatch = main
        .split("Some(Commands::HoldDaemonUpdateBoundary")
        .nth(1)
        .and_then(|section| {
            section
                .split("// The release-verified direct installer uses this hidden boundary")
                .next()
        })
        .expect("hidden daemon-boundary dispatch");
    assert_before(
        hidden_dispatch,
        "output::set_message_mode(MessageMode::Silent)",
        "daemon::hold_installer_daemon_update_boundary(",
    );
    assert!(
        !service.contains(".status()"),
        "service-manager child processes must capture stdout/stderr instead of inheriting the holder's marker-only stdout"
    );
    for command in ["plutil", "systemd-analyze", "systemctl"] {
        let command_section = service
            .split(&format!("Command::new(\"{command}\")"))
            .nth(1)
            .expect("service-manager command invocation");
        assert!(
            command_section
                .split(';')
                .next()
                .expect("service-manager command expression")
                .contains(".output()"),
            "{command} must capture child output so the lifecycle FIFO remains marker-only"
        );
    }
    assert_eq!(
        daemon
            .matches("codex-switch-global-pace daemon update boundary")
            .count(),
        1,
        "Rust wire-protocol prefix must have one definition"
    );
    assert_eq!(
        script.matches("DAEMON_BOUNDARY_PROTOCOL_PREFIX=").count(),
        1,
        "shell wire-protocol prefix must have one definition"
    );
    let prepare = script
        .split("prepare_daemon_upgrade() {")
        .nth(1)
        .and_then(|section| section.split("abort_install_upgrade() {").next())
        .expect("daemon preflight implementation");
    assert!(
        prepare.contains("DAEMON_PREVIOUS_BIN=\"$INSTALL_DEST\""),
        "fresh installs must bind the lifecycle holder to the future public path"
    );

    let transaction = script
        .split("\nSYSTEM_MARKER_CREATED=false\n")
        .nth(1)
        .expect("Unix install transaction section");
    assert_before(
        transaction,
        "start_daemon_update_boundary",
        "stage_and_replace_binary",
    );
    assert_before(
        transaction,
        "stage_and_replace_binary",
        "request_daemon_update_boundary_new_state",
    );
    assert_before(
        transaction,
        "request_daemon_update_boundary_new_state",
        "finish_daemon_update_boundary",
    );
    assert_before(
        transaction,
        "finish_daemon_update_boundary",
        "commit_installed_binary",
    );
    let holder = daemon
        .split("pub(crate) fn hold_installer_daemon_update_boundary(")
        .nth(1)
        .and_then(|section| section.split("pub(crate) fn print_installer_state").next())
        .expect("Unix installer daemon holder");
    assert_before(
        holder,
        "let mut phase = InstallerBoundaryPhase::Stopping;",
        "catch_installer_boundary_unwind(||",
    );
    assert_before(
        holder,
        "catch_installer_boundary_unwind(||",
        "transaction.stop_before_update_inner()?;",
    );
    assert_before(
        holder,
        "transaction.stop_before_update_inner()?;",
        "phase = InstallerBoundaryPhase::Stopped;",
    );
    assert_before(
        holder,
        "phase = InstallerBoundaryPhase::Stopped;",
        "run_installer_daemon_boundary_protocol(",
    );
    assert_before(
        holder,
        "run_installer_daemon_boundary_protocol(",
        "finalize_installer_boundary_result(boundary_result",
    );
    assert!(holder.contains("transaction.finalize_installer_boundary_error("));

    let protocol = daemon
        .split("fn run_installer_daemon_boundary_protocol")
        .nth(1)
        .and_then(|section| {
            section
                .split("pub(crate) fn hold_installer_daemon_update_boundary")
                .next()
        })
        .expect("phase-aware installer daemon protocol");
    assert_before(protocol, "ready {}", "transaction.verify_final_state()?;");
    assert!(daemon.contains(
        "InstallerBoundaryPhase::Stopping => InstallerBoundaryFinalization::RestoreFailedStop"
    ));
    assert!(daemon.contains("InstallerBoundaryFinalization::ReestablishStopped"));
    assert!(daemon.contains("InstallerBoundaryFinalization::ClassifyUninstall"));

    let exit_cleanup = script
        .split("cleanup_install_exit() {")
        .nth(1)
        .and_then(|section| section.split("cleanup_installer_temp_directory() {").next())
        .expect("Unix installer EXIT cleanup");
    assert_before(
        exit_cleanup,
        "cleanup_daemon_update_boundary_on_exit",
        "cleanup_install_artifacts",
    );
    assert_before(
        exit_cleanup,
        "cleanup_install_artifacts",
        "cleanup_update_locks_on_exit",
    );
    assert_before(
        exit_cleanup,
        "cleanup_update_locks_on_exit",
        "cleanup_installer_temp_directory",
    );
}

#[test]
fn unix_installer_accepts_only_the_candidate_exact_state_tuple() {
    let script = repo_file("scripts/install.sh");
    let parser = script
        .split("read_checked_daemon_status() {")
        .nth(1)
        .and_then(|section| section.split("verify_candidate_version() {").next())
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
        .split("\nSYSTEM_MARKER_CREATED=false\n")
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
        transaction.rfind("commit_managed_path_changes").unwrap()
            < transaction.rfind("commit_installed_binary").unwrap(),
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
fn unix_installer_binds_recovery_files_and_keeps_upgrade_cutover_atomic() {
    let script = repo_file("scripts/install.sh");
    let publication = script
        .split("stage_and_replace_binary() {")
        .nth(1)
        .and_then(|section| section.split("commit_installed_binary() {").next())
        .expect("Unix binary publication transaction");
    let rollback = script
        .split("rollback_installed_binary() {")
        .nth(1)
        .and_then(|section| section.split("stage_and_replace_binary() {").next())
        .expect("Unix binary rollback transaction");

    for required in [
        "capture_installer_file_token() {",
        "installer_file_token_matches() {",
        "capture_installer_file_copy() {",
        "exchange_installer_files() {",
        "INSTALL_STAGE_TOKEN",
        "INSTALL_ORIGINAL_TOKEN",
        "INSTALL_PUBLISHED_TOKEN",
        "UNINSTALL_HOLD_TOKEN",
        "LEGACY_ORIGINAL_TOKEN",
    ] {
        assert!(
            script.contains(required),
            "missing recovery binding `{required}`"
        );
    }
    assert!(publication.contains("capture_installer_file_copy \\"));
    assert!(publication.contains("exchange_installer_files \\"));
    assert!(publication.contains("move_installer_file_noreplace \\"));
    assert!(!publication.contains(" ln "));
    assert!(!publication.contains("mv -f"));
    assert!(
        !publication.contains("run_install_fs rm \"$INSTALL_DEST\""),
        "an upgrade must not create a gap at the executable path"
    );
    assert!(rollback.contains("installer_file_token_matches \\"));
    assert!(rollback.contains("exchange_installer_files \\"));
    assert!(rollback.contains("remove_installer_file_owned \\"));
}

#[cfg(unix)]
#[test]
fn unix_binary_transaction_never_removes_a_foreign_replacement() {
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let definitions = script.split("# Parse arguments").next().unwrap();
    let temp = support::tempdir();
    let install_dir = temp.path().join("bin");
    fs::create_dir(&install_dir).unwrap();
    let candidate = temp.path().join("candidate");
    fs::write(&candidate, "candidate-bytes").unwrap();
    let installed = install_dir.join("codex-switch-global-pace");
    fs::write(&installed, "original-bytes").unwrap();

    let harness = format!(
        r#"{definitions}
INSTALL_DIR="$1"
INSTALL_DEST="$INSTALL_DIR/$BINARY_NAME"
INSTALL_STAGE="$INSTALL_DIR/$INSTALL_STAGE_NAME"
INSTALL_BACKUP="$INSTALL_DIR/$INSTALL_BACKUP_NAME"
CANDIDATE_BIN="$3"
INSTALL_WITH_SUDO=false
INSTALL_STAGE_OWNED=false
INSTALL_STAGE_TOKEN=""
BINARY_REPLACED=false
CANDIDATE_ERROR=""
stage_and_replace_binary "$2"
printf '%s' foreign-bytes > "$INSTALL_DEST.foreign"
mv -f "$INSTALL_DEST.foreign" "$INSTALL_DEST"
if commit_installed_binary; then exit 70; fi
if rollback_installed_binary; then exit 71; fi
[ "$(cat "$INSTALL_DEST")" = foreign-bytes ]
[ "$(cat "$INSTALL_BACKUP")" = original-bytes ]
"#
    );
    let output = Command::new("bash")
        .args(["-c", &harness, "installer-test"])
        .arg(&install_dir)
        .arg(&candidate)
        .arg(env!("CARGO_BIN_EXE_codex-switch-global-pace"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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
        "begin_uninstall_file_transaction",
        "start_daemon_update_boundary",
        "commit_managed_path_changes",
        "hold_uninstall_binary_for_commit",
        "request_daemon_update_boundary_uninstall_state",
        "commit_uninstall_file_transaction",
        "finish_daemon_update_boundary",
        "release_daemon_update_boundary",
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
        "start_daemon_update_boundary",
    );
    assert_before(
        uninstall,
        "commit_managed_path_changes",
        "hold_uninstall_binary_for_commit",
    );
    assert_before(
        uninstall,
        "hold_uninstall_binary_for_commit",
        "request_daemon_update_boundary_uninstall_state",
    );
    assert_before(
        uninstall,
        "request_daemon_update_boundary_uninstall_state",
        "commit_uninstall_file_transaction",
    );
    assert_before(
        uninstall,
        "finish_daemon_update_boundary",
        "commit_uninstall_file_transaction",
    );
    assert_before(
        uninstall,
        "commit_uninstall_file_transaction",
        "release_daemon_update_boundary",
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
        uninstall.rfind("commit_managed_path_changes").unwrap()
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
    assert!(
        !uninstall.contains("\"$CANDIDATE_BIN\" daemon uninstall"),
        "service removal must be executed by the persistent lifecycle holder, not a second child"
    );
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
    let dir = support::tempdir();
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

    let script = repo_file("scripts/install.sh");
    let home = support::tempdir();
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
    : > "$UPDATE_LOCK_HELD"
    printf 'codex-switch-global-pace update lock ready\n'
    cat >/dev/null
    rm -f "$UPDATE_LOCK_HELD"
    ;;
  __hold-daemon-update-boundary)
    [ "$2" = --initial-executable ] || exit 61
    [ "$3" = "$UNINSTALL_TARGET" ] || exit 62
    [ "$4" = --replacement-executable ] || exit 63
    [ "$5" = "$UNINSTALL_TARGET" ] || exit 64
    : > "$LIFECYCLE_HELD"
    printf 'codex-switch-global-pace daemon update boundary ready running=false service_installed=false\n'
    while IFS= read -r command; do
      case "$command" in
        uninstall)
          [ -f "$UPDATE_LOCK_HELD" ] || exit 65
          [ -f "$LIFECYCLE_HELD" ] || exit 66
          [ -e "$UNINSTALL_TARGET" ] || exit 67
          printf 'daemon-uninstalled\n' > "$UNINSTALL_LOG"
          printf 'codex-switch-global-pace daemon update boundary uninstall state ready\n'
          ;;
        finish)
          [ -f "$UPDATE_LOCK_HELD" ] || exit 68
          [ -f "$LIFECYCLE_HELD" ] || exit 69
          : > "$FINAL_CONFIRMED"
          printf 'codex-switch-global-pace daemon update boundary final state confirmed\n'
          ;;
        release)
          [ -f "$UPDATE_LOCK_HELD" ] || exit 70
          [ -f "$LIFECYCLE_HELD" ] || exit 71
          [ -f "$FINAL_CONFIRMED" ] || exit 72
          [ ! -e "$UNINSTALL_TARGET" ] || exit 73
          printf 'codex-switch-global-pace daemon update boundary lifecycle authority released\n'
          rm -f "$LIFECYCLE_HELD"
          exit 0
          ;;
        *) exit 74 ;;
      esac
    done
    exit 70
    ;;
  __installer-file-op)
    exec "$REAL_INSTALLER_HELPER" "$@"
    ;;
  daemon)
    [ "$2" = uninstall ] || exit 71
    [ "$3" = --expected-executable ] || exit 72
    [ "$4" = "$UNINSTALL_TARGET" ] || exit 73
    [ "${5:-}" = --check-owner ] || exit 74
    ;;
  *) exit 75 ;;
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

    let output = unix_installer_test_command()
        .args(["-c", &harness])
        .env("HOME", home.path())
        .env("CANDIDATE", &candidate)
        .env("UNINSTALL_TARGET", &binary)
        .env("UPDATE_LOCK_HELD", &held)
        .env("LIFECYCLE_HELD", home.path().join("lifecycle-held"))
        .env("FINAL_CONFIRMED", home.path().join("final-confirmed"))
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
    assert!(!home.path().join("lifecycle-held").exists());
    assert!(home.path().join("final-confirmed").exists());
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

    let script = repo_file("scripts/install.sh");
    let home = support::tempdir();
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
  __hold-daemon-update-boundary)
    [ "$2" = --initial-executable ] || exit 71
    [ "$3" = "$UNINSTALL_TARGET" ] || exit 72
    [ "$4" = --replacement-executable ] || exit 73
    [ "$5" = "$UNINSTALL_TARGET" ] || exit 74
    printf 'codex-switch-global-pace daemon update boundary ready running=false service_installed=false\n'
    while IFS= read -r command; do
      case "$command" in
        uninstall)
          [ -e "$UNINSTALL_TARGET" ] || exit 75
          : > "$SERVICE_ATTEMPTED"
          printf 'codex-switch-global-pace daemon update boundary uninstall state failed\n'
          ;;
        rollback)
          [ -e "$UNINSTALL_TARGET" ] || exit 76
          printf 'codex-switch-global-pace daemon update boundary old state restored\n'
          exit 0
          ;;
        *) exit 77 ;;
      esac
    done
    exit 78
    ;;
  __installer-file-op)
    exec "$REAL_INSTALLER_HELPER" "$@"
    ;;
  daemon)
    [ "$2" = uninstall ] || exit 79
    [ "$3" = --expected-executable ] || exit 80
    [ "$4" = "$UNINSTALL_TARGET" ] || exit 81
    [ "${5:-}" = --check-owner ] || exit 82
    ;;
  *) exit 83 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755)).unwrap();
    let harness = format!(
        "{}\nSYSTEM_INSTALL=false\nINSTALL_DIR=\"$USER_INSTALL_DIR\"\nINSTALL_DEST=\"$INSTALL_DIR/$BINARY_NAME\"\nOS=\"$TEST_OS\"\nTMP_DIR=\"$HOME/uninstall-tmp\"\nmkdir -p \"$TMP_DIR\"\nINSTALL_STAGE=\nINSTALL_BACKUP=\nINSTALL_WITH_SUDO=false\nUPDATE_LOCK_PID_8=\nUPDATE_LOCK_PID_9=\nUPDATE_LOCK_ERROR=\nCANDIDATE_BIN=\"$CANDIDATE\"\nrun_uninstall\n",
        unix_uninstall_harness(&script)
    );
    let output = unix_installer_test_command()
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
    let home = support::tempdir();
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
    let home = support::tempdir();
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
    let home = support::tempdir();
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
    let home = support::tempdir();
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
    let home = support::tempdir();
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

    let script = repo_file("scripts/install.sh");
    let home = support::tempdir();
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
  __hold-daemon-update-boundary)
    [ "$2" = --initial-executable ] || exit 60
    [ "$4" = --replacement-executable ] || exit 61
    [ "$3" = "$CANDIDATE_EXPECTED_TARGET" ] || exit 62
    [ "$5" = "$CANDIDATE_EXPECTED_TARGET" ] || exit 63
    printf 'codex-switch-global-pace daemon update boundary ready running=false service_installed=false\n'
    phase=stopped
    while IFS= read -r command; do
      case "$phase:$command" in
        stopped:uninstall)
          phase=uninstall
          printf 'codex-switch-global-pace daemon update boundary uninstall state ready\n'
          ;;
        uninstall:finish)
          phase=confirmed
          printf 'codex-switch-global-pace daemon update boundary final state confirmed\n'
          ;;
        confirmed:release)
          printf 'codex-switch-global-pace daemon update boundary lifecycle authority released\n'
          exit 0
          ;;
        *) exit 64 ;;
      esac
    done
    exit 65
    ;;
  __installer-file-op)
    exec "$REAL_INSTALLER_HELPER" "$@"
    ;;
  daemon)
    [ "$2" = uninstall ] || exit 66
    [ "$3" = --expected-executable ] || exit 67
    [ "$4" = "$CANDIDATE_EXPECTED_TARGET" ] || exit 68
    [ "${5:-}" = --check-owner ] || exit 69
    ;;
  *) exit 70 ;;
esac
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&candidate).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&candidate, permissions).unwrap();
    let harness = format!(
        "{}\nSYSTEM_INSTALL=false\nINSTALL_DIR=\"$USER_INSTALL_DIR\"\nINSTALL_DEST=\"$INSTALL_DIR/$BINARY_NAME\"\nSYSTEM_INSTALL_DIR=\"$TEST_SYSTEM_DIR\"\nLEGACY_BIN=\"$SYSTEM_INSTALL_DIR/$BINARY_NAME\"\nSYSTEM_INSTALL_MARKER=\"$SYSTEM_INSTALL_DIR/.codex-switch-global-pace-system-install-v1\"\nOS=\"$TEST_OS\"\nTMP_DIR=\"$HOME/uninstall-tmp\"\nmkdir -p \"$TMP_DIR\"\nINSTALL_STAGE=\nINSTALL_BACKUP=\nINSTALL_WITH_SUDO=false\nUPDATE_LOCK_PID_8=\nUPDATE_LOCK_PID_9=\nUPDATE_LOCK_ERROR=\nCANDIDATE_BIN=\"$CANDIDATE\"\nexport CANDIDATE_EXPECTED_TARGET=\"$LEGACY_BIN\"\nrun_uninstall\n",
        unix_uninstall_harness(&script)
    );
    let output = unix_installer_test_command()
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

    let script = repo_file("scripts/install.sh");
    let home = support::tempdir();
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
  "__hold-daemon-update-boundary --initial-executable")
    [ "$4" = --replacement-executable ] || exit 61
    [ "$3" = "$CANDIDATE_EXPECTED_TARGET" ] || exit 62
    [ "$5" = "$CANDIDATE_EXPECTED_TARGET" ] || exit 63
    [ -f "$LOCK_HELD" ] || exit 64
    printf false > "$DAEMON_STATE"
    rm -f "$DAEMON_PIDFILE"
    printf 'codex-switch-global-pace daemon update boundary ready running=true service_installed=false\n'
    phase=stopped
    while IFS= read -r command; do
      case "$phase:$command" in
        stopped:uninstall)
          phase=uninstall
          printf 'codex-switch-global-pace daemon update boundary uninstall state ready\n'
          ;;
        uninstall:finish)
          phase=confirmed
          printf 'codex-switch-global-pace daemon update boundary final state confirmed\n'
          ;;
        confirmed:release)
          printf 'codex-switch-global-pace daemon update boundary lifecycle authority released\n'
          exit 0
          ;;
        *) exit 65 ;;
      esac
    done
    exit 66
    ;;
  "__installer-file-op "*)
    exec "$REAL_INSTALLER_HELPER" "$@"
    ;;
  "daemon uninstall")
    [ -f "$LOCK_HELD" ] || exit 67
    [ "$3" = --expected-executable ] || exit 68
    [ "$4" = "$CANDIDATE_EXPECTED_TARGET" ] || exit 69
    [ "${5:-}" = --check-owner ] || exit 70
    ;;
  *) exit 71 ;;
esac
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&candidate).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&candidate, permissions).unwrap();
    let harness = format!(
        "{}\nSYSTEM_INSTALL=false\nINSTALL_DIR=\"$USER_INSTALL_DIR\"\nINSTALL_DEST=\"$INSTALL_DIR/$BINARY_NAME\"\nOS=\"$TEST_OS\"\nTMP_DIR=\"$HOME/uninstall-tmp\"\nmkdir -p \"$TMP_DIR\"\nINSTALL_STAGE=\nINSTALL_BACKUP=\nINSTALL_WITH_SUDO=false\nUPDATE_LOCK_PID_8=\nUPDATE_LOCK_PID_9=\nUPDATE_LOCK_ERROR=\nCANDIDATE_BIN=\"$CANDIDATE\"\nexport CANDIDATE_EXPECTED_TARGET=\"$INSTALL_DEST\"\nrun_uninstall\n",
        unix_uninstall_harness(&script)
    );
    let held = home.path().join("lock-held");
    let output = unix_installer_test_command()
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
fn unix_daemon_upgrade_boundary_restores_a_running_service() {
    use std::os::unix::fs::PermissionsExt;

    let script = repo_file("scripts/install.sh");
    let definitions = script.split("# Parse arguments").next().unwrap();
    let dir = support::tempdir();
    let binary = dir.path().join("daemon-fixture");
    let state = dir.path().join("state");
    fs::write(&state, "true").unwrap();
    fs::write(
        &binary,
        r#"#!/bin/sh
case "$1" in
  __hold-daemon-update-boundary)
    [ "$2" = --initial-executable ] || exit 60
    [ "$3" = "$0" ] || exit 61
    [ "$4" = --replacement-executable ] || exit 62
    [ "$5" = "$0" ] || exit 63
    printf false > "$DAEMON_FIXTURE_STATE"
    printf 'codex-switch-global-pace daemon update boundary ready running=true service_installed=true\n'
    IFS= read -r command
    [ "$command" = rollback ] || exit 64
    printf true > "$DAEMON_FIXTURE_STATE"
    printf 'codex-switch-global-pace daemon update boundary old state restored\n'
    ;;
  *) exit 64 ;;
esac
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&binary).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&binary, permissions).unwrap();
    let harness = format!(
        "{definitions}\nTMP_DIR=\"$(mktemp -d)\"\ntrap 'rm -rf \"$TMP_DIR\"' EXIT\nDAEMON_BOUNDARY_PID=\nDAEMON_BOUNDARY_ACTIVE=false\nDAEMON_BOUNDARY_ROLLBACK_SAFE=false\nDAEMON_STATE_CAPTURED=false\nstart_daemon_update_boundary \"$BIN\" \"$BIN\" \"$BIN\"\n[ \"$DAEMON_WAS_RUNNING\" = true ]\n[ \"$DAEMON_SERVICE_INSTALLED\" = true ]\n[ \"$(cat \"$DAEMON_FIXTURE_STATE\")\" = false ]\nrestore_daemon_update_boundary_old_state\n[ \"$DAEMON_BOUNDARY_ACTIVE\" = false ]\n[ \"$(cat \"$DAEMON_FIXTURE_STATE\")\" = true ]\nprintf 'transaction-ok\\n'\n"
    );
    let output = unix_installer_test_command()
        .args(["-c", &harness])
        .env("BIN", &binary)
        .env("DAEMON_FIXTURE_STATE", &state)
        .env("TMPDIR", dir.path())
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
    let home = support::tempdir();
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
        "profile_file=\"${HOME}/.config/fish/config.fish\"",
        "# >>> codex-switch-global-pace PATH >>>",
        "# <<< codex-switch-global-pace PATH <<<",
        "prepare_managed_path_removals",
        "commit_managed_path_changes",
        "rollback_managed_path_changes",
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
        "commit_managed_path_changes() {",
        "rollback_managed_path_changes() {",
        "resolve_path_target() (",
        "file_identity() (",
        "while [ -L \"$profile_target\" ]",
        "SYMLINK_RESOLUTION_MAX_HOPS",
        "link_target=\"$(readlink \"$profile_target\")\"",
        "cd -P \"$(dirname \"$profile_target\")\" && pwd -P",
        "profile_stage=\"${profile_target}.${BINARY_NAME}.install\"",
        "capture_installer_file_copy \\",
        "current_target=\"$(resolve_path_target \"$logical\")\"",
        "capture_installer_file_token \"$current_target\" false current_identity",
        "exchange_installer_files \\",
        "move_installer_file_noreplace \"$stage\" \"$target\"",
        "PATH_TRANSACTION_IDENTITY",
        "PATH_TRANSACTION_STAGE_TOKEN",
        "PATH_TRANSACTION_COMMITTED_IDENTITY",
        "capture_installer_file_copy \\",
        "the exact displaced original remains at ${stage}",
        "remove_installer_file_owned \"$stage\"",
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

#[test]
fn unix_installer_bounds_symlink_resolution_and_binds_recursive_temp_cleanup() {
    let script = repo_file("scripts/install.sh");
    for required in [
        "SYMLINK_RESOLUTION_MAX_HOPS=40",
        "[ \"$link_hops\" -le \"$SYMLINK_RESOLUTION_MAX_HOPS\" ]",
        "cleanup_installer_temp_directory() {",
        "TMP_DIR_PARENT_IDENTITY",
        "TMP_DIR_IDENTITY",
        "temporary directory identity changed; preserved",
        "rm -rf -- \"$TMP_DIR\"",
        "local original_status=$? cleanup_status=0",
        "Installer EXIT cleanup failed",
    ] {
        assert!(
            script.contains(required),
            "missing bounded Unix cleanup contract `{required}`"
        );
    }
    assert_eq!(script.matches("rm -rf -- \"$TMP_DIR\"").count(), 1);
}

#[test]
fn unix_installer_routes_privileged_file_operations_through_one_hidden_boundary() {
    let script = repo_file("scripts/install.sh");
    let helper = script
        .split("run_installer_file_op() {")
        .nth(1)
        .and_then(|section| section.split("installer_file_token() {").next())
        .expect("Unix installer file-op adapter");
    assert!(helper.contains("sudo \"$CANDIDATE_BIN\" __installer-file-op \"$@\""));
    assert!(helper.contains("\"$CANDIDATE_BIN\" __installer-file-op \"$@\""));

    let owned_removal = script
        .split("remove_installer_file_owned() {")
        .nth(1)
        .and_then(|section| section.split("file_token_digest() {").next())
        .expect("Unix installer token-bound removal adapter");
    assert!(owned_removal.contains("removed-namespace-durability-unconfirmed)"));
    assert!(owned_removal.contains(
        "the exact owned file at ${source} was removed, but parent-directory namespace durability was not confirmed"
    ));
    assert!(owned_removal.contains("return 1"));
    assert!(!owned_removal.contains("warn \"Removed the exact owned file"));

    let install = script
        .split("stage_and_replace_binary() {")
        .nth(1)
        .and_then(|section| section.split("commit_installed_binary() {").next())
        .unwrap();
    for required in [
        "capture_installer_file_copy \\",
        "exchange_installer_files \\",
        "move_installer_file_noreplace \\",
        "\"$INSTALL_WITH_SUDO\"",
    ] {
        assert!(install.contains(required), "install omits `{required}`");
    }

    let uninstall = script
        .split("begin_uninstall_file_transaction() {")
        .nth(1)
        .and_then(|section| section.split("hold_uninstall_binary_for_commit() {").next())
        .unwrap();
    for required in [
        "capture_installer_file_copy \\",
        "capture_empty_installer_file \\",
        "remove_installer_file_owned \\",
        "\"$UNINSTALL_WITH_SUDO\"",
    ] {
        assert!(uninstall.contains(required), "uninstall omits `{required}`");
    }
}

#[cfg(unix)]
#[test]
fn unix_symlink_cycle_and_temp_identity_checks_execute_fail_closed() {
    use std::os::unix::fs::symlink;
    use std::process::Command;

    let script = repo_file("scripts/install.sh");
    let definitions = script.split("# Parse arguments").next().unwrap();
    let directory = support::tempdir();

    let first = directory.path().join("first-link");
    let second = directory.path().join("second-link");
    symlink(&second, &first).unwrap();
    symlink(&first, &second).unwrap();
    let cycle_harness = directory.path().join("cycle.sh");
    fs::write(
        &cycle_harness,
        format!("{definitions}\nSYMLINK_RESOLUTION_MAX_HOPS=4\nresolve_path_target \"$1\"\n"),
    )
    .unwrap();
    let cycle = Command::new("bash")
        .arg(&cycle_harness)
        .arg(&first)
        .env("HOME", directory.path())
        .output()
        .unwrap();
    assert!(!cycle.status.success());
    assert!(
        String::from_utf8_lossy(&cycle.stderr).contains("exceeded 4 hops"),
        "{}",
        String::from_utf8_lossy(&cycle.stderr)
    );

    let cleanup_harness = directory.path().join("cleanup.sh");
    fs::write(
        &cleanup_harness,
        format!(
            r#"{definitions}
TMP_DIR="$1/owned"
mkdir -p "$TMP_DIR/nested"
printf payload > "$TMP_DIR/nested/file"
TMP_DIR_PARENT="$(dirname "$TMP_DIR")"
TMP_DIR_PARENT_IDENTITY="$(file_identity "$TMP_DIR_PARENT")"
TMP_DIR_IDENTITY="$(file_identity "$TMP_DIR")"
TMP_CLEANUP_ERROR=""
cleanup_installer_temp_directory
[ ! -e "$TMP_DIR" ]

TMP_DIR="$1/replaced"
mkdir "$TMP_DIR"
TMP_DIR_PARENT="$(dirname "$TMP_DIR")"
TMP_DIR_PARENT_IDENTITY="$(file_identity "$TMP_DIR_PARENT")"
TMP_DIR_IDENTITY="$(file_identity "$TMP_DIR")"
mv "$TMP_DIR" "$1/original-held"
mkdir "$TMP_DIR"
if cleanup_installer_temp_directory; then
  exit 21
fi
[ -d "$TMP_DIR" ]
"#
        ),
    )
    .unwrap();
    let cleanup = Command::new("bash")
        .arg(&cleanup_harness)
        .arg(directory.path())
        .env("HOME", directory.path())
        .output()
        .unwrap();
    assert!(
        cleanup.status.success(),
        "{}{}",
        String::from_utf8_lossy(&cleanup.stdout),
        String::from_utf8_lossy(&cleanup.stderr)
    );
    assert!(directory.path().join("replaced").is_dir());
}

#[cfg(unix)]
#[test]
fn unix_installer_preserves_multi_level_profile_symlinks() {
    use std::os::unix::fs::symlink;

    let script = repo_file("scripts/install.sh");
    let function_prefix = script
        .split("\nmanaged_path_block_exists() {")
        .next()
        .expect("installer must define managed_path_block_exists");
    let temp = support::tempdir();
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
            "{function_prefix}\nTMP_DIR=\"$(mktemp -d)\"\ntrap 'rm -rf \"$TMP_DIR\"' EXIT\nreset_managed_path_transaction\nprepare_path_block_removal \"$1\"\ncommit_managed_path_changes\n"
        ),
    )
    .unwrap();
    let output = unix_installer_test_command()
        .arg(&harness)
        .arg(&profile_link)
        .env("TMPDIR", temp.path())
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
fn unix_path_addition_uses_the_shared_transaction_and_rolls_back_exactly() {
    let script = repo_file("scripts/install.sh");
    let definitions = script.split("# Parse arguments").next().unwrap();
    let home = support::tempdir();
    let profile = home.path().join(".profile");
    fs::write(&profile, "export KEEP=1\n").unwrap();
    let harness = format!(
        r#"{definitions}
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
PLATFORM=linux
reset_managed_path_transaction
prepare_managed_path_addition
commit_managed_path_changes
grep -F "$PATH_BLOCK_BEGIN" "$HOME/.profile" >/dev/null
rollback_managed_path_changes
[ "$(cat "$HOME/.profile")" = 'export KEEP=1' ]
[ ! -e "$HOME/.profile.$BINARY_NAME.install" ]
"#
    );
    let output = unix_installer_test_command()
        .args(["-c", &harness])
        .env("HOME", home.path())
        .env("TMPDIR", home.path())
        .env("SHELL", "/bin/bash")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(profile).unwrap(), "export KEEP=1\n");
}

#[cfg(unix)]
#[test]
fn unix_path_rollback_preserves_a_fixed_original_when_the_profile_is_replaced() {
    let script = repo_file("scripts/install.sh");
    let definitions = script.split("# Parse arguments").next().unwrap();
    let home = support::tempdir();
    let profile = home.path().join(".profile");
    let recovery = home
        .path()
        .join(".profile.codex-switch-global-pace.install");
    fs::write(&profile, "export KEEP=1\n").unwrap();
    let harness = format!(
        r#"{definitions}
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
PLATFORM=linux
reset_managed_path_transaction
prepare_managed_path_addition
commit_managed_path_changes
recovery_path="${{PATH_TRANSACTION_STAGE[0]}}"
printf '%s\n' 'foreign profile' > "$HOME/replacement-profile"
mv -f "$HOME/replacement-profile" "$HOME/.profile"
if rollback_managed_path_changes; then
  exit 1
fi
[ "$PATH_TRANSACTION_ERROR" = "exact displaced profile remains at $recovery_path" ]
[ "$(cat "$HOME/.profile")" = 'foreign profile' ]
[ "$(cat "$recovery_path")" = 'export KEEP=1' ]
"#
    );
    let output = unix_installer_test_command()
        .args(["-c", &harness])
        .env("HOME", home.path())
        .env("TMPDIR", home.path())
        .env("SHELL", "/bin/bash")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(profile).unwrap(), "foreign profile\n");
    assert_eq!(fs::read_to_string(recovery).unwrap(), "export KEEP=1\n");
}

#[cfg(unix)]
#[test]
fn unix_installer_aborts_if_profile_symlink_changes_during_rewrite() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let script = repo_file("scripts/install.sh");
    let function_prefix = script
        .split("\nmanaged_path_block_exists() {")
        .next()
        .expect("installer must define managed_path_block_exists");
    let temp = support::tempdir();
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
    let race_triggered = temp.path().join("profile-link-race-triggered");
    fs::write(
        &fake_cp,
        "#!/bin/sh\nset -eu\nrm -f \"$PROFILE_LINK\"\nln -s \"$REPLACEMENT_PROFILE\" \"$PROFILE_LINK\"\nprintf triggered > \"$RACE_TRIGGERED\"\nexec /bin/cp \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_cp, fs::Permissions::from_mode(0o755)).unwrap();

    let harness = temp.path().join("remove-path-block.sh");
    fs::write(
        &harness,
        format!(
            "{function_prefix}\nTMP_DIR=\"$(mktemp -d)\"\ntrap 'rm -rf \"$TMP_DIR\"' EXIT\nreset_managed_path_transaction\nprepare_path_block_removal \"$1\"\ncommit_managed_path_changes\n"
        ),
    )
    .unwrap();
    let output = unix_installer_test_command()
        .arg(&harness)
        .arg(&profile_link)
        .env("TMPDIR", temp.path())
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .env("PROFILE_LINK", &profile_link)
        .env("REPLACEMENT_PROFILE", &replacement_profile)
        .env("RACE_TRIGGERED", &race_triggered)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "a changed profile symlink must abort the rewrite"
    );
    assert_eq!(fs::read_to_string(race_triggered).unwrap(), "triggered");
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

    let script = repo_file("scripts/install.sh");
    let function_prefix = script
        .split("\nmanaged_path_block_exists() {")
        .next()
        .unwrap();
    let temp = support::tempdir();
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
    let race_triggered = temp.path().join("profile-parent-race-triggered");
    fs::write(
        &fake_cp,
        "#!/bin/sh\nset -eu\nrm -f \"$CURRENT_LINK\"\nln -s \"$NEW_DIR\" \"$CURRENT_LINK\"\nprintf triggered > \"$RACE_TRIGGERED\"\nexec /bin/cp \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_cp, fs::Permissions::from_mode(0o755)).unwrap();
    let harness = temp.path().join("remove-path-block.sh");
    fs::write(
        &harness,
        format!(
            "{function_prefix}\nTMP_DIR=\"$(mktemp -d)\"\ntrap 'rm -rf \"$TMP_DIR\"' EXIT\nreset_managed_path_transaction\nprepare_path_block_removal \"$1\"\ncommit_managed_path_changes\n"
        ),
    )
    .unwrap();

    let output = unix_installer_test_command()
        .arg(&harness)
        .arg(current.join("profile"))
        .env("TMPDIR", temp.path())
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .env("CURRENT_LINK", &current)
        .env("NEW_DIR", &dir_b)
        .env("RACE_TRIGGERED", &race_triggered)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(race_triggered).unwrap(), "triggered");
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

    let script = repo_file("scripts/install.sh");
    let function_prefix = script
        .split("\nmanaged_path_block_exists() {")
        .next()
        .unwrap();
    let temp = support::tempdir();
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
            "{function_prefix}\nTMP_DIR=\"$(mktemp -d)\"\ntrap 'rm -rf \"$TMP_DIR\"' EXIT\nreset_managed_path_transaction\nprepare_path_block_removal \"$1\"\ncommit_managed_path_changes\n"
        ),
    )
    .unwrap();

    let output = unix_installer_test_command()
        .arg(&harness)
        .arg(&profile)
        .env("TMPDIR", temp.path())
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
        "capture_empty_installer_file \\",
        "\"$SYSTEM_INSTALL_MARKER\" \"$INSTALL_WITH_SUDO\" SYSTEM_MARKER_CREATED_TOKEN",
        "commit_held_legacy_install",
        "remove_installer_file_owned \\",
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
        "remove_installer_file_owned \\",
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
    assert!(script.contains("Set-ExactUserPathTransition"));
}

#[test]
fn windows_user_path_updates_use_one_exact_compare_and_swap_contract() {
    let script = repo_file("scripts/install.ps1");
    let registry = repo_file("src/installer_registry.rs");

    for required in [
        "function Set-ExactUserPathTransition",
        "function Restore-ExactUserPathTransition",
        "function Invoke-ExactProcessPathTransition",
        "function Restore-ExactProcessPathTransition",
        "Test-ProcessPathSnapshotEqual",
        "[System.StringComparer]::OrdinalIgnoreCase.Equals",
        "[System.StringSplitOptions]::None",
        "SetEnvironmentVariable(\"Path\", $RequestedValue, \"Process\")",
        "Windows environment notification failed",
    ] {
        assert!(
            script.contains(required),
            "missing User PATH CAS step `{required}`"
        );
    }
    for required in [
        "RegOpenKeyTransactedW",
        "RegQueryValueExW",
        "RegSetValueExW",
        "transaction.commit(\"registry\")?",
        "SendMessageTimeoutW",
        "WM_SETTINGCHANGE",
        "ENVIRONMENT_NOTIFICATION",
        "path-transition|{}|{notification}",
        "serde(deny_unknown_fields)",
        "before != after || before != final_path",
    ] {
        assert!(
            registry.contains(required),
            "missing native User PATH boundary `{required}`"
        );
    }
    assert!(!script.contains("SetEnvironmentVariable(\"Path\", $Requested, \"User\")"));
    assert!(!script.contains("GetEnvironmentVariable(\"Path\", \"User\")"));
    assert_eq!(
        script.matches("Set-ExactUserPathTransition `").count(),
        2,
        "install and uninstall must share the exact mutation helper"
    );
    assert_eq!(
        script.matches("Restore-ExactUserPathTransition `").count(),
        2,
        "install and uninstall must share the exact rollback helper"
    );
    assert!(script.starts_with("# codex-switch-global-pace installer"));
    assert!(script.contains("\n& {\n$ErrorActionPreference = \"Stop\""));
    assert!(
        script
            .lines()
            .all(|line| !line.trim_start().starts_with("exit ")),
        "the irm | iex installer must propagate errors without exiting its caller host"
    );
}

#[cfg(windows)]
#[test]
fn windows_process_path_transform_preserves_unrelated_raw_segments() {
    use std::process::Command;

    let installer = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/install.ps1");
    let command = r#"
$Source = [IO.File]::ReadAllText($env:INSTALLER_UNDER_TEST)
$Entrypoint = $Source.IndexOf('# Detect architecture', [StringComparison]::Ordinal)
if ($Entrypoint -lt 0) { throw 'entrypoint marker missing' }
$Definitions = $Source.Substring(0, $Entrypoint)
$Definitions = ([regex]'(?m)^& \{\r?\n').Replace($Definitions, '', 1)
Invoke-Expression $Definitions
function Present([string]$Value) { [pscustomobject]@{ Present = $true; Value = $Value } }
$Empty = Get-RequestedProcessPathSnapshot -Current (Present '') -Action add -Entry 'Entry'
$Normal = Get-RequestedProcessPathSnapshot -Current (Present 'A') -Action add -Entry 'Entry'
$Trailing = Get-RequestedProcessPathSnapshot -Current (Present 'A;') -Action add -Entry 'Entry'
$Removed = Get-RequestedProcessPathSnapshot -Current (Present 'A;;B') -Action remove -Entry 'a'
$RemovedOnly = Get-RequestedProcessPathSnapshot -Current (Present 'A') -Action remove -Entry 'a'
$Existing = Get-RequestedProcessPathSnapshot -Current (Present 'C:\Tool;;B') -Action add -Entry 'c:\tool'
if ($Empty.Value -cne 'Entry' -or
    $Normal.Value -cne 'A;Entry' -or
    $Trailing.Value -cne 'A;;Entry' -or
    $Removed.Value -cne ';B' -or
    $RemovedOnly.Present -or
    $Existing.Value -cne 'C:\Tool;;B') {
    throw 'process PATH transform did not preserve the explicit empty-segment policy'
}
'process-path-transform-ok'
"#;
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", command])
        .env("INSTALLER_UNDER_TEST", installer)
        .output()
        .unwrap();
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{diagnostic}");
    assert!(
        diagnostic.contains("process-path-transform-ok"),
        "{diagnostic}"
    );
}

#[cfg(windows)]
#[test]
fn windows_installer_never_exits_the_irm_iex_caller_host() {
    use std::process::Command;

    let installer = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/install.ps1");
    let command = r#"$ErrorActionPreference = 'Continue'; $Repo = 'caller-sentinel'; function New-InstallerRecoveryPath { 'caller-sentinel' }; $Source = [IO.File]::ReadAllText($env:INSTALLER_UNDER_TEST); try { Invoke-Expression $Source } catch { 'installer-error-caught' }; if ($ErrorActionPreference -cne 'Continue') { throw 'caller ErrorActionPreference leaked' }; if ($Repo -cne 'caller-sentinel') { throw 'caller variable leaked' }; if ((New-InstallerRecoveryPath) -cne 'caller-sentinel') { throw 'caller function leaked' }; 'caller-host-alive'"#;
    let iex = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", command])
        .env("INSTALLER_UNDER_TEST", &installer)
        .env_remove("CS_VERSION")
        .env_remove("CS_DEV")
        .env_remove("CS_UNINSTALL")
        .output()
        .unwrap();
    let iex_output = format!(
        "{}{}",
        String::from_utf8_lossy(&iex.stdout),
        String::from_utf8_lossy(&iex.stderr)
    );
    assert!(iex.status.success(), "{iex_output}");
    assert!(
        iex_output.contains("installer-error-caught"),
        "{iex_output}"
    );
    assert!(iex_output.contains("caller-host-alive"), "{iex_output}");

    let standalone = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&installer)
        .env_remove("CS_VERSION")
        .env_remove("CS_DEV")
        .env_remove("CS_UNINSTALL")
        .output()
        .unwrap();
    assert!(
        !standalone.status.success(),
        "a standalone installer failure must produce a failing process status"
    );
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
        "$CurrentRecoveryPattern = '^\\.' + [regex]::Escape($Stem) + '\\.(displaced|failed)-[0-9a-f]{32}\\.exe$'",
        "[System.Security.Cryptography.RandomNumberGenerator]::Create()",
        "$RecoveryNameCollisionLimit",
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
    assert!(script.contains("[Guid]::NewGuid().ToString('N')"));
    assert!(!script.contains("Move-Item -LiteralPath $InstalledBin -Destination $BackupBin"));
    assert!(
        !script
            .contains("Remove-Item -LiteralPath $StagedBin -Force -ErrorAction SilentlyContinue")
    );
}

#[cfg(windows)]
#[test]
fn windows_installer_transaction_helpers_execute_fail_closed() {
    use std::process::{Command, Output};

    fn helper(candidate: &Path, arguments: &[&str], paths: &[&Path]) -> Output {
        let mut command = Command::new(candidate);
        command.arg("__installer-file-op");
        for argument in arguments {
            command.arg(argument);
        }
        for path in paths {
            command.arg(path);
        }
        command.output().unwrap()
    }

    fn token(candidate: &Path, path: &Path) -> String {
        let output = helper(candidate, &["token", "--source"], &[path]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn operation_diagnostic(output: &Output) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    let candidate = Path::new(env!("CARGO_BIN_EXE_codex-switch-global-pace"));
    let directory = support::tempdir();

    let source = directory.path().join("source.exe");
    let staged = directory.path().join("staged.exe");
    fs::write(&source, b"verified candidate").unwrap();
    let source_token = token(candidate, &source);
    let copy = helper(
        candidate,
        &[
            "copy-exclusive",
            "--source",
            source.to_str().unwrap(),
            "--destination",
            staged.to_str().unwrap(),
            "--expected-token",
            &source_token,
        ],
        &[],
    );
    assert!(copy.status.success(), "{}", operation_diagnostic(&copy));
    let staged_token = String::from_utf8(copy.stdout)
        .unwrap()
        .trim()
        .strip_prefix("created|")
        .expect("explicit creation outcome")
        .to_string();
    assert_eq!(fs::read(&staged).unwrap(), b"verified candidate");
    assert_eq!(token(candidate, &staged), staged_token);

    let occupied = directory.path().join("occupied.exe");
    fs::write(&occupied, b"foreign writer").unwrap();
    let refused_copy = helper(
        candidate,
        &[
            "copy-exclusive",
            "--source",
            source.to_str().unwrap(),
            "--destination",
            occupied.to_str().unwrap(),
            "--expected-token",
            &source_token,
        ],
        &[],
    );
    assert!(!refused_copy.status.success());
    assert_eq!(fs::read(&occupied).unwrap(), b"foreign writer");

    let move_source = directory.path().join("move-source.exe");
    fs::write(&move_source, b"move candidate").unwrap();
    let move_token = token(candidate, &move_source);
    let refused_move = helper(
        candidate,
        &[
            "move-noreplace",
            "--source",
            move_source.to_str().unwrap(),
            "--destination",
            occupied.to_str().unwrap(),
            "--expected-token",
            &move_token,
        ],
        &[],
    );
    assert!(!refused_move.status.success());
    assert_eq!(fs::read(&move_source).unwrap(), b"move candidate");
    assert_eq!(fs::read(&occupied).unwrap(), b"foreign writer");

    let installed = directory.path().join("installed.exe");
    let replacement = directory.path().join("replacement.exe");
    let displaced = directory.path().join(".installed.displaced-test.exe");
    let failed = directory.path().join(".installed.failed-test.exe");
    fs::write(&installed, b"previous").unwrap();
    fs::write(&replacement, b"candidate").unwrap();
    let previous_token = token(candidate, &installed);
    let replacement_token = token(candidate, &replacement);
    let publication = helper(
        candidate,
        &[
            "replace-with-displaced",
            "--source",
            replacement.to_str().unwrap(),
            "--destination",
            installed.to_str().unwrap(),
            "--displaced",
            displaced.to_str().unwrap(),
            "--expected-token",
            &replacement_token,
            "--expected-destination-token",
            &previous_token,
        ],
        &[],
    );
    assert!(
        publication.status.success(),
        "{}",
        operation_diagnostic(&publication)
    );
    assert_eq!(
        String::from_utf8_lossy(&publication.stdout).trim(),
        "replaced"
    );
    assert_eq!(fs::read(&installed).unwrap(), b"candidate");
    assert_eq!(fs::read(&displaced).unwrap(), b"previous");
    assert!(!replacement.exists());

    let rollback = helper(
        candidate,
        &[
            "replace-with-displaced",
            "--source",
            displaced.to_str().unwrap(),
            "--destination",
            installed.to_str().unwrap(),
            "--displaced",
            failed.to_str().unwrap(),
            "--expected-token",
            &previous_token,
            "--expected-destination-token",
            &replacement_token,
        ],
        &[],
    );
    assert!(
        rollback.status.success(),
        "{}",
        operation_diagnostic(&rollback)
    );
    assert_eq!(fs::read(&installed).unwrap(), b"previous");
    assert_eq!(fs::read(&failed).unwrap(), b"candidate");
    assert!(!displaced.exists());

    let wrong_removal = helper(
        candidate,
        &[
            "remove-owned",
            "--source",
            installed.to_str().unwrap(),
            "--expected-token",
            &replacement_token,
        ],
        &[],
    );
    assert!(!wrong_removal.status.success());
    assert_eq!(fs::read(&installed).unwrap(), b"previous");

    for (path, expected) in [(&installed, &previous_token), (&failed, &replacement_token)] {
        let removal = helper(
            candidate,
            &[
                "remove-owned",
                "--source",
                path.to_str().unwrap(),
                "--expected-token",
                expected,
            ],
            &[],
        );
        assert!(
            removal.status.success(),
            "{}",
            operation_diagnostic(&removal)
        );
        assert_eq!(String::from_utf8_lossy(&removal.stdout).trim(), "removed");
        assert!(!path.exists());
    }

    let help = Command::new(candidate).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(!String::from_utf8_lossy(&help.stdout).contains("__installer-file-op"));
}

#[test]
fn windows_installer_preserves_a_running_daemon_across_upgrade() {
    let script = repo_file("scripts/install.ps1");
    let install_transaction = script
        .split("# Stage the verified candidate")
        .nth(1)
        .expect("Windows install transaction");

    for required in [
        "function Start-DaemonLifecycleHolder",
        "__hold-daemon-update-boundary --initial-executable",
        "$StartInfo.RedirectStandardInput = $true",
        "$StartInfo.RedirectStandardOutput = $true",
        "$StartInfo.RedirectStandardError = $false",
        "$DaemonBoundaryPrefix ready running=true service_installed=true",
        "$DaemonBoundaryPrefix ready running=false service_installed=false",
        "function Invoke-DaemonLifecycleCommand",
        "replacement daemon state was rejected; exact daemon absence was retained for rollback",
        "daemon lifecycle holder PID $($Holder.Process.Id) did not exit after stdin EOF",
        "$InstallLifecycleHolder = Start-DaemonLifecycleHolder",
        "$DaemonWasRunning = $InstallLifecycleHolder.Running",
        "$DaemonServiceInstalled = $InstallLifecycleHolder.ServiceInstalled",
        "Invoke-DaemonLifecycleCommand -Holder $InstallLifecycleHolder -Command \"new\"",
        "-Command \"rollback\"",
        "-Command \"finish\"",
        "-Command \"release\"",
        "$DaemonSafeForBinaryRollback",
        "automatic binary rollback was refused",
        ".$BinaryStem.install.exe",
        ".$BinaryStem.rollback.exe",
        "New-InstallerRecoveryPath -Directory $InstallDir -Stem $BinaryStem -Role \"failed\"",
        "$AmbiguousBinaryState",
        "function Remove-InstallerArtifactIfOwned",
        "function Invoke-ClassifiedInstallerReplace",
        "$Publication = Invoke-ClassifiedInstallerReplace",
        "$Rollback = Invoke-ClassifiedInstallerReplace",
        "$InstallPostCommitErrors",
    ] {
        assert!(
            script.contains(required),
            "Windows installer must contain the daemon-upgrade safeguard `{required}`"
        );
    }
    assert_before(
        &script,
        "$CandidateVersionOutput = & $CandidateBin --version",
        "$StagedBin = Join-Path $InstallDir",
    );
    assert_before(
        install_transaction,
        "$StagedVersionOutput = & $StagedBin --version",
        "$InstallLifecycleHolder = Start-DaemonLifecycleHolder",
    );
    assert_before(
        install_transaction,
        "$InstallLifecycleHolder = Start-DaemonLifecycleHolder",
        "$Publication = Invoke-ClassifiedInstallerReplace",
    );
    assert!(
        !script.contains("function Stop-And-ConfirmDaemonAbsent")
            && !script.contains("function Get-CheckedDaemonStatus"),
        "split status/stop helpers must not survive the persistent holder transition"
    );

    let rollback_start = install_transaction
        .find("if ($null -ne $InstallError)")
        .expect("Windows installer must have an explicit rollback branch");
    let successful_transaction = &install_transaction[..rollback_start];
    assert_before(
        successful_transaction,
        "$InstalledVersionLine =",
        "Invoke-DaemonLifecycleCommand -Holder $InstallLifecycleHolder -Command \"new\"",
    );
    assert_before(
        successful_transaction,
        "Invoke-DaemonLifecycleCommand -Holder $InstallLifecycleHolder -Command \"new\"",
        "if ($InstallPostCommitErrors.Count -eq 0 -and $OldBinaryBackedUp) {",
    );
    assert_before(
        successful_transaction,
        "Invoke-DaemonLifecycleCommand -Holder $InstallLifecycleHolder -Command \"finish\"",
        "if ($InstallPostCommitErrors.Count -eq 0 -and $OldBinaryBackedUp) {",
    );
    assert_before(
        successful_transaction,
        "if ($InstallPostCommitErrors.Count -eq 0 -and $OldBinaryBackedUp) {",
        "Invoke-DaemonLifecycleCommand -Holder $InstallLifecycleHolder -Command \"release\"",
    );
    let rollback = &install_transaction[rollback_start..];
    assert_before(
        rollback,
        "$Rollback = Invoke-ClassifiedInstallerReplace",
        "-Command \"rollback\"",
    );
    assert_before(
        rollback,
        "Restore-ExactUserPathTransition `",
        "-Command \"rollback\"",
    );
    assert!(!script.contains("$Holder.Process.Kill()"));
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
        "$UpdateLockReleaseExitTimeoutMilliseconds = 10000",
        "$LockProcess.WaitForExit($UpdateLockReleaseExitTimeoutMilliseconds)",
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
        "$OriginalUserPathSnapshot = Invoke-RequiredInstallerFileOperation",
    );
    assert_before(
        install_transaction,
        "$UpdateLockHolder = Start-UpdateLockHolder",
        "$InstallLifecycleHolder = Start-DaemonLifecycleHolder",
    );
    assert_before(
        install_transaction,
        "$InstallLifecycleHolder = Start-DaemonLifecycleHolder",
        "$Publication = Invoke-ClassifiedInstallerReplace",
    );
    assert_before(
        install_transaction,
        "if ($null -ne $InstallError)",
        "Complete-UpdateLockHolder -LockProcess $UpdateLockHolder",
    );
    assert_before(
        install_transaction,
        "if ($OldBinaryBackedUp) {",
        "Complete-UpdateLockHolder -LockProcess $UpdateLockHolder",
    );
    let transaction_finally = install_transaction
        .find("} finally {\n    $LifecycleReleaseError = $null")
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
        "function Invoke-ClassifiedInstallerReplace",
        "function New-InstallerEmptyFileExclusive",
        "function Copy-InstallerFileExclusive",
        "function Remove-InstallerOwnedFile",
        "-Operation \"replace-with-displaced\"",
        "-Operation \"remove-owned\"",
        "function Start-DaemonLifecycleHolder",
        "function Invoke-DaemonLifecycleCommand",
        ".$BinaryStem.uninstall.exe",
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
        "$OriginalBinaryToken",
        "$UninstallHoldToken",
        "$UninstallPlaceholderToken",
        "$OriginalUserPathSnapshot",
        "$OriginalProcessPathSnapshot",
        "$PathMutationAttempted",
        "$ProcessPathMutationAttempted",
        "$UninstallMutationAttempted",
        "$UninstallCommitted",
        "$PostCommitCleanupError",
        "Assert-CandidateServiceOwner",
        "$UninstallLifecycleHolder = Start-DaemonLifecycleHolder",
        "Invoke-DaemonLifecycleCommand -Holder $UninstallLifecycleHolder -Command \"uninstall\"",
        "$Staging = Invoke-ClassifiedInstallerReplace",
        "$Restore = Invoke-ClassifiedInstallerReplace",
        "Remove-InstallerOwnedFile `",
        "-Command \"rollback\"",
        "-Command \"finish\"",
        "-Command \"release\"",
        "The uninstall did not commit, and the exact pre-uninstall binary, PATH, and running state were restored",
        "Uninstall committed, but post-commit cleanup could not be confirmed",
        "Recovery residue path: $UninstallBackupBin",
        "Set-ExactUserPathTransition `",
        "Restore-ExactUserPathTransition `",
        "Complete-UpdateLockHolder -LockProcess $UninstallLockHolder",
    ] {
        assert!(
            uninstall.contains(required),
            "Windows uninstaller must contain locked candidate contract `{required}`"
        );
    }
    assert_before(
        uninstall,
        "$UninstallLockHolder = Start-UpdateLockHolder",
        "$OriginalBinaryToken = if ($InstalledBinaryWasPresent)",
    );
    assert_before(
        uninstall,
        "$UninstallLockHolder = Start-UpdateLockHolder",
        "Assert-CandidateServiceOwner `",
    );
    assert_before(
        uninstall,
        "Assert-CandidateServiceOwner `",
        "$UninstallLifecycleHolder = Start-DaemonLifecycleHolder",
    );
    assert_before(
        uninstall,
        "$UninstallLifecycleHolder = Start-DaemonLifecycleHolder",
        "Set-ExactUserPathTransition `",
    );
    assert_before(
        uninstall,
        "Set-ExactUserPathTransition `",
        "$Staging = Invoke-ClassifiedInstallerReplace",
    );
    assert_before(
        uninstall,
        "$Staging = Invoke-ClassifiedInstallerReplace",
        "Invoke-DaemonLifecycleCommand -Holder $UninstallLifecycleHolder -Command \"uninstall\"",
    );
    assert_before(
        uninstall,
        "Invoke-DaemonLifecycleCommand -Holder $UninstallLifecycleHolder -Command \"uninstall\"",
        "$UninstallCommitted = $true",
    );
    assert_before(
        uninstall,
        "$UninstallCommitted = $true",
        "-Command \"finish\"",
    );
    assert_before(
        uninstall,
        "-Command \"finish\"",
        "Remove-InstallerOwnedFile `",
    );
    assert_before(
        uninstall,
        "Remove-InstallerOwnedFile `",
        "-Command \"release\"",
    );
    assert_before(
        uninstall,
        "-Command \"release\"",
        "Complete-UpdateLockHolder -LockProcess $UninstallLockHolder",
    );
    let rollback = uninstall
        .split("} catch {\n        $UninstallFailure = $_")
        .nth(1)
        .expect("Windows uninstall rollback branch");
    assert_before(
        rollback,
        "$Restore = Invoke-ClassifiedInstallerReplace",
        "Restore-ExactUserPathTransition `",
    );
    assert_before(
        rollback,
        "Restore-ExactUserPathTransition `",
        "-Command \"rollback\"",
    );
    assert!(!uninstall.contains("Remove-Item -LiteralPath $InstalledBin -Force"));
    assert!(!uninstall.contains("daemon install"));
    assert!(!uninstall.contains("Get-ScheduledTask"));
    assert!(!uninstall.contains("schtasks.exe"));
    assert!(!uninstall.contains("& $CandidateBin daemon stop"));
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
        .split("fn install_launchd(executable: &Path, expected_existing_executable: Option<&Path>)")
        .nth(1)
        .and_then(|section| {
            section
                .split("fn start_launchd(expected_executable: &Path)")
                .next()
        })
        .expect("LaunchAgent install implementation");
    assert_before(
        launchd_install,
        "generated LaunchAgent failed plutil validation",
        "was_loaded",
    );
    assert!(
        launchd_install.contains("validate_launchd_definition_owner(")
            && launchd_install.matches("require_service_snapshot(").count() >= 2,
        "LaunchAgent install must prove owner and revalidate its exact snapshot before replacement"
    );
    let systemd_install = service
        .split("fn install_systemd(executable: &Path, expected_existing_executable: Option<&Path>)")
        .nth(1)
        .and_then(|section| {
            section
                .split("fn start_systemd(expected_executable: &Path)")
                .next()
        })
        .expect("systemd install implementation");
    assert_before(
        systemd_install,
        "generated systemd user service failed validation",
        "was_active",
    );
    assert!(
        systemd_install.contains("validate_systemd_definition_owner(")
            && systemd_install.matches("require_service_snapshot(").count() >= 2,
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
        "let preparation = (|| -> Result<()> {",
    );
    assert!(
        task_install.contains("validate_task_scheduler_definition_owner(")
            && task_install.contains("require_task_definition_snapshot(")
            && task_install.contains("transaction_error_with_restoration("),
        "Task Scheduler install must prove owner and guard its exact definition snapshot"
    );
    assert!(service.contains("reload systemd user units after uninstall"));
    assert!(service.contains("systemd service uninstall failed and rollback was incomplete"));
    let service_runtime = service
        .split("#[cfg(test)]\nmod tests")
        .next()
        .expect("service runtime implementation");
    assert!(
        !service_runtime.contains("path.exists()"),
        "service lifecycle must not collapse metadata errors into a missing definition"
    );
    let systemd_uninstall = service
        .split("fn uninstall_systemd(expected_executable: &Path)")
        .nth(1)
        .and_then(|section| section.split("// -- Windows Task Scheduler --").next())
        .expect("systemd uninstall implementation");
    assert!(
        systemd_uninstall
            .contains("let Some(previous) = optional_service_file_snapshot(&path)? else")
    );
    assert_before(
        systemd_uninstall,
        "begin_service_file_removal(&path, &previous)?",
        "reload systemd user units after uninstall",
    );
    assert!(
        !systemd_uninstall.contains("path.exists()"),
        "systemd uninstall must not collapse metadata errors into a missing service"
    );
}

#[test]
fn installer_only_daemon_checks_run_before_normal_initialization() {
    let main = repo_file("src/app.rs");
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
        "SelfUpdateDaemonBoundaryClient::start()",
    );
}

#[test]
fn self_update_daemon_restart_holds_both_lifecycle_authorities_through_commit() {
    let daemon = repo_file("src/daemon/mod.rs");
    let command = repo_file("src/commands/update.rs");
    let pidfile = repo_file("src/daemon/pidfile.rs");
    let service = repo_file("src/daemon/service.rs");
    let transaction = daemon
        .split("pub struct SelfUpdateDaemonRestart")
        .nth(1)
        .and_then(|tail| tail.split("pub(crate) fn print_installer_state").next())
        .expect("self-update daemon restart transaction");

    for required in [
        "initial_executable: std::path::PathBuf",
        "initial_generation: Option<pidfile::DaemonGeneration>",
        "initial_service_snapshot: service::ServiceStateSnapshot",
        "expected_service_snapshot: service::ServiceStateSnapshot",
        "executable: std::path::PathBuf",
        "service_executable: std::path::PathBuf",
        "service_lease: service::ServiceOperationLease",
        "absence_lease: Option<pidfile::DaemonAbsenceLease>",
        "service::capture_service_state_snapshot(&executable, initial_pid)?",
        "classify_initial_launch_mechanism(initial_pid, initial_service_snapshot.manager_pid())?",
        "service::stop_installed_manager_observed_locked(",
        "service::start_installed_locked(&self.initial_executable, &self.service_lease)?",
        "service::start_installed_locked(&previous_service_executable, &self.service_lease)",
        "start_detached_executable_locked(&self.initial_executable, &self.service_lease)?",
        "start_detached_executable_locked(replacement_executable, &self.service_lease)",
        "self.reacquire_absence_after_foreground_contenders(self.initial_generation.clone())",
        "self.record_expected_service_snapshot_after_restart(restarted_pid)?",
        "pub fn verify_final_state(&mut self)",
    ] {
        assert!(
            transaction.contains(required),
            "daemon restart must retain the explicit public executable path through replacement: `{required}`"
        );
    }
    assert!(!transaction.contains("service::start_installed()?"));
    assert!(!transaction.contains("start_detached()?"));
    assert_before(
        transaction,
        "validate_running_daemon_executable(&executable",
        "service::capture_service_state_snapshot(&executable, initial_pid)?",
    );
    assert_before(
        transaction,
        "service::capture_service_state_snapshot(&executable, initial_pid)?",
        "service::stop_installed_manager_observed_locked(",
    );
    let capture = transaction
        .split("fn capture_for_executable(executable: std::path::PathBuf)")
        .nth(1)
        .and_then(|tail| tail.split("fn stop_before_update_inner").next())
        .expect("self-update daemon capture");
    assert_before(
        capture,
        "service::acquire_service_operation_lease()?",
        "pidfile::running_identity_checked()?",
    );
    let client = daemon
        .split("pub(crate) struct SelfUpdateDaemonBoundaryClient")
        .nth(1)
        .and_then(|tail| tail.split("enum InstallerUninstallTransition").next())
        .expect("independent self-update daemon lifecycle client");
    for required in [
        "__hold-daemon-update-boundary",
        ".stdin(std::process::Stdio::piped())",
        ".stdout(std::process::Stdio::piped())",
        "impl Drop for SelfUpdateDaemonBoundaryClient",
        "self.input.take();",
        "child.wait()",
        "pub(crate) fn stop_replacement_for_rollback",
        "isolate_background_child_from_terminal_interrupt(&mut command)",
    ] {
        assert!(
            client.contains(required),
            "the async self-update client must retain an independent phase-aware holder: `{required}`"
        );
    }
    assert!(daemon.contains("libc::setsid()"));
    assert!(daemon.contains("CREATE_NEW_PROCESS_GROUP"));
    assert!(daemon.contains("CREATE_NO_WINDOW"));
    assert!(daemon.contains("detached_background_child_survives_parent_process_group_interrupt"));
    assert!(daemon.contains("detached_background_child_survives_parent_session_hangup"));
    assert_before(
        &command,
        "SelfUpdateDaemonBoundaryClient::start()",
        "update::self_update_dev(show_progress",
    );
    let synchronous_finish = command
        .split("fn finish_self_update_result_inner")
        .nth(1)
        .and_then(|tail| tail.split("pub(crate) async fn self_update_cmd").next())
        .expect("synchronous self-update commit boundary");
    assert_before(
        synchronous_finish,
        "daemon_boundary.restart_replacement()",
        ".verify_replacement_before_commit()",
    );
    assert_before(
        synchronous_finish,
        ".verify_replacement_before_commit()",
        ".commit_replacement()",
    );
    assert_before(
        synchronous_finish,
        ".commit_replacement()",
        ".release_verified_replacement()",
    );
    let interruption_recovery = command
        .split("fn recover_interrupted_self_update")
        .nth(1)
        .and_then(|tail| tail.split("fn panic_payload_message").next())
        .expect("phase-aware self-update interruption recovery");
    assert_before(
        interruption_recovery,
        "boundary.stop_replacement_for_rollback()?",
        ".rollback_replacement()",
    );
    assert_before(
        interruption_recovery,
        ".rollback_replacement()",
        ".restore_prior()",
    );
    assert!(command.contains("std::panic::catch_unwind"));
    assert!(transaction.contains("fn stop_lifecycle_generation"));
    assert!(transaction.contains("request.settle_for_lifecycle()"));
    assert!(transaction.contains("wait_for_requested_generation_to_settle("));
    assert!(
        !transaction.contains("stop_daemon_generation(pid)?"),
        "lifecycle transactions must not use the ordinary finite CLI stop wait"
    );
    assert!(pidfile.contains("pub(crate) struct DaemonAbsenceLease"));
    assert!(pidfile.contains("write_pidfile_exclusive"));

    let detached_start = daemon
        .split("// All CLI-initiated detached starts share the service-operation lease.")
        .nth(1)
        .and_then(|tail| tail.split("async fn run_foreground").next())
        .expect("normal detached-start lifecycle boundary");
    assert_before(
        detached_start,
        "service::acquire_service_operation_lease()?",
        "pidfile::running_pid_checked()?",
    );
    assert!(detached_start.contains("start_detached_executable_locked("));
    let detached_spawn = daemon
        .split("fn start_detached_executable_locked(")
        .nth(1)
        .and_then(|tail| tail.split("fn start_windows_installer_owned").next())
        .expect("detached daemon spawn implementation");
    assert!(
        detached_spawn.contains("isolate_background_child_from_terminal_interrupt(&mut command)")
    );

    let normal_stop = daemon
        .split("fn stop(expected_service_executable:")
        .nth(1)
        .and_then(|tail| tail.split("fn stop_windows_installer_owned").next())
        .expect("normal daemon-stop lifecycle boundary");
    assert_before(
        normal_stop,
        "service::acquire_service_operation_lease()?",
        "pidfile::running_pid_checked()?",
    );
    assert!(normal_stop.contains("stop_detached_locked(&service_lease)"));

    for required in [
        "return start_launchd(expected_executable)",
        "return start_systemd(expected_executable)",
        "fn start_launchd(expected_executable: &Path)",
        "validate_launchd_definition_owner(&contents, expected_executable)?",
        "fn start_systemd(expected_executable: &Path)",
        "validate_systemd_definition_owner(&contents, expected_executable)?",
    ] {
        assert!(
            service.contains(required),
            "Unix service restart must not rediscover the renamed running executable: `{required}`"
        );
    }
}

#[test]
fn windows_self_update_recovery_names_are_random_and_transaction_owned() {
    let update = repo_file("src/update.rs");
    assert!(update.contains("WINDOWS_RECOVERY_PATH_COLLISION_RETRY_LIMIT"));
    assert!(update.contains("let mut nonce = [0_u8; 16]"));
    assert!(update.contains("rand::rng().fill_bytes(&mut nonce)"));
    assert!(update.contains("\"failed candidate executable\""));

    let windows_replace = update
        .split("#[cfg(windows)]\nfn replace_candidate_inner(")
        .nth(1)
        .and_then(|tail| {
            tail.split("#[cfg(windows)]\nfn rollback_windows_replacement(")
                .next()
        })
        .expect("Windows replacement transaction");
    assert_eq!(
        windows_replace
            .matches("random_windows_recovery_sibling_path(")
            .count(),
        2,
        "both ReplaceFileW recovery operands must use independent randomized sibling names"
    );
    assert!(
        windows_replace.contains("replace_file_windows(executable, &staged, &displaced_previous)")
    );
}

#[test]
fn windows_self_update_cleanup_is_attested_journaled_and_retryable() {
    let update = repo_file("src/update.rs");
    let cleanup = repo_file("src/update/cleanup_worker.rs");
    let daemon = repo_file("src/daemon/mod.rs");
    let cli = repo_file("src/cli.rs");
    let app = repo_file("src/app.rs");
    let command = repo_file("src/commands/update.rs");

    for field in [
        "parent_pid: u32",
        "displaced: std::path::PathBuf",
        "expected_token: String",
        "expected_executable_token: String",
        "journal: std::path::PathBuf",
        "expected_journal_token: String",
        "ready_nonce: String",
    ] {
        assert!(
            cli.contains(field),
            "hidden cleanup command lost exact field {field}"
        );
    }
    assert!(cli.contains("name = \"__cleanup-self-update\""));
    assert_before(
        &app,
        "if let Some(Commands::CleanupSelfUpdate",
        "let use_json = cli.json || cli.json_pretty;",
    );

    assert!(cleanup.contains("const JOURNAL_SUFFIX: &str = \".self-update-cleanup-journal\""));
    assert!(cleanup.contains("backup_file_name: Vec<u16>"));
    assert!(cleanup.contains("backup_token: String"));
    assert!(
        !cleanup.contains("CODEX_SWITCH_HOME") && !cleanup.contains("crate::auth::app_home"),
        "cleanup authority must stay at the stable executable transaction boundary"
    );
    assert_before(
        &cleanup,
        "let (journal_path, journal_token) = create_journal(",
        "let mut command = std::process::Command::new(public_executable);",
    );
    assert_before(
        &cleanup,
        "let parent = open_parent(parent_pid)?;",
        "println!(\"{READY_PREFIX} {ready_nonce}\");",
    );
    assert!(cleanup.contains("OpenProcess(PROCESS_SYNCHRONIZE"));
    assert!(cleanup.contains("WaitForSingleObject(parent.0, INFINITE)"));
    assert!(cleanup.contains("complete_after_revalidation(&cleanup)"));
    assert!(cleanup.contains("super::acquire_update_lease(public_executable)"));
    assert!(cleanup.contains("super::recover_pending(&public)"));
    assert!(cleanup.contains("remove_exact(&cleanup.backup, &cleanup.backup_token)"));
    assert!(cleanup.contains("journaled backup executable changed before exact removal"));
    assert!(cleanup.contains("an_undeletable_journal_remains_exactly_retryable"));
    assert!(
        cleanup.contains("malformed_journal_is_preserved_without_deleting_the_displaced_image")
    );

    let verified_spawn = daemon
        .split("pub(crate) fn prepare_verified_background_spawn(")
        .nth(1)
        .and_then(|tail| {
            tail.split("pub(crate) fn validate_background_ready_nonce")
                .next()
        })
        .expect("verified background spawn primitive");
    assert!(verified_spawn.contains(".share_mode(FILE_SHARE_READ)"));
    assert!(!verified_spawn.contains("FILE_SHARE_WRITE"));
    assert!(!verified_spawn.contains("FILE_SHARE_DELETE"));
    assert!(daemon.contains("--expected-executable-token"));
    assert!(daemon.contains("--ready-nonce"));
    assert!(daemon.contains("read_marker_with_timeout(Some(LIFECYCLE_READY_TIMEOUT))"));
    assert!(daemon.contains("self.read_marker_with_timeout(None)"));
    assert!(daemon.contains("BACKGROUND_MARKER_LINE_MAX_BYTES"));
    assert!(daemon.contains("total marker output limit"));
    assert!(daemon.contains("inherited stdout handle defeated the marker deadline"));

    let commit = update
        .split("fn commit(&mut self) -> Result<()>")
        .nth(1)
        .and_then(|tail| tail.split("fn rollback(&mut self)").next())
        .expect("self-update commit boundary");
    assert_before(
        commit,
        "cleanup_worker::prepare(",
        "\"old executable backup\"",
    );
    assert_before(
        commit,
        "\"old executable backup\"",
        "cleanup_worker::spawn(",
    );
    assert!(update.contains("windows_commit_cleans_a_running_old_image_after_process_exit"));
    assert!(update.contains("WINDOWS_COMMIT_EXIT_AFTER_BACKUP_ENV"));
    assert!(update.contains("post-backup process death lost its pre-mutation cleanup journal"));
    assert!(update.contains("fail_after_parent_exit_once"));
    assert!(update.contains("fs::copy(&source, &second_candidate)"));
    assert!(update.contains("pub(crate) struct PendingSelfUpdateCleanup"));
    assert!(app.contains("format_pending_self_update_cleanup_warning"));
    assert!(command.contains("exact recovery must succeed before another executable publication"));
    assert!(command.contains(
        "Previous executable cleanup is journaled and will finish after this updater exits"
    ));
}

#[test]
fn daemon_state_errors_and_untrusted_status_text_remain_observable() {
    let state = repo_file("src/daemon/state.rs");
    let daemon = repo_file("src/daemon/mod.rs");

    assert!(state.contains("pub fn read() -> anyhow::Result<Option<DaemonState>>"));
    assert!(state.contains("error.kind() == std::io::ErrorKind::NotFound"));
    assert!(state.contains("fn write_snapshot(state: &DaemonState) -> anyhow::Result<()>"));
    assert!(state.contains("pub fn write(state: &mut DaemonState)"));
    assert!(state.contains("tracing::warn!"));
    assert!(state.contains("malformed_snapshot_is_reported_instead_of_treated_as_missing"));
    assert!(state.contains("snapshot_write_error_is_returned_to_the_best_effort_logging_boundary"));

    assert!(daemon.contains("let snapshot = if running { state::read()? } else { None };"));
    assert!(daemon.contains("bounded_status_last_error"));
    assert!(daemon.contains("STATUS_LAST_ERROR_MAX_CHARS"));
    assert!(
        daemon.contains("persisted_last_error_is_control_free_and_bounded_at_terminal_boundary")
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

    assert!(
        !daemon.contains("service::is_installed()"),
        "daemon mutation paths must not fold scheduler or service-marker errors into detached mode"
    );
    let detached_start = daemon.find("fn stop_detached_locked(").unwrap();
    let detached_end = daemon[detached_start..]
        .find("struct DaemonGenerationStopRequest")
        .unwrap()
        + detached_start;
    let detached = &daemon[detached_start..detached_end];
    let request_start = daemon
        .find("impl DaemonGenerationStopRequest")
        .expect("generation stop must expose one typed request boundary");
    let request_end = daemon[request_start..]
        .find("fn stop_daemon_generation(")
        .unwrap()
        + request_start;
    let request = &daemon[request_start..request_end];
    assert!(
        detached.contains("pidfile::running_generation_checked()?")
            && detached.contains("stop_daemon_generation(target)?")
            && request.contains("pidfile::request_shutdown(&target)?")
            && request.contains("wait_for_requested_generation_stop_with(")
            && request.contains("observe_requested_generation(&target)"),
        "a live daemon must carry one exact generation token from selection through shutdown delivery"
    );
    assert!(
        !detached.contains("let _ = pidfile::cleanup_pidfile();"),
        "Windows graceful-stop completion must propagate a locked PID-file cleanup failure"
    );
    assert!(detached.contains("pidfile::cleanup_pidfile()?"));
    let generation_observer = daemon
        .split("fn observe_requested_generation(")
        .nth(1)
        .and_then(|tail| tail.split("impl DaemonGenerationStopRequest").next())
        .expect("shared exact-generation observer");
    assert!(generation_observer.contains("pidfile::running_generation_checked()?"));
    assert!(generation_observer.contains("RequestedGenerationObservation::TargetRunning"));
    assert!(generation_observer.contains("RequestedGenerationObservation::Settled(observed)"));

    let bounded_stop = daemon
        .split("fn wait_for_requested_generation_stop_with")
        .nth(1)
        .and_then(|tail| {
            tail.split("fn wait_for_requested_generation_to_settle(")
                .next()
        })
        .expect("bounded exact-generation stop settlement");
    assert!(bounded_stop.contains("RequestedGenerationObservation::Settled(None)"));
    assert!(bounded_stop.contains("RequestedGenerationObservation::Settled(Some(current))"));
    assert!(bounded_stop.contains("TransientDiagnostics::default()"));
    assert!(bounded_stop.contains("elapsed() >= DAEMON_TRANSITION_TIMEOUT"));

    let lifecycle_settle = daemon
        .split("fn wait_for_requested_generation_to_settle(")
        .nth(1)
        .and_then(|tail| {
            tail.split("fn wait_for_requested_generation_to_settle_with")
                .next()
        })
        .expect("lifecycle generation settlement");
    assert!(lifecycle_settle.contains("observe_requested_generation(&target)"));
    assert!(lifecycle_settle.contains("authority remains held"));

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
    let scheduled_stop = &service[scheduled_stop_start..];
    assert!(
        scheduled_stop.contains("crate::daemon::pidfile::request_shutdown(&target)"),
        "scheduled-daemon rollback must request shutdown from the generation selected by the PID lock"
    );
    assert!(
        scheduled_stop.contains("stop_exact_scheduled_daemon_generation_with(")
            && scheduled_stop.contains(".target_is_running()")
            && scheduled_stop.contains("let finalization = finalize();"),
        "scheduled-daemon rollback must settle one exact generation before its single scheduler finalization"
    );
    assert_before(
        scheduled_stop,
        "crate::daemon::pidfile::request_shutdown(&target)",
        "fn finalize_scheduled_daemon_rollback_stop()",
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

#[test]
fn binary_entrypoint_delegates_to_one_library_module_graph() {
    let main = repo_file("src/main.rs");
    let lib = repo_file("src/lib.rs");

    assert!(main.contains("codex_switch::run().await"));
    assert!(
        !main
            .lines()
            .any(|line| line.trim_start().starts_with("mod ")),
        "the binary must not compile a second copy of the application modules"
    );
    assert!(lib.contains("mod app;"));
    assert!(lib.contains("mod commands;"));
    assert!(lib.contains("pub use app::run;"));
}
