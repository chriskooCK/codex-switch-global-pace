use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs4::FileExt;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const REPO_OWNER: &str = "chriskooCK";
const REPO_NAME: &str = "codex-switch-global-pace";
const BIN_NAME: &str = "codex-switch-global-pace";
const PROVENANCE_ASSET_NAME: &str = "codex-switch-global-pace-build-provenance.json";
const RELEASE_WORKFLOW: &str = "chriskooCK/codex-switch-global-pace/.github/workflows/release.yml";
const SYSTEM_INSTALL_DIR: &str = "/usr/local/bin";
const SYSTEM_INSTALL_MARKER_NAME: &str = ".codex-switch-global-pace-system-install-v1";
const UPDATE_CACHE_NAME: &str = "global-pace-update-check.json";
const UPDATE_TTL_SECS: i64 = 12 * 60 * 60;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct LegacySystemInstallMigrationRequired {
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdatePlatform {
    Unix,
    Windows,
}

fn current_update_platform() -> UpdatePlatform {
    if cfg!(windows) {
        UpdatePlatform::Windows
    } else {
        UpdatePlatform::Unix
    }
}

fn unix_migration_command(release_tag: &str) -> String {
    let safe_tag = !release_tag.is_empty()
        && release_tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    let url = if safe_tag {
        format!(
            "https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/{release_tag}/install.sh"
        )
    } else {
        format!("https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/latest/download/install.sh")
    };
    if release_tag == "dev" && safe_tag {
        format!("`curl -fsSL {url} | bash -s -- --dev`")
    } else {
        format!("`curl -fsSL {url} | bash`")
    }
}

fn legacy_system_install_migration_hint(
    executable: &Path,
    platform: UpdatePlatform,
    marker_present: bool,
    use_dev: bool,
    requested_version: Option<&str>,
) -> Option<String> {
    if platform != UpdatePlatform::Unix
        || executable.parent() != Some(Path::new(SYSTEM_INSTALL_DIR))
        || marker_present
    {
        return None;
    }

    let exact_version = requested_version.map(normalize_version);
    if exact_version
        .as_deref()
        .is_some_and(|version| Version::parse(version).is_err())
    {
        return Some(format!(
            "One-time setup could not start\n\nThe requested version is not valid. Use a semantic version such as `20260712.2.0`, then retry. The existing installation at '{}' was not changed.",
            executable.display()
        ));
    }

    let (user_command, system_command) = if let Some(version) = exact_version {
        let url = format!(
            "https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/v{version}/install.sh"
        );
        (
            format!("curl -fsSL {url} | CS_VERSION={version} bash"),
            format!("curl -fsSL {url} | CS_VERSION={version} bash -s -- --system"),
        )
    } else if use_dev {
        (
            format!(
                "curl -fsSL https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/dev/install.sh | bash -s -- --dev"
            ),
            format!(
                "curl -fsSL https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/download/dev/install.sh | bash -s -- --dev --system"
            ),
        )
    } else {
        (
            format!(
                "curl -fsSL https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/latest/download/install.sh | bash"
            ),
            format!(
                "curl -fsSL https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/latest/download/install.sh | bash -s -- --system"
            ),
        )
    };

    Some(format!(
        "One-time setup required\n\ncodex-switch-global-pace is still installed system-wide at '{}'. Choose how future updates should work.\n\nRecommended — move it to your user account:\n  {user_command}\n\nProfiles and configuration are preserved. Future updates will not need sudo.\n\nKeep the system-wide install instead:\n  {system_command}\n\nFuture updates will continue to require sudo.",
        executable.display()
    ))
}

fn canonical_executable_path(executable: PathBuf) -> PathBuf {
    fs::canonicalize(&executable).unwrap_or(executable)
}

pub fn ensure_legacy_system_install_migrated(
    use_dev: bool,
    requested_version: Option<&str>,
) -> Result<()> {
    let executable =
        canonical_executable_path(std::env::current_exe().context("locating current executable")?);
    let platform = current_update_platform();
    let marker_present = executable
        .parent()
        .map(|parent| parent.join(SYSTEM_INSTALL_MARKER_NAME).is_file())
        .unwrap_or(false);

    if let Some(hint) = legacy_system_install_migration_hint(
        &executable,
        platform,
        marker_present,
        use_dev,
        requested_version,
    ) {
        return Err(LegacySystemInstallMigrationRequired { message: hint }.into());
    }
    Ok(())
}

fn replacement_permission_hint(
    executable: &Path,
    platform: UpdatePlatform,
    release_tag: &str,
) -> String {
    let parent = executable.parent().unwrap_or(executable);
    match platform {
        UpdatePlatform::Unix if parent == Path::new(SYSTEM_INSTALL_DIR) => format!(
            "install directory '{}' is not writable; for a legacy direct install, rerun the user-level installer once with {}. If codex-switch-global-pace was intentionally installed with `--system`, run `sudo codex-switch-global-pace self-update` instead",
            parent.display(),
            unix_migration_command(release_tag)
        ),
        UpdatePlatform::Unix => format!(
            "user-owned install directory '{}' is not writable; fix its ownership or reinstall with the user-level installer. Do not run self-update with elevated privileges",
            parent.display()
        ),
        UpdatePlatform::Windows => format!(
            "install directory '{}' is not writable; close running codex-switch-global-pace processes and retry, or reinstall with the user-level installer",
            parent.display()
        ),
    }
}

pub(crate) fn homebrew_dev_install_hint() -> &'static str {
    "run `brew uninstall codex-switch-global-pace`, then follow the development-release instructions at https://github.com/chriskooCK/codex-switch-global-pace/blob/dev/docs/wiki/Development-Releases.md#install-the-rolling-dev-build"
}

fn homebrew_dev_install_error() -> String {
    format!(
        "codex-switch-global-pace is installed via Homebrew. To switch to dev, {}.",
        homebrew_dev_install_hint()
    )
}

