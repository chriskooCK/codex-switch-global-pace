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
    assert!(
        workflow.contains("Parser]::ParseFile") && workflow.contains("scripts/install.ps1"),
        "Windows CI must parse install.ps1 with the PowerShell parser"
    );
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
fn release_stages_isolated_candidates_and_rechecks_every_tag_before_cutover() {
    let workflow = repo_file(".github/workflows/release.yml");

    for required in [
        "concurrency:",
        "group: release-${{ github.ref }}",
        "cancel-in-progress: false",
        "Prepare release verifiers",
        "git/ref/tags/${tag}",
        "git/tags/${sha}",
        "Confirm release tag still targets this source before publish",
        "Inspect an existing exact-tag release",
        "Create isolated candidate draft",
        "candidate_tag=\"release-candidate-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}\"",
        "draft:true",
        "Upload and verify isolated candidate assets",
        "Confirm exact tag still targets this source before cutover",
        "Archive the previous dev release without changing its assets",
        "Publish verified candidate on the exact tag",
        "Remove temporary cutover state after verified publication",
        "if: steps.publish.outputs.complete == 'true'",
        "cleanup-verified-release-assets",
        "Verified release %s remains published, but temporary state cleanup failed:",
        "Roll back an incomplete dev cutover",
        "Remove only this run's incomplete candidate",
        "steps.candidate.outputs.id",
        "repos/${GITHUB_REPOSITORY}/releases/${RELEASE_ID}",
        "'{tag_name:$tag,name:$name,draft:false,prerelease:$prerelease}'",
        "${GITHUB_SHA,,}",
    ] {
        assert!(
            workflow.contains(required),
            "rolling release freshness contract must contain `{required}`"
        );
    }
    assert_before(
        &workflow,
        "Confirm release tag still targets this source before publish",
        "Inspect an existing exact-tag release",
    );
    assert_before(
        &workflow,
        "Create isolated candidate draft",
        "Upload and verify isolated candidate assets",
    );
    assert_before(
        &workflow,
        "Upload and verify isolated candidate assets",
        "Archive the previous dev release without changing its assets",
    );
    assert_before(
        &workflow,
        "Confirm exact tag still targets this source before cutover",
        "Archive the previous dev release without changing its assets",
    );
    assert_before(
        &workflow,
        "Archive the previous dev release without changing its assets",
        "Publish verified candidate on the exact tag",
    );
    assert_before(
        &workflow,
        "Publish verified candidate on the exact tag",
        "Remove temporary cutover state after verified publication",
    );
    assert!(!workflow.contains("Delete existing dev release"));
    assert!(!workflow.contains("gh release delete dev"));
    assert!(!workflow.contains("--clobber"));
}

#[test]
fn release_reruns_preserve_published_assets_and_fail_closed_on_drift() {
    let workflow = repo_file(".github/workflows/release.yml");

    for required in [
        "releases/tags/${tag}",
        "Existing stable release ${release_id} metadata differs from this exact source.",
        "verify-release-assets.sh",
        "existing-release-assets\" attest",
        "gh attestation verify",
        "--bundle \"$provenance_bundle\"",
        "--signer-workflow \"$GITHUB_REPOSITORY/.github/workflows/release.yml\"",
        "--source-digest \"$GITHUB_SHA\"",
        "--source-ref \"$GITHUB_REF\"",
        "Existing checksum $(basename \"$checksum\") must contain exactly one line.",
        "[[ ! \"$recorded_digest\" =~ ^[0-9a-fA-F]{64}$",
        "actual_digest=$(sha256sum -- \"$archive\")",
        "externalParameters.workflow.path == \".github/workflows/release.yml\"",
        ".digest.gitCommit == $sha",
        "echo \"skip=true\" >> \"$GITHUB_OUTPUT\"",
        "archive_tag=\"dev-archive-${OLD_RELEASE_ID}\"",
        "refs/tags/${archive_tag}",
        "echo \"ref_created=true\" >> \"$GITHUB_OUTPUT\"",
        "ARCHIVE_REF_CREATED: ${{ steps.archive.outputs.ref_created }}",
        "'{tag_name:\"dev\"}'",
        "Candidate ${CANDIDATE_ID} changed identity; refusing to delete it during rollback.",
        "Previous dev release ${OLD_RELEASE_ID} changed identity; refusing rollback.",
        "${candidate_target,,}",
        "candidate_draft=$(jq -r '.draft'",
        "Release ${RELEASE_ID} no longer matches this run; refusing cleanup.",
    ] {
        assert!(
            workflow.contains(required),
            "transactional rerun contract must contain `{required}`"
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
        "Upload and verify isolated candidate assets",
        "refs/tags/${archive_tag}",
    );
    let verified_cleanup = workflow
        .split("- name: Remove temporary cutover state after verified publication")
        .nth(1)
        .and_then(|tail| {
            tail.split("- name: Roll back an incomplete dev cutover")
                .next()
        })
        .expect("verified temporary-state cleanup step");
    for required in [
        "steps.publish.outputs.complete == 'true'",
        "cleanup-verified-release-assets",
        "repos/${GITHUB_REPOSITORY}/releases/${OLD_RELEASE_ID}",
        "git/refs/tags/${archive_tag}",
        "old archive release ${OLD_RELEASE_ID} remains",
        "archive ref ${archive_tag} remains",
        "archive ref ${archive_tag} has no creation proof and was preserved",
        "Verified release %s remains published, but temporary state cleanup failed:",
    ] {
        assert!(
            verified_cleanup.contains(required),
            "verified cleanup contract must contain `{required}`"
        );
    }
    assert_before(
        verified_cleanup,
        "cleanup-verified-release-assets",
        "if [[ \"$old_release_safe\" == true ]]",
    );
    let old_delete = verified_cleanup
        .split("if [[ \"$old_release_safe\" == true ]]")
        .nth(1)
        .expect("old archive release deletion branch");
    assert!(old_delete.contains("repos/${GITHUB_REPOSITORY}/releases/${OLD_RELEASE_ID}"));
    assert!(!verified_cleanup.contains("tag_name:\"dev\""));
    let rollback = workflow
        .split("- name: Roll back an incomplete dev cutover")
        .nth(1)
        .and_then(|tail| {
            tail.split("- name: Remove only this run's incomplete candidate")
                .next()
        })
        .expect("incomplete dev cutover rollback step");
    for required in [
        "${old_target,,}",
        "$(jq -r '.draft' <<<\"$old\")",
        "${candidate_target,,}",
        "candidate_draft=$(jq -r '.draft' <<<\"$candidate\")",
        "candidate_after_target=$(jq -r '.target_commitish' <<<\"$candidate_after\")",
        "final deletion state is ambiguous; refusing rollback",
        "if [[ \"$ARCHIVE_REF_CREATED\" == true ]]",
    ] {
        assert!(
            rollback.contains(required),
            "rollback ownership contract must contain `{required}`"
        );
    }
    assert_before(
        rollback,
        "${candidate_target,,}",
        "gh api --method DELETE \\",
    );
    assert_before(
        rollback,
        "gh api --method DELETE \\",
        "'{tag_name:\"dev\"}'",
    );

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
        "( \"$tag\" == \"$CANDIDATE_TAG\" && \"$draft\" != true )",
        "( \"$tag\" == \"$final_tag\" && \"$draft\" != false )",
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
        "rm -f \"$LEGACY_BIN\"",
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

    assert!(install.contains("if [ -e \"$LEGACY_BIN\" ] || [ -L \"$LEGACY_BIN\" ]; then"));
    assert_before(
        install,
        "classify_legacy_binary",
        "if [ \"$SYSTEM_INSTALL\" = false ]; then",
    );
    assert_before(install, "[ \"$LEGACY_KIND\" = \"homebrew\" ]", "# Install");
}

#[test]
fn legacy_service_migration_preserves_the_old_absolute_path_until_reinstall_succeeds() {
    let script = repo_file("scripts/install.sh");
    let install = script
        .split("# Download, verify, and extract")
        .nth(1)
        .expect("Unix install transaction section");
    for required in [
        "legacy_service_references_binary",
        "legacy_service_is_running",
        "MIGRATE_LEGACY_SERVICE=true",
        "\"$INSTALL_DEST\" daemon install",
        "\"$LEGACY_BIN\" daemon install",
        "Both verified binaries were kept",
    ] {
        assert!(
            script.contains(required),
            "missing service migration contract `{required}`"
        );
    }
    assert_before(
        install,
        "\"$INSTALL_DEST\" daemon install",
        "rm -f \"$LEGACY_BIN\"",
    );
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
        "classify_legacy_binary()",
        "LEGACY_RESOLVED=\"$(resolve_path_target \"$LEGACY_BIN\")\"",
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
        "if [ \"$LEGACY_KIND\" = \"homebrew\" ]; then",
        "MIGRATE_LEGACY=true",
    );

    let uninstall = script
        .split("# ── Uninstall")
        .nth(1)
        .and_then(|section| section.split("# ── Install").next())
        .expect("Unix installer must retain distinct uninstall/install sections");
    assert!(uninstall.contains("classify_legacy_binary"));
    assert!(uninstall.contains("the direct uninstaller did not change Homebrew files"));
    assert!(uninstall.contains("[ \"$LEGACY_KIND\" = \"direct\" ]"));
    assert_before(
        uninstall,
        "if [ \"$LEGACY_KIND\" = \"homebrew\" ]",
        "rm -f \"$BIN_PATH\"",
    );
}