fn ensure_replace_parent_writable(
    executable: &Path,
    platform: UpdatePlatform,
    release_tag: &str,
) -> Result<()> {
    let parent = executable
        .parent()
        .with_context(|| format!("current executable has no parent: {}", executable.display()))?;
    tempfile::NamedTempFile::new_in(parent)
        .with_context(|| replacement_permission_hint(executable, platform, release_tag))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallSource {
    Homebrew,
    Direct,
}

impl InstallSource {
    pub fn as_str(self) -> &'static str {
        match self {
            InstallSource::Homebrew => "homebrew",
            InstallSource::Direct => "direct",
        }
    }

    pub fn upgrade_hint(self) -> &'static str {
        match self {
            InstallSource::Homebrew => "brew upgrade codex-switch-global-pace",
            InstallSource::Direct => "codex-switch-global-pace self-update",
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub install_source: InstallSource,
}

#[derive(Debug)]
pub struct SelfUpdateResult {
    pub current_version: String,
    pub latest_version: String,
    pub install_source: InstallSource,
    pub updated: bool,
    replacement: Option<PendingReplacement>,
    transaction_lease: Option<UpdateLease>,
}

impl SelfUpdateResult {
    /// Finish a successful update after every dependent process has restarted.
    ///
    /// On Windows the old executable is the image backing this still-running
    /// process, so cleanup is scheduled for process exit. Cleanup failure does
    /// not invalidate a replacement whose binary and daemon are already healthy.
    pub(crate) fn commit_replacement(&mut self) {
        if let Some(mut replacement) = self.replacement.take() {
            replacement.commit();
        }
        self.transaction_lease.take();
    }

    /// Restore the pre-update executable before restarting the old daemon.
    #[cfg(windows)]
    pub(crate) fn rollback_replacement(&mut self) -> Result<()> {
        if let Some(mut replacement) = self.replacement.take() {
            replacement.rollback()?;
        }
        self.transaction_lease.take();
        Ok(())
    }

    /// Keep the durable backup when process state cannot be proven safe enough
    /// for an automatic rollback, and return the exact manual-recovery paths.
    #[cfg(windows)]
    pub(crate) fn preserve_replacement_for_recovery(&mut self) -> Result<ReplacementRecoveryPaths> {
        let mut replacement = self
            .replacement
            .take()
            .context("updated Windows result has no pending executable replacement")?;
        let recovery_paths = replacement.recovery_paths();
        replacement.preserve();
        self.transaction_lease.take();
        Ok(recovery_paths)
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplacementRecoveryPaths {
    pub(crate) executable: PathBuf,
    pub(crate) previous_executable: PathBuf,
}

#[derive(Debug, Clone)]
struct UpdateLease {
    _inner: std::sync::Arc<UpdateLeaseInner>,
}

#[derive(Debug)]
struct UpdateLeaseInner {
    file: fs::File,
}

#[derive(Debug, Clone)]
pub(crate) struct SelfUpdateLease(UpdateLease);

impl Drop for UpdateLeaseInner {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplacementState {
    Pending,
    Finished,
    Preserved,
}

#[derive(Debug)]
struct PendingReplacement {
    lease: Option<UpdateLease>,
    state: ReplacementState,
    #[cfg(windows)]
    executable: PathBuf,
    #[cfg(windows)]
    backup: PathBuf,
    #[cfg(windows)]
    failed_candidate: PathBuf,
}

impl PendingReplacement {
    #[cfg(not(windows))]
    fn new(lease: UpdateLease) -> Self {
        Self {
            lease: Some(lease),
            state: ReplacementState::Pending,
        }
    }

    #[cfg(windows)]
    fn new(
        lease: UpdateLease,
        executable: PathBuf,
        backup: PathBuf,
        failed_candidate: PathBuf,
    ) -> Self {
        Self {
            lease: Some(lease),
            state: ReplacementState::Pending,
            executable,
            backup,
            failed_candidate,
        }
    }

    #[cfg(windows)]
    fn recovery_paths(&self) -> ReplacementRecoveryPaths {
        ReplacementRecoveryPaths {
            executable: self.executable.clone(),
            previous_executable: self.backup.clone(),
        }
    }

    fn commit(&mut self) {
        if self.state != ReplacementState::Pending {
            return;
        }
        #[cfg(windows)]
        if let Err(err) = cleanup_committed_windows_backup(&self.backup) {
            tracing::warn!(
                "Updated executable is active, but old executable cleanup was deferred: {err}"
            );
        }
        self.state = ReplacementState::Finished;
        self.lease.take();
    }

    #[cfg(windows)]
    fn rollback(&mut self) -> Result<()> {
        if self.state != ReplacementState::Pending {
            return Ok(());
        }
        // A failed rollback must leave the backup untouched for manual recovery;
        // Drop must never reinterpret that state as permission to delete it.
        self.state = ReplacementState::Preserved;
        #[cfg(windows)]
        rollback_windows_replacement(&self.executable, &self.backup, &self.failed_candidate)?;
        self.state = ReplacementState::Finished;
        self.lease.take();
        Ok(())
    }

    fn preserve(&mut self) {
        if self.state == ReplacementState::Pending {
            self.state = ReplacementState::Preserved;
        }
        self.lease.take();
    }
}

impl Drop for PendingReplacement {
    fn drop(&mut self) {
        // A pending replacement has not passed the caller's daemon-health
        // boundary. Early return or unwinding must retain the old executable
        // for manual recovery; only an explicit commit may clean it up.
        if self.state == ReplacementState::Pending {
            self.preserve();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateCache {
    checked_at: i64,
    latest_version: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubRelease {
    tag_name: String,
    name: Option<String>,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubGitReference {
    object: GithubGitObject,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubGitTag {
    object: GithubGitObject,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubGitObject {
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

pub async fn check_for_update(force: bool) -> Result<Option<UpdateInfo>> {
    let current_version = current_version().to_string();
    let latest_version = latest_release_version(force).await?;
    if !is_newer_version(&latest_version, &current_version) {
        return Ok(None);
    }

    Ok(Some(UpdateInfo {
        current_version,
        latest_version,
        install_source: detect_install_source(),
    }))
}

/// Check whether a newer dev release exists on GitHub.
///
/// Dev versions use a `dev` pre-release component. Older timestamped dev
/// versions remain supported for updates from existing installations.
pub async fn check_for_dev_update() -> Result<Option<UpdateInfo>> {
    let current_version = current_version().to_string();
    let release = match fetch_release_optional(Some("dev"))
        .await
        .context("checking dev release")?
    {
        Some(r) => r,
        None => return Ok(None), // No dev release exists (404).
    };
    let dev_version = extract_release_version(&release);
    if !is_dev_update_available(&dev_version, &current_version) {
        return Ok(None);
    }
    Ok(Some(UpdateInfo {
        current_version,
        latest_version: dev_version,
        install_source: detect_install_source(),
    }))
}

pub(crate) async fn self_update(
    version: Option<&str>,
    show_progress: bool,
    lease: SelfUpdateLease,
) -> Result<SelfUpdateResult> {
    // Before anything reaches the network: the argument becomes part of a
    // GitHub API path, so it is rejected here rather than encoded and sent.
    let requested_version = version.map(validate_requested_version).transpose()?;

    let install_source = detect_install_source();
    if install_source == InstallSource::Homebrew {
        anyhow::bail!(
            "Homebrew-managed install detected. Run `{}` instead.",
            install_source.upgrade_hint()
        );
    }
    let update_lease = lease.0;

    let current_version = current_version().to_string();
    let release = fetch_release(requested_version.as_deref()).await?;
    let latest_version = extract_release_version(&release);

    if let Some(requested) = requested_version {
        if requested != latest_version {
            anyhow::bail!("requested version '{requested}' was not found on GitHub Releases");
        }
        if is_older_version(&latest_version, &current_version) {
            anyhow::bail!(
                "downgrades are not supported: requested version {latest_version} is older than current version {current_version}"
            );
        }
        if latest_version == current_version {
            return Ok(SelfUpdateResult {
                current_version,
                latest_version,
                install_source,
                updated: false,
                replacement: None,
                transaction_lease: Some(update_lease),
            });
        }
    } else if !is_newer_version(&latest_version, &current_version) {
        return Ok(SelfUpdateResult {
            current_version,
            latest_version,
            install_source,
            updated: false,
            replacement: None,
            transaction_lease: Some(update_lease),
        });
    }

    let replacement = download_and_replace(&release, show_progress, "", update_lease).await?;

    save_update_cache(&UpdateCache {
        checked_at: crate::auth::now_unix_secs(),
        latest_version: latest_version.clone(),
    });

    Ok(SelfUpdateResult {
        current_version,
        latest_version,
        install_source,
        updated: true,
        replacement: Some(replacement),
        transaction_lease: None,
    })
}

/// Install the dev build from the `dev` GitHub Release tag.
///
/// Switching from dev→stable uses the normal `self_update` path.
pub(crate) async fn self_update_dev(
    show_progress: bool,
    lease: SelfUpdateLease,
) -> Result<SelfUpdateResult> {
    let install_source = detect_install_source();
    if install_source == InstallSource::Homebrew {
        anyhow::bail!(homebrew_dev_install_error());
    }
    let update_lease = lease.0;

    let current_version = current_version().to_string();
    let release = fetch_release(Some("dev"))
        .await
        .context("fetching dev release from GitHub")?;
    let dev_version = extract_release_version(&release);

    if !is_dev_update_available(&dev_version, &current_version) {
        return Ok(SelfUpdateResult {
            current_version,
            latest_version: dev_version,
            install_source,
            updated: false,
            replacement: None,
            transaction_lease: Some(update_lease),
        });
    }

    let replacement = download_and_replace(&release, show_progress, " (dev)", update_lease).await?;

    Ok(SelfUpdateResult {
        current_version,
        latest_version: dev_version,
        install_source,
        updated: true,
        replacement: Some(replacement),
        transaction_lease: None,
    })
}

/// Extract a semver-compatible version string from a GitHub Release.
///
/// For dev releases (`is_dev = true`) the version is embedded in the release
/// name (e.g. `"dev (20260712.1.0-dev)"`) because the tag itself is just
/// `"dev"`. For stable releases the tag carries the version directly.
fn extract_release_version(release: &GithubRelease) -> String {
    // Dev releases carry the version in the name: "dev (X.Y.Z-dev)"
    if release.tag_name == "dev"
        && let Some(v) = release
            .name
            .as_deref()
            .and_then(|n| n.strip_prefix("dev ("))
            .and_then(|n| n.strip_suffix(')'))
        && Version::parse(v).is_ok()
    {
        return v.to_string();
    }
    normalize_version(&release.tag_name)
}

/// Download, verify, extract and replace the current binary from a GitHub Release.
async fn download_and_replace(
    release: &GithubRelease,
    show_progress: bool,
    label_suffix: &str,
    update_lease: UpdateLease,
) -> Result<PendingReplacement> {
    let executable =
        fs::canonicalize(std::env::current_exe().context("locating current executable")?)
            .context("resolving current executable")?;
    let platform = current_update_platform();
    ensure_replace_parent_writable(&executable, platform, &release.tag_name)?;
    let client =
        crate::auth::build_http_client().context("building HTTP client for self-update")?;
    let archive_name = asset_name();
    let archive_asset = release
        .assets
        .iter()
        .find(|a| a.name == archive_name)
        .cloned()
        .with_context(|| format!("release does not contain asset '{archive_name}'"))?;
    let checksum_name = format!("{archive_name}.sha256");
    let checksum_asset = release
        .assets
        .iter()
        .find(|a| a.name == checksum_name)
        .cloned()
        .with_context(|| format!("release does not contain checksum asset '{checksum_name}'"))?;
    let provenance_asset = release
        .assets
        .iter()
        .find(|a| a.name == PROVENANCE_ASSET_NAME)
        .cloned()
        .with_context(|| {
            format!("release does not contain provenance asset '{PROVENANCE_ASSET_NAME}'")
        })?;

    let temp_dir = tempfile::tempdir().context("creating temporary update directory")?;
    let archive_path = temp_dir.path().join(&archive_asset.name);
    let provenance_path = temp_dir.path().join(PROVENANCE_ASSET_NAME);
    if show_progress {
        eprintln!("Downloading {}{}...", archive_asset.name, label_suffix);
    }
    download_file(&client, &archive_asset.browser_download_url, &archive_path).await?;
    verify_checksum(&client, &checksum_asset.browser_download_url, &archive_path).await?;
    download_file(
        &client,
        &provenance_asset.browser_download_url,
        &provenance_path,
    )
    .await?;
    let source_digest = fetch_tag_commit_sha(&client, &release.tag_name).await?;
    verify_build_provenance(
        &archive_path,
        &provenance_path,
        &release.tag_name,
        &source_digest,
    )?;

    let extracted_path = temp_dir.path().join(extracted_binary_name());
    if show_progress {
        eprintln!("Extracting update package...");
    }
    extract_binary(&archive_path, &extracted_path)?;
    verify_candidate_binary(&extracted_path, &extract_release_version(release))?;

    // Keep the mutable-tag check after every expensive local operation. A dev
    // tag move during extraction or candidate execution must be observed before
    // the one irreversible operation below.
    let confirmed_digest = fetch_tag_commit_sha(&client, &release.tag_name).await?;
    if confirmed_digest != source_digest {
        anyhow::bail!(
            "release tag '{}' moved from {source_digest} to {confirmed_digest} during update; \
             refusing to replace the executable",
            release.tag_name
        );
    }

    if show_progress {
        eprintln!("Replacing current executable...");
    }
    replace_candidate(
        &executable,
        &extracted_path,
        update_lease,
        platform,
        &release.tag_name,
    )
}

fn transaction_sibling_path(executable: &Path, suffix: &str) -> Result<PathBuf> {
    let parent = executable
        .parent()
        .with_context(|| format!("current executable has no parent: {}", executable.display()))?;
    let file_name = executable.file_name().with_context(|| {
        format!(
            "current executable has no file name: {}",
            executable.display()
        )
    })?;
    let mut transaction_name = std::ffi::OsString::from(".");
    transaction_name.push(file_name);
    transaction_name.push(suffix);
    Ok(parent.join(transaction_name))
}

pub(crate) fn acquire_self_update_lease() -> Result<SelfUpdateLease> {
    let executable =
        fs::canonicalize(std::env::current_exe().context("locating current executable")?)
            .context("resolving current executable")?;
    Ok(SelfUpdateLease(acquire_update_lease(&executable)?))
}

fn acquire_update_lease(executable: &Path) -> Result<UpdateLease> {
    let lock_path = transaction_sibling_path(executable, ".self-update.lock")?;
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) if !metadata.file_type().is_file() => anyhow::bail!(
            "self-update lock path is not a regular file: {}",
            lock_path.display()
        ),
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("inspecting self-update lock {}", lock_path.display()));
        }
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("opening self-update lock {}", lock_path.display()))?;
    FileExt::lock(&file)
        .with_context(|| format!("locking self-update transaction {}", lock_path.display()))?;
    Ok(UpdateLease {
        _inner: std::sync::Arc::new(UpdateLeaseInner { file }),
    })
}

#[cfg(not(windows))]
fn replace_candidate(
    executable: &Path,
    candidate: &Path,
    update_lease: UpdateLease,
    platform: UpdatePlatform,
    release_tag: &str,
) -> Result<PendingReplacement> {
    self_replace::self_replace(candidate).with_context(|| {
        format!(
            "replacing current executable: {}",
            replacement_permission_hint(executable, platform, release_tag)
        )
    })?;
    Ok(PendingReplacement::new(update_lease))
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsReplaceFaultPoint {
    BeforePublish,
    AfterPublish,
    #[cfg(test)]
    OriginalMovedToBackup,
    #[cfg(test)]
    OriginalMovedToBackupAndBlockRestore,
}

#[cfg(all(windows, test))]
thread_local! {
    static WINDOWS_REPLACE_FAULT: std::cell::Cell<Option<WindowsReplaceFaultPoint>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(windows)]
fn inject_windows_replace_fault(point: WindowsReplaceFaultPoint) -> Result<()> {
    #[cfg(test)]
    if WINDOWS_REPLACE_FAULT.with(|fault| {
        let matches = fault.get() == Some(point);
        if matches {
            fault.set(None);
        }
        matches
    }) {
        anyhow::bail!("injected Windows replacement failure at {point:?}");
    }
    let _ = point;
    Ok(())
}

#[cfg(windows)]
#[derive(Debug)]
enum WindowsReplaceOutcome {
    Success,
    UnchangedFailure(io::Error),
    OriginalMovedToBackupPartialFailure(io::Error),
}

#[cfg(windows)]
fn inject_windows_replace_api_fault(
    replaced: &Path,
    backup: &Path,
) -> Option<WindowsReplaceOutcome> {
    #[cfg(test)]
    {
        let fault = WINDOWS_REPLACE_FAULT.with(|fault| match fault.get() {
            Some(
                point @ (WindowsReplaceFaultPoint::OriginalMovedToBackup
                | WindowsReplaceFaultPoint::OriginalMovedToBackupAndBlockRestore),
            ) => {
                fault.set(None);
                Some(point)
            }
            _ => None,
        });
        if let Some(fault) = fault {
            // Reproduce the documented ERROR_UNABLE_TO_MOVE_REPLACEMENT_2
            // layout: the original has already moved to lpBackupFileName,
            // the replacement remains at its original path, and the public
            // executable path is absent.
            fs::rename(replaced, backup).expect("inject 1177 original-to-backup move");
            if fault == WindowsReplaceFaultPoint::OriginalMovedToBackupAndBlockRestore {
                fs::create_dir(replaced).expect("inject a blocker at the executable path");
            }
            return Some(WindowsReplaceOutcome::OriginalMovedToBackupPartialFailure(
                io::Error::from_raw_os_error(
                    windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 as i32,
                ),
            ));
        }
    }
    let _ = (replaced, backup);
    None
}

#[cfg(windows)]
fn require_transaction_path_absent(path: &Path, purpose: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("inspecting {purpose} {}", path.display())),
        Ok(_) => anyhow::bail!(
            "{purpose} already exists at {}; refusing to overwrite recovery data",
            path.display()
        ),
    }
}

#[cfg(windows)]
fn stage_windows_candidate(executable: &Path, candidate: &Path) -> Result<PathBuf> {
    let parent = executable
        .parent()
        .with_context(|| format!("current executable has no parent: {}", executable.display()))?;
    let mut source = fs::File::open(candidate)
        .with_context(|| format!("opening update candidate {}", candidate.display()))?;
    let source_len = source
        .metadata()
        .with_context(|| format!("reading update candidate metadata {}", candidate.display()))?
        .len();
    let mut staged = tempfile::Builder::new()
        .prefix(".codex-switch-global-pace.self-update-")
        .suffix(".exe")
        .tempfile_in(parent)
        .with_context(|| format!("staging update beside {}", executable.display()))?;
    let copied = io::copy(&mut source, staged.as_file_mut())
        .with_context(|| format!("copying update candidate {}", candidate.display()))?;
    if copied != source_len {
        anyhow::bail!(
            "staged update length changed while copying: expected {source_len} bytes, copied {copied}"
        );
    }
    staged
        .as_file_mut()
        .sync_all()
        .context("flushing staged Windows update")?;
    let (staged_file, staged_path) = staged
        .keep()
        .map_err(|err| err.error)
        .context("preserving staged Windows update for atomic replacement")?;
    drop(staged_file);
    Ok(staged_path)
}

#[cfg(windows)]
fn replace_file_windows(
    replaced: &Path,
    replacement: &Path,
    backup: &Path,
) -> WindowsReplaceOutcome {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2;
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    if let Some(outcome) = inject_windows_replace_api_fault(replaced, backup) {
        return outcome;
    }

    let replaced_wide: Vec<u16> = replaced.as_os_str().encode_wide().chain(Some(0)).collect();
    let replacement_wide: Vec<u16> = replacement
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let backup_wide: Vec<u16> = backup.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: all three buffers are stable, NUL-terminated UTF-16 paths for the
    // duration of the call; the optional pointer parameters are intentionally null.
    let replace_succeeded = unsafe {
        ReplaceFileW(
            replaced_wide.as_ptr(),
            replacement_wide.as_ptr(),
            backup_wide.as_ptr(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replace_succeeded != 0 {
        return WindowsReplaceOutcome::Success;
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 as i32) {
        WindowsReplaceOutcome::OriginalMovedToBackupPartialFailure(error)
    } else {
        WindowsReplaceOutcome::UnchangedFailure(error)
    }
}

#[cfg(windows)]
fn remove_windows_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn restore_windows_original_after_partial_replace(
    executable: &Path,
    original_backup: &Path,
    retained_replacement: &Path,
) -> Result<()> {
    match fs::symlink_metadata(executable) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "inspecting the empty executable path {}; recovery files were preserved at {} and {}",
                    executable.display(),
                    original_backup.display(),
                    retained_replacement.display()
                )
            });
        }
        Ok(_) => anyhow::bail!(
            "executable path {} is no longer empty; recovery files were preserved at {} and {}",
            executable.display(),
            original_backup.display(),
            retained_replacement.display()
        ),
    }
    match fs::symlink_metadata(original_backup) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => anyhow::bail!(
            "original backup is not a regular file at {}; executable path {} and retained replacement {} were preserved",
            original_backup.display(),
            executable.display(),
            retained_replacement.display()
        ),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "inspecting original backup {}; executable path {} and retained replacement {} were preserved",
                    original_backup.display(),
                    executable.display(),
                    retained_replacement.display()
                )
            });
        }
    }
    fs::rename(original_backup, executable).with_context(|| {
        format!(
            "restoring original executable {} from {}; retained replacement remains at {}",
            executable.display(),
            original_backup.display(),
            retained_replacement.display()
        )
    })
}

#[cfg(windows)]
fn windows_replace_error(error: io::Error, replaced: &Path, replacement: &Path) -> anyhow::Error {
    anyhow::Error::new(error).context(format!(
        "atomically replacing {} with {}",
        replaced.display(),
        replacement.display()
    ))
}

#[cfg(windows)]
fn replace_candidate(
    executable: &Path,
    candidate: &Path,
    update_lease: UpdateLease,
    platform: UpdatePlatform,
    release_tag: &str,
) -> Result<PendingReplacement> {
    let backup = transaction_sibling_path(executable, ".self-update-backup.exe")?;
    let failed_candidate = transaction_sibling_path(executable, ".self-update-failed.exe")?;
    require_transaction_path_absent(&backup, "self-update backup")?;
    require_transaction_path_absent(&failed_candidate, "failed self-update candidate")?;
    let staged = stage_windows_candidate(executable, candidate)?;

    if let Err(err) = inject_windows_replace_fault(WindowsReplaceFaultPoint::BeforePublish) {
        let _ = fs::remove_file(&staged);
        return Err(err.context("Windows self-update stopped before publication"));
    }
    match replace_file_windows(executable, &staged, &backup) {
        WindowsReplaceOutcome::Success => {}
        WindowsReplaceOutcome::UnchangedFailure(error) => {
            let error = windows_replace_error(error, executable, &staged);
            if let Err(cleanup_error) = remove_windows_file_if_present(&staged) {
                return Err(error.context(format!(
                    "Windows replacement left the original executable unchanged, but staged candidate cleanup failed at {}: {cleanup_error}",
                    staged.display()
                )));
            }
            return Err(error.context(format!(
                "replacing current executable: {}",
                replacement_permission_hint(executable, platform, release_tag)
            )));
        }
        WindowsReplaceOutcome::OriginalMovedToBackupPartialFailure(error) => {
            let error = windows_replace_error(error, executable, &staged);
            if let Err(recovery_error) =
                restore_windows_original_after_partial_replace(executable, &backup, &staged)
            {
                return Err(error.context(format!(
                    "ReplaceFileW returned ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 (1177) after moving the original executable to {}; automatic recovery failed: {recovery_error}. Preserve the original backup at {}, the staged replacement at {}, and the executable path {} for manual recovery",
                    backup.display(),
                    backup.display(),
                    staged.display(),
                    executable.display()
                )));
            }
            if let Err(cleanup_error) = remove_windows_file_if_present(&staged) {
                return Err(error.context(format!(
                    "ReplaceFileW returned ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 (1177); the original executable was restored at {}, but the staged replacement could not be removed from {}: {cleanup_error}",
                    executable.display(),
                    staged.display()
                )));
            }
            return Err(error.context(format!(
                "ReplaceFileW returned ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 (1177); the original executable was restored at {} and the update was not published",
                executable.display()
            )));
        }
    }

    let mut pending = PendingReplacement::new(
        update_lease,
        executable.to_path_buf(),
        backup,
        failed_candidate,
    );
    if let Err(err) = inject_windows_replace_fault(WindowsReplaceFaultPoint::AfterPublish) {
        if let Err(rollback_err) = pending.rollback() {
            return Err(err.context(format!(
                "Windows replacement failed after publication and rollback also failed: {rollback_err}"
            )));
        }
        return Err(err.context(
            "Windows replacement failed after publication; previous executable was restored",
        ));
    }
    Ok(pending)
}