#[cfg(unix)]
#[test]
fn unix_homebrew_classifier_executes_against_a_resolved_cellar_symlink() {
    use std::os::unix::fs::symlink;
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
    let classifier = section(
        &script,
        "classify_legacy_binary() {",
        "resolve_path_target() (",
    );
    let resolver = section(&script, "resolve_path_target() (", "file_identity() (");
    let cellar_matcher = section(
        &script,
        "is_homebrew_cellar_path() {",
        "classify_legacy_binary() {",
    );
    let harness = format!(
        "set -eu\n{cellar_matcher}\n{classifier}\n{resolver}\nclassify_legacy_binary\n[ \"$LEGACY_KIND\" = homebrew ]\nprintf 'refused:%s\\n' \"$LEGACY_RESOLVED\"\n"
    );

    let dir = tempfile::tempdir().unwrap();
    let cellar_binary = dir
        .path()
        .join("Cellar/codex-switch-global-pace/20260824.7.0/bin/codex-switch-global-pace");
    fs::create_dir_all(cellar_binary.parent().unwrap()).unwrap();
    fs::write(&cellar_binary, "homebrew-owned").unwrap();
    let legacy_link = dir.path().join("legacy-bin");
    symlink(&cellar_binary, &legacy_link).unwrap();

    let output = Command::new("bash")
        .args(["-c", &harness])
        .env("LEGACY_BIN", &legacy_link)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "stdout={stdout}\nstderr={stderr}");
    assert!(
        stdout.starts_with("refused:")
            && stdout.contains("/Cellar/codex-switch-global-pace/20260824.7.0/bin/"),
        "{stdout}"
    );
    assert_eq!(fs::read_to_string(cellar_binary).unwrap(), "homebrew-owned");
}

#[test]
fn unix_installer_preserves_migration_and_path_lifecycle() {
    let script = repo_file("scripts/install.sh");

    for required in [
        "*/fish)",
        "PROFILE_FILE=\"${HOME}/.config/fish/config.fish\"",
        "# >>> codex-switch-global-pace PATH >>>",
        "# <<< codex-switch-global-pace PATH <<<",
        "remove_managed_path_blocks",
        "remove_path_block \"${HOME}/.zprofile\"",
        "remove_path_block \"${HOME}/.bash_profile\"",
        "remove_path_block \"${HOME}/.profile\"",
        "remove_path_block \"${HOME}/.config/fish/config.fish\"",
        "!seen_begin || !seen_end || inside",
    ] {
        assert!(
            script.contains(required),
            "Unix installer must contain `{required}`"
        );
    }

    assert_before(&script, "tar xzf", "sudo -v");
    assert_before(
        &script,
        "mkdir -p \"$INSTALL_DIR\"",
        "sudo rm -f \"$LEGACY_BIN\"",
    );
    assert!(script.contains(
        "if [ \"$SYSTEM_INSTALL\" = false ]; then\n    remove_managed_path_blocks\n  fi"
    ));
}