#[cfg(windows)]
fn rollback_windows_replacement(
    executable: &Path,
    backup: &Path,
    failed_candidate: &Path,
) -> Result<()> {
    require_transaction_path_absent(failed_candidate, "failed self-update candidate")?;
    match replace_file_windows(executable, backup, failed_candidate) {
        WindowsReplaceOutcome::Success => {}
        WindowsReplaceOutcome::UnchangedFailure(error) => {
            return Err(windows_replace_error(error, executable, backup).context(format!(
                "restoring previous executable {} from {}; the previous executable remains preserved at {}",
                executable.display(),
                backup.display(),
                backup.display()
            )));
        }
        WindowsReplaceOutcome::OriginalMovedToBackupPartialFailure(error) => {
            let error = windows_replace_error(error, executable, backup);
            if let Err(recovery_error) =
                restore_windows_original_after_partial_replace(executable, backup, failed_candidate)
            {
                return Err(error.context(format!(
                    "rollback ReplaceFileW returned ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 (1177); automatic recovery failed: {recovery_error}. Preserve the previous executable at {}, the failed candidate at {}, and the executable path {} for manual recovery",
                    backup.display(),
                    failed_candidate.display(),
                    executable.display()
                )));
            }
            tracing::warn!(
                "Rollback ReplaceFileW returned ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 (1177), but the previous executable was restored at {}",
                executable.display()
            );
        }
    }
    if let Err(err) = remove_windows_file_if_present(failed_candidate) {
        tracing::warn!(
            "Previous executable was restored, but failed update cleanup at {} was deferred: {err}",
            failed_candidate.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn cleanup_committed_windows_backup(backup: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_DELETE_ON_CLOSE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    };

    match fs::remove_file(backup) {
        Ok(()) => return Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(remove_err) => {
            let delete_on_exit = fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
                .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
                .open(backup)
                .map_err(|schedule_err| {
                    anyhow::anyhow!(
                        "removing old executable {} failed ({remove_err}); marking it for deletion on process exit also failed: {schedule_err}",
                        backup.display()
                    )
                })?;
            // Keep the delete-on-close handle alive until this process releases
            // the image section for the old executable.
            std::mem::forget(delete_on_exit);
        }
    }
    Ok(())
}

fn candidate_version_line(stdout: &str) -> Option<&str> {
    stdout
        .lines()
        .next()
        .and_then(|line| line.trim().strip_prefix(BIN_NAME))
        .and_then(|version| version.strip_prefix(' '))
        .filter(|version| !version.is_empty())
}

fn verify_candidate_binary(path: &Path, expected_version: &str) -> Result<()> {
    Version::parse(expected_version).with_context(|| {
        format!("release metadata contains invalid version '{expected_version}'")
    })?;
    let output = std::process::Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("executing downloaded candidate {}", path.display()))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if detail.is_empty() {
            "no diagnostic output".to_string()
        } else {
            detail
        };
        anyhow::bail!(
            "downloaded candidate {} failed `--version` ({}): {}",
            path.display(),
            output.status,
            detail
        );
    }
    let stdout = String::from_utf8(output.stdout).with_context(|| {
        format!(
            "candidate {} emitted non-UTF-8 version output",
            path.display()
        )
    })?;
    let reported = candidate_version_line(&stdout).ok_or_else(|| {
        anyhow::anyhow!(
            "downloaded candidate {} did not report a `{BIN_NAME} <version>` line",
            path.display()
        )
    })?;
    if reported != expected_version {
        anyhow::bail!(
            "downloaded candidate {} reported version {reported}, but release metadata requires {expected_version}",
            path.display()
        );
    }
    Ok(())
}

fn attestation_verify_args(
    archive_path: &Path,
    bundle_path: &Path,
    release_tag: &str,
    source_digest: &str,
) -> Vec<String> {
    vec![
        "attestation".to_string(),
        "verify".to_string(),
        archive_path.to_string_lossy().into_owned(),
        "--bundle".to_string(),
        bundle_path.to_string_lossy().into_owned(),
        "--repo".to_string(),
        format!("{REPO_OWNER}/{REPO_NAME}"),
        "--signer-workflow".to_string(),
        RELEASE_WORKFLOW.to_string(),
        "--source-ref".to_string(),
        format!("refs/tags/{release_tag}"),
        "--source-digest".to_string(),
        source_digest.to_string(),
        "--deny-self-hosted-runners".to_string(),
    ]
}

fn verify_build_provenance(
    archive_path: &Path,
    bundle_path: &Path,
    release_tag: &str,
    source_digest: &str,
) -> Result<()> {
    let args = attestation_verify_args(archive_path, bundle_path, release_tag, source_digest);
    let output = std::process::Command::new("gh")
        .args(&args)
        .output()
        .with_context(|| {
            "running `gh attestation verify`; install a GitHub CLI version with attestation \
             support before using self-update"
        })?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    anyhow::bail!(
        "release provenance verification failed for {}: {}",
        archive_path.display(),
        if detail.is_empty() {
            "gh attestation verify returned a non-zero status"
        } else {
            detail
        }
    )
}

/// Returns true if the given version string contains a pre-release component
/// (e.g. `20260712.1.0-dev`; legacy timestamped versions also match).
pub fn is_dev_version(version: &str) -> bool {
    normalize_version(version).contains("-dev")
}

pub fn detect_install_source() -> InstallSource {
    let exe = std::env::current_exe().ok();
    let exe = exe
        .as_ref()
        .and_then(|path| fs::canonicalize(path).ok())
        .or(exe)
        .unwrap_or_else(|| PathBuf::from(BIN_NAME));
    let path = exe.to_string_lossy().replace('\\', "/");

    if path.contains("/Cellar/codex-switch-global-pace/")
        || path.contains("/Homebrew/Cellar/codex-switch-global-pace/")
    {
        InstallSource::Homebrew
    } else {
        InstallSource::Direct
    }
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn should_show_download_progress() -> bool {
    io::stderr().is_terminal()
}

async fn latest_release_version(force: bool) -> Result<String> {
    if !force
        && let Some(cache) = load_update_cache()
        && crate::auth::now_unix_secs() - cache.checked_at <= update_ttl_secs()
    {
        return Ok(cache.latest_version);
    }

    let release = fetch_release(None).await?;
    let latest_version = normalize_version(&release.tag_name);
    save_update_cache(&UpdateCache {
        checked_at: crate::auth::now_unix_secs(),
        latest_version: latest_version.clone(),
    });
    Ok(latest_version)
}

async fn fetch_release(version: Option<&str>) -> Result<GithubRelease> {
    fetch_release_inner(version).await?.ok_or_else(|| {
        let requested = version
            .map(|value| format!(" matching '{value}'"))
            .unwrap_or_default();
        anyhow::anyhow!(
            "self-update is unavailable: {REPO_OWNER}/{REPO_NAME} has no GitHub Release{requested}"
        )
    })
}

/// Fetch a GitHub Release, returning `Ok(None)` for 404 (release not found)
/// and propagating all other errors.
async fn fetch_release_optional(version: Option<&str>) -> Result<Option<GithubRelease>> {
    fetch_release_inner(version).await
}

async fn fetch_release_inner(version: Option<&str>) -> Result<Option<GithubRelease>> {
    let client =
        crate::auth::build_http_client().context("building HTTP client for update check")?;
    let url = release_api_url(version);
    let resp = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .context("requesting GitHub release metadata")?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    let release = resp
        .error_for_status()
        .context("GitHub release request failed")?
        .json::<GithubRelease>()
        .await
        .context("parsing GitHub release metadata")?;
    Ok(Some(release))
}

async fn fetch_tag_commit_sha(client: &reqwest::Client, tag: &str) -> Result<String> {
    let reference = fetch_github_json::<GithubGitReference>(
        client,
        &tag_ref_api_url(tag),
        "requesting GitHub release tag reference",
    )
    .await?;
    let mut object = reference.object;
    for _ in 0..5 {
        match object.kind.as_str() {
            "commit" => {
                validate_commit_sha(&object.sha)?;
                return Ok(object.sha.to_ascii_lowercase());
            }
            "tag" => {
                let tag_object = fetch_github_json::<GithubGitTag>(
                    client,
                    &git_tag_api_url(&object.sha),
                    "resolving annotated GitHub release tag",
                )
                .await?;
                object = tag_object.object;
            }
            other => anyhow::bail!(
                "release tag '{tag}' resolved to unsupported Git object type '{other}'"
            ),
        }
    }
    anyhow::bail!("release tag '{tag}' contains more than 5 nested annotated tags")
}

async fn fetch_github_json<T>(client: &reqwest::Client, url: &str, context: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .with_context(|| context.to_string())?
        .error_for_status()
        .with_context(|| format!("{context}: {url}"))?
        .json::<T>()
        .await
        .with_context(|| format!("parsing GitHub response from {url}"))
}

fn validate_commit_sha(sha: &str) -> Result<()> {
    if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("GitHub release tag returned an invalid commit SHA: '{sha}'");
    }
    Ok(())
}

async fn download_file(client: &reqwest::Client, url: &str, path: &Path) -> Result<()> {
    let bytes = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed for {url}"))?
        .bytes()
        .await
        .with_context(|| format!("reading response body from {url}"))?;
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

async fn verify_checksum(client: &reqwest::Client, url: &str, archive_path: &Path) -> Result<()> {
    let checksum_text = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("checksum download failed for {url}"))?
        .text()
        .await
        .with_context(|| format!("reading checksum response from {url}"))?;

    let expected = extract_checksum_digest(&checksum_text)
        .context("checksum file did not contain a SHA256 digest")?;

    let actual = {
        let bytes = fs::read(archive_path)
            .with_context(|| format!("reading downloaded asset {}", archive_path.display()))?;
        hex::encode(Sha256::digest(&bytes))
    };

    if !checksum_matches(expected, &actual) {
        anyhow::bail!(
            "SHA256 mismatch for {} (expected {}, got {})",
            archive_path.display(),
            expected,
            actual
        );
    }

    Ok(())
}

fn extract_checksum_digest(checksum_text: &str) -> Option<&str> {
    checksum_text
        .split_whitespace()
        .next()
        .filter(|value| !value.is_empty())
}

fn checksum_matches(expected: &str, actual: &str) -> bool {
    expected.eq_ignore_ascii_case(actual)
}