#[test]
fn unix_installer_rewrites_shell_profiles_atomically() {
    let script = repo_file("scripts/install.sh");

    for required in [
        "remove_path_block() (",
        "resolve_path_target() (",
        "file_identity() (",
        "while [ -L \"$profile_target\" ]",
        "link_target=\"$(readlink \"$profile_target\")\"",
        "cd -P \"$(dirname \"$profile_target\")\" && pwd -P",
        "mktemp \"${profile_dir}/.${BINARY_NAME}.XXXXXX\"",
        "cp -p \"$profile_target\" \"$tmp_file\"",
        "current_profile_target=\"$(resolve_path_target \"$profile_file\")\"",
        "current_profile_identity=\"$(file_identity \"$current_profile_target\")\"",
        "mv -f \"$tmp_file\" \"$profile_target\"",
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
        .split("\nremove_managed_path_blocks() {")
        .next()
        .expect("installer must define remove_managed_path_blocks");
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
        format!("{function_prefix}\nremove_path_block \"$1\"\n"),
    )
    .unwrap();
    let output = Command::new("bash")
        .arg(&harness)
        .arg(&profile_link)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "remove_path_block failed: {}",
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
        .split("\nremove_managed_path_blocks() {")
        .next()
        .expect("installer must define remove_managed_path_blocks");
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
        format!("{function_prefix}\nremove_path_block \"$1\"\n"),
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
        .split("\nremove_managed_path_blocks() {")
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
        format!("{function_prefix}\nremove_path_block \"$1\"\n"),
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
        .split("\nremove_managed_path_blocks() {")
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
        format!("{function_prefix}\nremove_path_block \"$1\"\n"),
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
        "rm -f \"$LEGACY_BIN\" \"$SYSTEM_INSTALL_MARKER\"",
        "sudo rm -f \"$LEGACY_BIN\" \"$SYSTEM_INSTALL_MARKER\"",
    ] {
        assert!(
            script.contains(required),
            "Unix installer must preserve system-install marker lifecycle: `{required}`"
        );
    }
}

#[test]
fn windows_installer_verifies_checksum_before_extracting() {
    let script = repo_file("scripts/install.ps1");

    assert!(script.contains("$ChecksumUrl"));
    assert!(script.contains("Get-FileHash"));
    assert!(script.contains("SHA256"));
    assert_before(&script, "Get-FileHash", "Expand-Archive");
    assert!(
        script.contains("Checksum mismatch"),
        "Windows installer must fail clearly on checksum mismatch"
    );
    assert!(script.contains("$env:LOCALAPPDATA"));
    assert!(script.contains("SetEnvironmentVariable(\"Path\", $NewPath, \"User\")"));
}