fn extract_binary(archive_path: &Path, output_path: &Path) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }

    let binary_name = extracted_binary_name();
    if archive_path.extension().and_then(|ext| ext.to_str()) == Some("zip") {
        extract_zip_binary(archive_path, &binary_name, output_path)?;
    } else {
        extract_tar_gz_binary(archive_path, &binary_name, output_path)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut perms = fs::metadata(output_path)
            .with_context(|| format!("reading metadata for {}", output_path.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(output_path, perms)
            .with_context(|| format!("setting permissions on {}", output_path.display()))?;
    }

    Ok(())
}

fn extract_tar_gz_binary(archive_path: &Path, binary_name: &str, output_path: &Path) -> Result<()> {
    let file = fs::File::open(archive_path)
        .with_context(|| format!("opening archive {}", archive_path.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries().context("listing tar archive entries")? {
        let mut entry = entry.context("reading tar archive entry")?;
        let path = entry.path().context("reading tar entry path")?;
        if path.file_name().and_then(|name| name.to_str()) == Some(binary_name) {
            let mut out = fs::File::create(output_path)
                .with_context(|| format!("creating {}", output_path.display()))?;
            io::copy(&mut entry, &mut out)
                .with_context(|| format!("extracting {}", output_path.display()))?;
            return Ok(());
        }
    }

    anyhow::bail!(
        "binary '{}' not found inside {}",
        binary_name,
        archive_path.display()
    );
}

fn extract_zip_binary(archive_path: &Path, binary_name: &str, output_path: &Path) -> Result<()> {
    let file = fs::File::open(archive_path)
        .with_context(|| format!("opening archive {}", archive_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("opening zip archive")?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("reading zip entry #{index}"))?;
        let name = entry.name().replace('\\', "/");
        if Path::new(&name)
            .file_name()
            .and_then(|value| value.to_str())
            == Some(binary_name)
        {
            let mut out = fs::File::create(output_path)
                .with_context(|| format!("creating {}", output_path.display()))?;
            io::copy(&mut entry, &mut out)
                .with_context(|| format!("extracting {}", output_path.display()))?;
            return Ok(());
        }
    }

    anyhow::bail!(
        "binary '{}' not found inside {}",
        binary_name,
        archive_path.display()
    );
}

fn asset_name() -> String {
    if cfg!(target_os = "windows") {
        format!("{BIN_NAME}-{}.zip", release_target())
    } else {
        format!("{BIN_NAME}-{}.tar.gz", release_target())
    }
}

fn extracted_binary_name() -> String {
    if cfg!(target_os = "windows") {
        format!("{BIN_NAME}.exe")
    } else {
        BIN_NAME.to_string()
    }
}

fn release_target() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{platform}-{arch}")
}

fn release_tag(version: &str) -> String {
    let version = version.trim();
    // The dev channel uses the bare tag "dev", not "vdev".
    if version == "dev" {
        return "dev".to_string();
    }
    if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    }
}

fn release_api_url(version: Option<&str>) -> String {
    let base = github_api_base();

    match version {
        // Encoded for the same reason as `tag_ref_api_url`: the tag is a path
        // segment, and `url` would otherwise resolve `..` inside it and send
        // the request to a different repository.
        Some(version) => format!(
            "{base}/repos/{REPO_OWNER}/{REPO_NAME}/releases/tags/{}",
            urlencoding::encode(&release_tag(version))
        ),
        None => format!("{base}/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest"),
    }
}

fn tag_ref_api_url(tag: &str) -> String {
    format!(
        "{}/repos/{REPO_OWNER}/{REPO_NAME}/git/ref/tags/{}",
        github_api_base(),
        urlencoding::encode(tag)
    )
}

fn git_tag_api_url(sha: &str) -> String {
    format!(
        "{}/repos/{REPO_OWNER}/{REPO_NAME}/git/tags/{sha}",
        github_api_base()
    )
}

fn github_api_base() -> String {
    std::env::var("CS_GITHUB_API_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string())
}

fn normalize_version(version: &str) -> String {
    version.trim().trim_start_matches('v').to_string()
}

/// Normalize a `--version` argument, rejecting anything that is not a plain
/// semantic version.
///
/// The value reaches `release_api_url` as a path segment. `url` resolves `..`
/// segments per the WHATWG spec, so an unencoded traversal would walk the
/// request onto another repository's release metadata. `release_api_url` now
/// percent-encodes, which contains the value; this rejects it outright so the
/// safety of that path never rests on a downstream string comparison, and so a
/// typo is reported as a bad argument rather than as a 404.
fn validate_requested_version(version: &str) -> Result<String> {
    let normalized = normalize_version(version);
    Version::parse(&normalized).map_err(|err| {
        anyhow::anyhow!(
            "invalid --version '{version}': expected a semantic version such as 20260731.1.0 ({err}). \
             Use --dev for the rolling development build."
        )
    })?;
    Ok(normalized)
}

fn update_ttl_secs() -> i64 {
    std::env::var("CS_UPDATE_TTL_SECS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value >= 0)
        .unwrap_or(UPDATE_TTL_SECS)
}

fn update_cache_path() -> anyhow::Result<PathBuf> {
    // Account/profile data intentionally remains shared with codex-switch, but
    // release metadata cannot be shared because the two programs update from
    // different repositories.
    Ok(crate::auth::app_home()?.join(UPDATE_CACHE_NAME))
}

fn load_update_cache() -> Option<UpdateCache> {
    let path = update_cache_path().ok()?;
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_update_cache(cache: &UpdateCache) {
    let path = match update_cache_path() {
        Ok(p) => p,
        Err(_) => return,
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = fs::write(path, json);
    }
}

fn is_newer_version(candidate: &str, current: &str) -> bool {
    compare_versions(candidate, current)
        .is_some_and(|ordering| ordering == std::cmp::Ordering::Greater)
}

fn is_older_version(candidate: &str, current: &str) -> bool {
    compare_versions(candidate, current)
        .is_some_and(|ordering| ordering == std::cmp::Ordering::Less)
}

fn is_dev_update_available(candidate: &str, current: &str) -> bool {
    if is_newer_version(candidate, current) {
        return true;
    }
    if is_dev_version(current) && is_dev_version(candidate) {
        let candidate = Version::parse(&normalize_version(candidate)).ok();
        let current = Version::parse(&normalize_version(current)).ok();
        return matches!((candidate, current), (Some(candidate), Some(current))
            if candidate.major == current.major
                && candidate.minor == current.minor
                && candidate.patch == current.patch
                && candidate.pre.as_str() == "dev"
                && current.pre.as_str().starts_with("dev."));
    }
    // Explicit --dev should be able to switch from a stable/base install to the
    // rolling dev build with the same base version, e.g. 20260712.1.0 -> 20260712.1.0-dev.
    if !is_dev_version(candidate) {
        return false;
    }
    let Some(candidate_base) = version_base(candidate) else {
        return false;
    };
    let Some(current_base) = version_base(current) else {
        return false;
    };
    candidate_base >= current_base
}

fn version_base(version: &str) -> Option<(u64, u64, u64)> {
    let parsed = match Version::parse(&normalize_version(version)) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to parse version '{version}': {e}");
            return None;
        }
    };
    Some((parsed.major, parsed.minor, parsed.patch))
}

fn compare_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let left_parsed = match Version::parse(&normalize_version(left)) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to parse version '{left}': {e}");
            return None;
        }
    };
    let right_parsed = match Version::parse(&normalize_version(right)) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("failed to parse version '{right}': {e}");
            return None;
        }
    };
    Some(left_parsed.cmp(&right_parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_assets_and_update_cache_are_project_namespaced() {
        assert!(asset_name().starts_with("codex-switch-global-pace-"));
        assert!(extracted_binary_name().starts_with("codex-switch-global-pace"));
        assert_eq!(UPDATE_CACHE_NAME, "global-pace-update-check.json");
    }

    #[test]
    fn candidate_version_requires_the_exact_binary_prefix_and_first_line() {
        assert_eq!(
            candidate_version_line(
                "codex-switch-global-pace 20260824.7.0-dev\nhttps://github.com/example\n"
            ),
            Some("20260824.7.0-dev")
        );
        assert_eq!(
            candidate_version_line("other-binary 20260824.7.0-dev\n"),
            None
        );
        assert_eq!(candidate_version_line("\n20260824.7.0-dev\n"), None);
    }

    #[cfg(windows)]
    fn set_windows_replace_fault(point: WindowsReplaceFaultPoint) {
        WINDOWS_REPLACE_FAULT.with(|fault| {
            assert!(fault.replace(Some(point)).is_none());
        });
    }

    #[cfg(windows)]
    fn windows_replacement_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().expect("create replacement fixture");
        let executable = temp.path().join("codex-switch-global-pace.exe");
        let candidate = temp.path().join("candidate.exe");
        fs::write(&executable, b"old executable").expect("write old executable");
        fs::write(&candidate, b"new executable").expect("write candidate executable");
        (temp, executable, candidate)
    }

    #[test]
    fn self_update_lease_serializes_concurrent_replacements() {
        let temp = tempfile::tempdir().expect("create lease fixture");
        let executable = temp.path().join(extracted_binary_name());
        fs::write(&executable, b"current executable").expect("write executable fixture");
        let first = acquire_update_lease(&executable).expect("acquire first update lease");
        let second_executable = executable.clone();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let second =
                acquire_update_lease(&second_executable).expect("acquire second update lease");
            acquired_tx.send(()).expect("report second acquisition");
            drop(second);
        });

        let acquired_while_held = acquired_rx
            .recv_timeout(std::time::Duration::from_millis(150))
            .is_ok();
        drop(first);

        assert!(
            !acquired_while_held,
            "second self-update entered while the first transaction still held its lease"
        );
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("second self-update did not resume after lease release");
        waiter.join().expect("join update-lease waiter");
    }

    #[cfg(windows)]
    #[test]
    fn windows_failure_before_publish_leaves_the_old_executable_untouched() {
        let (_temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        set_windows_replace_fault(WindowsReplaceFaultPoint::BeforePublish);

        let error = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .unwrap_err();

        assert!(error.to_string().contains("before publication"), "{error}");
        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        assert!(
            !transaction_sibling_path(&executable, ".self-update-backup.exe")
                .unwrap()
                .exists()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_1177_during_publish_restores_the_original_executable() {
        let (_temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        set_windows_replace_fault(WindowsReplaceFaultPoint::OriginalMovedToBackup);

        let error = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .expect_err("the injected ReplaceFileW 1177 must fail publication");

        assert!(
            error
                .to_string()
                .contains("ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 (1177)"),
            "{error:#}"
        );
        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        assert!(
            !transaction_sibling_path(&executable, ".self-update-backup.exe")
                .unwrap()
                .exists()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_1177_publish_recovery_failure_preserves_every_recovery_path() {
        let (temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        let backup = transaction_sibling_path(&executable, ".self-update-backup.exe").unwrap();
        set_windows_replace_fault(WindowsReplaceFaultPoint::OriginalMovedToBackupAndBlockRestore);

        let error = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .expect_err("the blocked 1177 recovery must fail closed");
        let staged = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .starts_with(".codex-switch-global-pace.self-update-")
                })
            })
            .expect("the staged replacement must be preserved");

        let message = error.to_string();
        assert!(message.contains("automatic recovery failed"), "{error:#}");
        assert!(message.contains(&backup.display().to_string()), "{error:#}");
        assert!(message.contains(&staged.display().to_string()), "{error:#}");
        assert!(
            message.contains(&executable.display().to_string()),
            "{error:#}"
        );
        assert!(
            executable.is_dir(),
            "the injected path blocker is preserved"
        );
        assert_eq!(fs::read(&backup).unwrap(), b"old executable");
        assert_eq!(fs::read(&staged).unwrap(), b"new executable");

        fs::remove_dir(&executable).unwrap();
        fs::remove_file(&backup).unwrap();
        fs::remove_file(&staged).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_failure_after_publish_atomically_restores_the_old_executable() {
        let (_temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        set_windows_replace_fault(WindowsReplaceFaultPoint::AfterPublish);

        let error = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("previous executable was restored"),
            "{error}"
        );
        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        assert!(
            !transaction_sibling_path(&executable, ".self-update-backup.exe")
                .unwrap()
                .exists()
        );
        assert!(
            !transaction_sibling_path(&executable, ".self-update-failed.exe")
                .unwrap()
                .exists()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_pending_replacement_can_be_rolled_back_exactly() {
        let (_temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        let mut replacement = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .expect("publish candidate");
        assert_eq!(fs::read(&executable).unwrap(), b"new executable");

        replacement.rollback().expect("restore old executable");

        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        assert!(
            !transaction_sibling_path(&executable, ".self-update-backup.exe")
                .unwrap()
                .exists()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_1177_during_rollback_still_restores_the_previous_executable() {
        let (_temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        let mut replacement = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .expect("publish candidate");
        set_windows_replace_fault(WindowsReplaceFaultPoint::OriginalMovedToBackup);

        replacement
            .rollback()
            .expect("1177 rollback recovery must restore the previous executable");

        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        assert!(
            !transaction_sibling_path(&executable, ".self-update-backup.exe")
                .unwrap()
                .exists()
        );
        assert!(
            !transaction_sibling_path(&executable, ".self-update-failed.exe")
                .unwrap()
                .exists()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_1177_rollback_recovery_failure_preserves_backup_and_failed_candidate() {
        let (_temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        let mut replacement = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .expect("publish candidate");
        let backup = transaction_sibling_path(&executable, ".self-update-backup.exe").unwrap();
        let failed = transaction_sibling_path(&executable, ".self-update-failed.exe").unwrap();
        set_windows_replace_fault(WindowsReplaceFaultPoint::OriginalMovedToBackupAndBlockRestore);

        let error = replacement
            .rollback()
            .expect_err("the blocked rollback recovery must fail closed");

        let message = error.to_string();
        assert!(message.contains("automatic recovery failed"), "{error:#}");
        assert!(message.contains(&backup.display().to_string()), "{error:#}");
        assert!(message.contains(&failed.display().to_string()), "{error:#}");
        assert!(
            message.contains(&executable.display().to_string()),
            "{error:#}"
        );
        assert!(
            executable.is_dir(),
            "the injected path blocker is preserved"
        );
        assert_eq!(fs::read(&backup).unwrap(), b"old executable");
        assert_eq!(fs::read(&failed).unwrap(), b"new executable");

        fs::remove_dir(&executable).unwrap();
        fs::remove_file(&backup).unwrap();
        fs::remove_file(&failed).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_dropping_pending_replacement_preserves_the_old_executable_backup() {
        let (_temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        let replacement = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .expect("publish candidate");
        let backup = transaction_sibling_path(&executable, ".self-update-backup.exe").unwrap();

        drop(replacement);

        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
        assert_eq!(fs::read(&backup).unwrap(), b"old executable");
    }

    #[cfg(windows)]
    #[test]
    fn windows_manual_recovery_exposes_exact_preserved_paths() {
        let (_temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        let replacement = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .expect("publish candidate");
        let backup = transaction_sibling_path(&executable, ".self-update-backup.exe").unwrap();
        let mut result = SelfUpdateResult {
            current_version: "1.0.0".to_string(),
            latest_version: "2.0.0".to_string(),
            install_source: InstallSource::Direct,
            updated: true,
            replacement: Some(replacement),
            transaction_lease: None,
        };

        let recovery = result
            .preserve_replacement_for_recovery()
            .expect("preserve pending replacement");

        assert_eq!(recovery.executable, executable);
        assert_eq!(recovery.previous_executable, backup);
        assert_eq!(fs::read(&recovery.executable).unwrap(), b"new executable");
        assert_eq!(
            fs::read(&recovery.previous_executable).unwrap(),
            b"old executable"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_commit_removes_the_recovery_backup_after_health_is_confirmed() {
        let (_temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        let mut replacement = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .expect("publish candidate");
        let backup = transaction_sibling_path(&executable, ".self-update-backup.exe").unwrap();
        assert!(backup.exists());

        replacement.commit();

        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
        assert!(!backup.exists());
    }

    #[test]
    fn attestation_verification_pins_repository_workflow_and_release_ref() {
        let archive = Path::new("/tmp/codex-switch-global-pace-linux-amd64.tar.gz");
        let bundle = Path::new("/tmp/codex-switch-global-pace-build-provenance.json");
        let source_digest = "0123456789abcdef0123456789abcdef01234567";
        let args = attestation_verify_args(archive, bundle, "v20260729.2.0", source_digest);

        assert_eq!(
            args,
            vec![
                "attestation",
                "verify",
                "/tmp/codex-switch-global-pace-linux-amd64.tar.gz",
                "--bundle",
                "/tmp/codex-switch-global-pace-build-provenance.json",
                "--repo",
                "chriskooCK/codex-switch-global-pace",
                "--signer-workflow",
                "chriskooCK/codex-switch-global-pace/.github/workflows/release.yml",
                "--source-ref",
                "refs/tags/v20260729.2.0",
                "--source-digest",
                source_digest,
                "--deny-self-hosted-runners",
            ]
        );
    }

    #[test]
    fn version_compare_ignores_v_prefix() {
        assert!(is_newer_version("v0.0.2", "0.0.1"));
        assert!(is_older_version("0.0.1", "v0.0.2"));
    }

    #[test]
    fn calendar_versions_remain_semver_comparable() {
        assert!(Version::parse("20260712.1").is_err());
        assert!(Version::parse("20260712.1.0").is_ok());
        assert!(is_newer_version("20260712.1.0", "0.0.21"));
        assert!(is_newer_version(
            "20260712.1.0-dev.20260712000000",
            "0.0.22-dev.20260711000000"
        ));
        assert!(is_newer_version("20260712.2.0", "20260712.1.0"));
        assert!(is_newer_version("20260713.1.0", "20260712.9.0"));
        assert!(is_newer_version(
            "20260712.1.0",
            "20260712.1.0-dev.20260712000000"
        ));
        assert!(is_dev_update_available(
            "20260712.1.0-dev.20260712000000",
            "20260712.1.0"
        ));
    }

    #[test]
    fn calendar_stable_release_upgrades_every_supported_legacy_version_family() {
        let stable = "20260713.1.0";
        for current in [
            "0.0.21",
            "0.0.22-dev.20260711000000",
            "20260712.1.0-dev.20260712000000",
            "20260712.2.0-dev",
        ] {
            assert!(
                is_newer_version(stable, current),
                "{current} must be able to graduate to stable {stable}"
            );
        }
    }

    #[test]
    fn release_api_url_uses_latest_or_tag_endpoint() {
        assert_eq!(
            release_api_url(None),
            "https://api.github.com/repos/chriskooCK/codex-switch-global-pace/releases/latest"
        );
        assert_eq!(
            release_api_url(Some("0.1.0")),
            "https://api.github.com/repos/chriskooCK/codex-switch-global-pace/releases/tags/v0.1.0"
        );
    }

    /// `--version` is interpolated into a GitHub API path. `url` resolves `..`
    /// segments per the WHATWG spec, so an unencoded value can walk the request
    /// out of this repository and onto another one's release metadata. Sibling
    /// `tag_ref_api_url` already encodes; this closes the inconsistency rather
    /// than leaving the safety of the path to a downstream string comparison.
    #[test]
    fn release_api_url_percent_encodes_the_requested_version() {
        let url = release_api_url(Some("0.1.0/../../../../../attacker/evil/releases/latest"));

        assert!(
            !url.contains("/../"),
            "path traversal survived encoding: {url}"
        );
        assert!(
            url.starts_with(
                "https://api.github.com/repos/chriskooCK/codex-switch-global-pace/releases/tags/"
            ),
            "the request must stay inside this repository: {url}"
        );
    }

    /// The encoding above keeps a hostile value inside its path segment; this
    /// rejects it outright, before any request is built, so the error names the
    /// bad input instead of surfacing as a confusing 404.
    #[test]
    fn a_requested_version_that_is_not_semver_is_rejected_before_any_request() {
        assert_eq!(
            validate_requested_version("20260731.1.0").unwrap(),
            "20260731.1.0"
        );
        assert_eq!(
            validate_requested_version("v20260731.1.0").unwrap(),
            "20260731.1.0"
        );

        let err = validate_requested_version("0.1.0/../../../../../attacker/evil/releases/latest")
            .unwrap_err();
        assert!(err.to_string().contains("invalid --version"), "{err}");

        // The dev channel is reached with `--dev`, not by naming a tag: this
        // has never resolved, and now says so instead of 404-ing.
        assert!(validate_requested_version("dev").is_err());
    }

    #[test]
    fn release_tag_dev_has_no_v_prefix() {
        assert_eq!(release_tag("dev"), "dev");
        assert_eq!(release_tag("0.1.0"), "v0.1.0");
        assert_eq!(release_tag("v0.1.0"), "v0.1.0");
    }

    #[test]
    fn release_api_url_dev_uses_dev_tag() {
        assert_eq!(
            release_api_url(Some("dev")),
            "https://api.github.com/repos/chriskooCK/codex-switch-global-pace/releases/tags/dev"
        );
    }

    #[test]
    fn tag_ref_api_url_uses_the_exact_release_tag() {
        assert_eq!(
            tag_ref_api_url("dev"),
            "https://api.github.com/repos/chriskooCK/codex-switch-global-pace/git/ref/tags/dev"
        );
        assert_eq!(
            tag_ref_api_url("release/candidate"),
            "https://api.github.com/repos/chriskooCK/codex-switch-global-pace/git/ref/tags/release%2Fcandidate"
        );
    }

    #[test]
    fn commit_digest_must_be_a_full_sha1() {
        validate_commit_sha("0123456789abcdef0123456789abcdef01234567").unwrap();
        assert!(validate_commit_sha("deadbeef").is_err());
        assert!(validate_commit_sha("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
    }

    #[test]
    fn is_dev_version_detects_prerelease() {
        assert!(is_dev_version("1.2.3-dev"));
        assert!(is_dev_version("1.2.3-dev.20260408143000"));
        assert!(is_dev_version("1.2.3-dev+abc1234"));
        assert!(!is_dev_version("1.2.3"));
    }

    #[test]
    fn dev_update_can_switch_from_same_base_stable() {
        assert!(is_dev_update_available(
            "0.0.20-dev.20260701094804",
            "0.0.20"
        ));
        assert!(is_dev_update_available(
            "0.0.20-dev.20260701094804",
            "0.0.20-dev.20260701090000"
        ));
        assert!(!is_dev_update_available(
            "0.0.20-dev.20260701094804",
            "0.0.20-dev.20260701094804"
        ));
        assert!(!is_dev_update_available(
            "0.0.20-dev.20260701094804",
            "0.0.21"
        ));
    }

    #[test]
    fn short_dev_version_replaces_legacy_timestamped_dev_on_the_same_base() {
        assert!(is_dev_update_available(
            "20260712.1.0-dev",
            "20260712.1.0-dev.20260712055522"
        ));
        assert!(!is_dev_update_available(
            "20260712.1.0-dev",
            "20260712.1.0-dev"
        ));
    }

    #[test]
    fn homebrew_dev_hint_avoids_removed_binary_and_unreviewed_pipe_command() {
        let hint = super::homebrew_dev_install_hint();
        assert!(hint.contains("brew uninstall codex-switch-global-pace"));
        assert!(hint.contains(
            "github.com/chriskooCK/codex-switch-global-pace/blob/dev/docs/wiki/Development-Releases.md#install-the-rolling-dev-build"
        ));
        assert!(
            !hint.contains("blob/master/"),
            "hint must point at the reviewed development instructions on dev"
        );
        assert!(!hint.contains("| bash"));
        assert!(!hint.contains("self-update"));
    }

    #[test]
    fn homebrew_dev_error_wraps_the_install_hint_once() {
        let message = super::homebrew_dev_install_error();
        assert!(
            message.contains("To switch to dev, run `brew uninstall codex-switch-global-pace`")
        );
        assert!(!message.contains("run `run `"));
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preflight_rejects_a_read_only_install_directory() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let install_dir = temp.path().join("bin");
        fs::create_dir(&install_dir).unwrap();
        let executable = install_dir.join("codex-switch-global-pace");
        fs::write(&executable, b"old binary").unwrap();
        fs::set_permissions(&install_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let error = ensure_replace_parent_writable(&executable, UpdatePlatform::Unix, "v1.2.3")
            .unwrap_err();

        fs::set_permissions(&install_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(error.to_string().contains("not writable"));
        assert!(error.to_string().contains("user-level installer"));
    }

    #[test]
    fn unix_system_install_hint_separates_legacy_migration_from_explicit_system_updates() {
        let hint = replacement_permission_hint(
            Path::new("/usr/local/bin/codex-switch-global-pace"),
            UpdatePlatform::Unix,
            "dev",
        );

        assert!(hint.contains("legacy direct install"));
        assert!(hint.contains("releases/download/dev/install.sh"));
        assert!(hint.contains("--dev"));
        assert!(hint.contains("intentionally installed with `--system`"));
        assert!(hint.contains("sudo codex-switch-global-pace self-update"));
    }

    #[test]
    fn unix_user_install_hint_never_recommends_sudo() {
        let hint = replacement_permission_hint(
            Path::new("/home/alice/.local/bin/codex-switch-global-pace"),
            UpdatePlatform::Unix,
            "v20260713.3.0",
        );

        assert!(hint.contains("user-owned install directory"));
        assert!(hint.contains("reinstall"));
        assert!(!hint.contains("sudo"));
    }

    #[test]
    fn windows_user_install_hint_never_recommends_administrator() {
        let hint = replacement_permission_hint(
            Path::new(
                r"C:\Users\Alice\AppData\Local\Programs\codex-switch-global-pace\codex-switch-global-pace.exe",
            ),
            UpdatePlatform::Windows,
            "v20260713.3.0",
        );

        assert!(hint.contains("close running codex-switch-global-pace processes"));
        assert!(hint.contains("user-level installer"));
        assert!(!hint.contains("Administrator"));
        assert!(!hint.contains("sudo"));
    }

    #[test]
    fn migration_hint_does_not_embed_an_untrusted_release_tag() {
        let hint = replacement_permission_hint(
            Path::new("/usr/local/bin/codex-switch-global-pace"),
            UpdatePlatform::Unix,
            "v1.2.3;echo-pwned",
        );

        assert!(hint.contains("releases/latest/download/install.sh"));
        assert!(!hint.contains("echo-pwned"));
    }

    #[test]
    fn markerless_unix_system_install_requires_the_dev_installer() {
        let hint = legacy_system_install_migration_hint(
            Path::new("/usr/local/bin/codex-switch-global-pace"),
            UpdatePlatform::Unix,
            false,
            true,
            None,
        )
        .expect("markerless /usr/local install must migrate");

        assert!(hint.contains("One-time setup required"));
        assert!(hint.contains("releases/download/dev/install.sh"));
        assert!(hint.contains("bash -s -- --dev"));
        assert!(hint.contains("--dev --system"));
    }

    #[test]
    fn legacy_migration_message_is_actionable_without_internal_jargon() {
        let hint = legacy_system_install_migration_hint(
            Path::new("/usr/local/bin/codex-switch-global-pace"),
            UpdatePlatform::Unix,
            false,
            true,
            None,
        )
        .expect("markerless /usr/local install must migrate");

        assert!(hint.starts_with("One-time setup required"));
        assert!(hint.contains("Recommended"));
        assert!(hint.contains("Future updates will not need sudo"));
        assert!(hint.contains("Profiles and configuration are preserved"));
        assert!(hint.contains("Keep the system-wide install instead"));
        assert!(hint.contains('\n'));
        assert!(!hint.contains("legacy direct install detected"));
        assert!(!hint.contains("direct self-update is paused"));
    }

    #[test]
    fn markerless_unix_system_install_requires_the_stable_installer() {
        let hint = legacy_system_install_migration_hint(
            Path::new("/usr/local/bin/codex-switch-global-pace"),
            UpdatePlatform::Unix,
            false,
            false,
            None,
        )
        .expect("markerless /usr/local install must migrate");

        assert!(hint.contains("releases/latest/download/install.sh"));
        assert!(!hint.contains("--dev"));
        assert!(hint.contains("--system"));
    }

    #[test]
    fn marked_system_and_user_installs_do_not_enter_legacy_migration() {
        assert!(
            legacy_system_install_migration_hint(
                Path::new("/usr/local/bin/codex-switch-global-pace"),
                UpdatePlatform::Unix,
                true,
                false,
                None,
            )
            .is_none()
        );
        assert!(
            legacy_system_install_migration_hint(
                Path::new("/home/alice/.local/bin/codex-switch-global-pace"),
                UpdatePlatform::Unix,
                false,
                false,
                None,
            )
            .is_none()
        );
        assert!(
            legacy_system_install_migration_hint(
                Path::new(
                    r"C:\Users\Alice\AppData\Local\Programs\codex-switch-global-pace\codex-switch-global-pace.exe",
                ),
                UpdatePlatform::Windows,
                false,
                false,
                None,
            )
            .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn homebrew_symlink_is_resolved_before_legacy_migration_check() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("create temp directory");
        let cellar_dir = temp
            .path()
            .join("Cellar/codex-switch-global-pace/20260713.4.0/bin");
        fs::create_dir_all(&cellar_dir).expect("create fake Cellar path");
        let cellar_binary = cellar_dir.join("codex-switch-global-pace");
        fs::write(&cellar_binary, b"fixture").expect("write fake Homebrew binary");
        let symlink_path = temp.path().join("codex-switch-global-pace");
        symlink(&cellar_binary, &symlink_path).expect("create Homebrew symlink");

        let resolved = canonical_executable_path(symlink_path);
        assert!(
            resolved
                .to_string_lossy()
                .contains("/Cellar/codex-switch-global-pace/")
        );
        assert!(
            legacy_system_install_migration_hint(
                &resolved,
                UpdatePlatform::Unix,
                false,
                false,
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn markerless_system_install_preserves_an_exact_requested_version() {
        let hint = legacy_system_install_migration_hint(
            Path::new("/usr/local/bin/codex-switch-global-pace"),
            UpdatePlatform::Unix,
            false,
            false,
            Some("v20260712.2.0"),
        )
        .expect("markerless /usr/local install must migrate");

        assert!(hint.contains("releases/download/v20260712.2.0/install.sh"));
        assert!(hint.contains("| CS_VERSION=20260712.2.0 bash"));
        assert!(!hint.contains("releases/latest/download"));
    }

    #[test]
    fn checksum_matches_lowercase_expected() {
        assert!(checksum_matches(
            "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2",
            "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2"
        ));
    }

    #[test]
    fn checksum_matches_uppercase_expected() {
        assert!(checksum_matches(
            "D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2D2",
            "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2"
        ));
    }

    #[test]
    fn checksum_matches_rejects_mismatch() {
        assert!(!checksum_matches(
            "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2",
            "0000000000000000000000000000000000000000000000000000000000000000"
        ));
    }

    #[test]
    fn checksum_digest_extracts_gnu_two_column_format() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let text = format!("{digest}  codex-switch-global-pace-darwin-arm64.tar.gz\n");

        assert_eq!(extract_checksum_digest(&text), Some(digest));
        assert!(checksum_matches(
            extract_checksum_digest(&text).unwrap(),
            digest
        ));
    }

    #[test]
    fn checksum_digest_matches_uppercase_hash() {
        let lowercase = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let uppercase = lowercase.to_ascii_uppercase();
        let text = format!("{uppercase}  archive.tar.gz\n");

        assert!(checksum_matches(
            extract_checksum_digest(&text).unwrap(),
            lowercase
        ));
    }

    #[test]
    fn checksum_digest_rejects_wrong_hash() {
        let actual = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let wrong = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let text = format!("{wrong}  archive.tar.gz\n");

        assert!(!checksum_matches(
            extract_checksum_digest(&text).unwrap(),
            actual
        ));
    }

    #[test]
    fn checksum_digest_rejects_empty_or_whitespace_only_files() {
        assert_eq!(extract_checksum_digest(""), None);
        assert_eq!(extract_checksum_digest(" \t\n"), None);
    }
}