#[test]
fn windows_installer_preserves_a_running_daemon_across_upgrade() {
    let script = repo_file("scripts/install.ps1");

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
        "Boolean 'running' field",
        "Boolean 'platform.service_installed' field",
        "function Stop-And-ConfirmDaemonAbsent",
        "$After = Get-CheckedDaemonStatus -BinPath $BinPath",
        "$DaemonSafeForBinaryRollback",
        "automatic binary rollback was refused",
        ".$BinaryStem.install-$TransactionId.exe",
        "--json daemon status",
        "& $InstalledBin daemon stop",
        "& $InstalledBin daemon start",
        "The running daemon could not be stopped safely",
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
        &script,
        "$DaemonStatus = Get-CheckedDaemonStatus -BinPath $InstalledBin",
        "Move-Item -LiteralPath $InstalledBin -Destination $BackupBin",
    );
    assert_before(
        &script,
        "$CandidateVersionOutput = & $CandidateBin --version",
        "$StagedBin = Join-Path $InstallDir",
    );
    assert_before(
        &script,
        "$StagedVersionOutput = & $StagedBin --version",
        "$DaemonStatus = Get-CheckedDaemonStatus -BinPath $InstalledBin",
    );
    assert_before(
        &script,
        "$DaemonStatus = Get-CheckedDaemonStatus -BinPath $InstalledBin",
        "& $InstalledBin daemon stop",
    );
    assert_before(&script, "& $InstalledBin daemon stop", "Move-Item");
    assert!(
        script.contains("if ($DaemonWasRunning -or $DaemonServiceInstalled)"),
        "an installed but currently stopped task must still be ended before its executable is replaced"
    );

    let rollback_start = script
        .find("if ($null -ne $InstallError)")
        .expect("Windows installer must have an explicit rollback branch");
    let rollback = &script[rollback_start..];
    assert_before(
        rollback,
        "Stop-And-ConfirmDaemonAbsent -BinPath $InstalledBin",
        "if ($NewBinaryPublished -and $DaemonSafeForBinaryRollback)",
    );
    assert!(rollback.contains(
        "$DaemonWasRunning -and $DaemonSafeForBinaryRollback -and $PreviousBinaryRestored"
    ));
    assert_before(
        rollback,
        "Move-Item -LiteralPath $BackupBin -Destination $InstalledBin",
        "Restarting the previous daemon after rollback",
    );
    assert_before(
        rollback,
        "SetEnvironmentVariable(\"Path\", $OriginalUserPath, \"User\")",
        "Restarting the previous daemon after rollback",
    );
}

#[test]
fn windows_uninstaller_fails_closed_without_a_binary_to_stop_a_running_task() {
    let script = repo_file("scripts/install.ps1");

    for required in [
        "Get-ScheduledTask",
        "$TaskState -notin @(\"Ready\", \"Disabled\")",
        "installed binary is unavailable for a graceful stop",
        "$ServiceUninstallFailed = $true",
        "Unregister-ScheduledTask",
    ] {
        assert!(
            script.contains(required),
            "Windows uninstaller must contain fail-closed task guard `{required}`"
        );
    }
    assert!(
        !script.contains("schtasks.exe /End"),
        "the no-binary fallback must never force-end a running daemon"
    );
    assert_before(
        &script,
        "$TaskState -notin @(\"Ready\", \"Disabled\")",
        "Unregister-ScheduledTask",
    );
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
    ] {
        assert!(
            service.contains(required),
            "missing service transaction contract `{required}`"
        );
    }
    assert_before(
        &service,
        "generated LaunchAgent failed plutil validation",
        "was_loaded",
    );
    assert_before(
        &service,
        "generated systemd user service failed validation",
        "was_active",
    );
    assert_before(
        &service,
        "create_scheduled_task(&stage_name",
        "previous_was_running {",
    );
}

#[test]
fn ci_pins_the_audit_executable_version() {
    let workflow = repo_file(".github/workflows/ci.yml");
    assert!(workflow.contains("cargo install cargo-audit --version 0.22.2 --locked"));
}

#[test]
fn self_update_gates_markerless_system_installs_before_network_checks() {
    let command = repo_file("src/commands/update.rs");

    assert_before(
        &command,
        "ensure_legacy_system_install_migrated(use_dev, version)",
        "if check",
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

    let uninstall_start = daemon.find("fn uninstall()").unwrap();
    let uninstall_end = daemon[uninstall_start..].find("async fn start").unwrap() + uninstall_start;
    let uninstall = &daemon[uninstall_start..uninstall_end];
    assert!(
        uninstall.matches("pidfile::running_pid_checked()?").count() >= 2,
        "Windows uninstall must check the PID-lock authority before graceful shutdown and again \
         immediately before Task Scheduler may force-stop the daemon"
    );

    let stop_start = daemon.find("fn stop()").unwrap();
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
        .find("fn wait_until_stopped_or_kill")
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
        .find("fn uninstall_task_scheduler()")
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

    let service_uninstall_start = service.find("fn uninstall_task_scheduler()").unwrap();
    let service_uninstall = &service[service_uninstall_start..];
    assert!(
        service_uninstall.contains("verify_daemon_absent_after_service_stop(")
            && service_uninstall.contains("crate::daemon::pidfile::running_pid_checked()"),
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
