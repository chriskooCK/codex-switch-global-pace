use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs4::FileExt;
#[cfg(windows)]
use rand::Rng as _;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(windows)]
mod cleanup_worker;

const REPO_OWNER: &str = "chriskooCK";
const REPO_NAME: &str = "codex-switch-global-pace";
const BIN_NAME: &str = "codex-switch-global-pace";
const PROVENANCE_ASSET_NAME: &str = "codex-switch-global-pace-build-provenance.json";
const RELEASE_WORKFLOW: &str = "chriskooCK/codex-switch-global-pace/.github/workflows/release.yml";
const SYSTEM_INSTALL_DIR: &str = "/usr/local/bin";
const SYSTEM_INSTALL_MARKER_NAME: &str = ".codex-switch-global-pace-system-install-v1";
const UPDATE_CACHE_NAME: &str = "global-pace-update-check.json";
const UPDATE_TTL_SECS: i64 = 12 * 60 * 60;
const UPDATE_LOCK_TARGET_ENV: &str = "CS_UPDATE_LOCK_TARGET";
const UPDATE_LOCK_READY_MARKER: &str = "codex-switch-global-pace update lock ready";
#[cfg(windows)]
const WINDOWS_RECOVERY_PATH_COLLISION_RETRY_LIMIT: usize = 16;
#[cfg(windows)]
const WINDOWS_DISPLACED_RECOVERY_PREFIX: &str = ".self-update-displaced-";
#[cfg(windows)]
const WINDOWS_FAILED_RECOVERY_PREFIX: &str = ".self-update-failed-";

#[cfg(windows)]
pub(crate) fn run_self_update_cleanup_worker(
    parent_pid: u32,
    displaced_previous: &Path,
    expected_token: &str,
    expected_executable_token: &str,
    journal_path: &Path,
    expected_journal_token: &str,
    ready_nonce: &str,
) -> Result<()> {
    cleanup_worker::run(
        parent_pid,
        displaced_previous,
        expected_token,
        expected_executable_token,
        journal_path,
        expected_journal_token,
        ready_nonce,
    )
}

#[cfg(not(windows))]
pub(crate) fn run_self_update_cleanup_worker(
    _parent_pid: u32,
    _displaced_previous: &Path,
    _expected_token: &str,
    _expected_executable_token: &str,
    _journal_path: &Path,
    _expected_journal_token: &str,
    _ready_nonce: &str,
) -> Result<()> {
    anyhow::bail!("the self-update cleanup worker is only available on Windows")
}

#[derive(Debug, thiserror::Error)]
#[error("previous executable cleanup remains pending: {source:#}")]
pub(crate) struct PendingSelfUpdateCleanup {
    #[source]
    source: anyhow::Error,
}

pub(crate) fn recover_pending_self_update_cleanup_on_startup()
-> std::result::Result<bool, PendingSelfUpdateCleanup> {
    #[cfg(windows)]
    {
        (|| -> Result<bool> {
            let executable = fs::canonicalize(
                std::env::current_exe()
                    .context("locating current executable for cleanup recovery")?,
            )
            .context("resolving current executable for cleanup recovery")?;
            cleanup_worker::recover_pending(&executable)
        })()
        .map_err(|source| PendingSelfUpdateCleanup { source })
    }
    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

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

fn canonical_executable_path(executable: PathBuf) -> Result<PathBuf> {
    fs::canonicalize(&executable)
        .with_context(|| format!("resolving executable path {}", executable.display()))
}

fn checked_regular_marker(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => anyhow::bail!(
            "system-install marker is not a regular file: {}",
            path.display()
        ),
        Err(err) => Err(err).with_context(|| format!("checking marker {}", path.display())),
    }
}

pub fn ensure_legacy_system_install_migrated(
    use_dev: bool,
    requested_version: Option<&str>,
) -> Result<()> {
    let executable =
        canonical_executable_path(std::env::current_exe().context("locating current executable")?)?;
    let platform = current_update_platform();
    let marker_present = if platform == UpdatePlatform::Unix
        && executable.parent() == Some(Path::new(SYSTEM_INSTALL_DIR))
    {
        checked_regular_marker(&Path::new(SYSTEM_INSTALL_DIR).join(SYSTEM_INSTALL_MARKER_NAME))?
    } else {
        false
    };

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
    replacement_state: SelfUpdateReplacementState,
    transaction_lease: Option<UpdateLease>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelfUpdateReplacementState {
    NotReplaced,
    Pending,
    Committed,
    RolledBack,
    Preserved,
}

impl SelfUpdateResult {
    pub(crate) fn replacement_state(&self) -> SelfUpdateReplacementState {
        self.replacement
            .as_ref()
            .map(PendingReplacement::public_state)
            .unwrap_or(self.replacement_state)
    }

    /// Finish a successful update after every dependent process has restarted.
    ///
    /// Commit revalidates the published executable token before consuming its
    /// exact recovery copies. On Windows, an exact cleanup journal is made
    /// durable before the independent backup is removed; a token-bound worker
    /// from the new public image then waits for this updater to exit before
    /// removing the mapped previous image and journal.
    pub(crate) fn commit_replacement(&mut self) -> Result<()> {
        if let Some(replacement) = self.replacement.as_mut() {
            let commit = replacement.commit();
            self.replacement_state = replacement.public_state();
            commit?;
        }
        self.replacement.take();
        self.transaction_lease.take();
        Ok(())
    }

    /// Restore the pre-update executable before restarting the old daemon.
    pub(crate) fn rollback_replacement(&mut self) -> Result<()> {
        if let Some(mut replacement) = self.replacement.take() {
            let rollback = replacement.rollback();
            self.replacement_state = replacement.public_state();
            if let Err(error) = rollback {
                let recovery = replacement.recovery_paths();
                replacement.preserve();
                self.transaction_lease.take();
                return Err(error.context(format!(
                    "exact executable rollback was incomplete; {}",
                    recovery.describe()
                )));
            }
        }
        self.transaction_lease.take();
        Ok(())
    }

    /// Keep the durable backup when process state cannot be proven safe enough
    /// for an automatic rollback, and return the exact manual-recovery paths.
    pub(crate) fn preserve_replacement_for_recovery(&mut self) -> Result<ReplacementRecoveryPaths> {
        let mut replacement = self
            .replacement
            .take()
            .context("updated result has no pending executable replacement")?;
        let recovery_paths = replacement.recovery_paths();
        replacement.preserve();
        self.replacement_state = replacement.public_state();
        self.transaction_lease.take();
        Ok(recovery_paths)
    }

    /// Convert an unsuccessful commit that still owns recovery material into
    /// an explicit preserved state and return the paths actually observed.
    /// A commit that reached `Committed` has already consumed those entries,
    /// so there is no recovery set to preserve.
    pub(crate) fn preserve_failed_commit_for_recovery(
        &mut self,
    ) -> Result<Option<ReplacementRecoveryPaths>> {
        match self.replacement_state() {
            SelfUpdateReplacementState::Pending | SelfUpdateReplacementState::Preserved => {
                self.preserve_replacement_for_recovery().map(Some)
            }
            SelfUpdateReplacementState::NotReplaced
            | SelfUpdateReplacementState::Committed
            | SelfUpdateReplacementState::RolledBack => Ok(None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplacementRecoveryPaths {
    pub(crate) executable: PathBuf,
    pub(crate) entries: Vec<ReplacementRecoveryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplacementRecoveryEntry {
    role: &'static str,
    path: PathBuf,
    state: ReplacementRecoveryEntryState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReplacementRecoveryEntryState {
    ExactPresent,
    Absent,
    DifferentIdentity,
    InspectionUnconfirmed(String),
}

impl ReplacementRecoveryPaths {
    pub(crate) fn describe(&self) -> String {
        let entries = self
            .entries
            .iter()
            .map(|entry| {
                let state = match &entry.state {
                    ReplacementRecoveryEntryState::ExactPresent => {
                        "exact entry is present".to_string()
                    }
                    ReplacementRecoveryEntryState::Absent => "entry is absent".to_string(),
                    ReplacementRecoveryEntryState::DifferentIdentity => {
                        "path has a different identity and is not claimed as recovery material"
                            .to_string()
                    }
                    ReplacementRecoveryEntryState::InspectionUnconfirmed(error) => {
                        format!("entry inspection was not confirmed: {error}")
                    }
                };
                format!("{} {} ({state})", entry.role, entry.path.display())
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "public executable {}; observed transaction entries: {entries}",
            self.executable.display()
        )
    }
}

fn observe_recovery_entry(
    role: &'static str,
    path: &Path,
    expected: &crate::fs_ops::FileToken,
) -> ReplacementRecoveryEntry {
    let state = match crate::fs_ops::token_if_present(path) {
        Ok(Some(observed)) if observed == *expected => ReplacementRecoveryEntryState::ExactPresent,
        Ok(Some(_)) => ReplacementRecoveryEntryState::DifferentIdentity,
        Ok(None) => ReplacementRecoveryEntryState::Absent,
        Err(error) => ReplacementRecoveryEntryState::InspectionUnconfirmed(format!("{error:#}")),
    };
    ReplacementRecoveryEntry {
        role,
        path: path.to_path_buf(),
        state,
    }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitFaultPoint {
    BeforeFinalRecoveryCleanup,
    AfterFinalRecoveryCleanup,
}

#[cfg(test)]
thread_local! {
    static COMMIT_FAULT: std::cell::Cell<Option<CommitFaultPoint>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
thread_local! {
    static PUBLICATION_ROLLBACK_PANIC: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

#[cfg(all(test, windows))]
thread_local! {
    static USE_EXTERNAL_WINDOWS_CLEANUP_WORKER: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

#[cfg(all(test, windows))]
const WINDOWS_COMMIT_EXIT_AFTER_BACKUP_ENV: &str = "CSGP_WINDOWS_COMMIT_EXIT_AFTER_BACKUP_CLEANUP";
#[cfg(all(test, windows))]
const WINDOWS_COMMIT_EXIT_AFTER_BACKUP_CODE: i32 = 86;

#[cfg(all(test, windows))]
fn use_external_windows_cleanup_worker_once() {
    USE_EXTERNAL_WINDOWS_CLEANUP_WORKER.with(|enabled| {
        assert!(!enabled.replace(true));
    });
}

#[cfg(all(test, windows))]
fn take_external_windows_cleanup_worker() -> bool {
    USE_EXTERNAL_WINDOWS_CLEANUP_WORKER.with(|enabled| enabled.replace(false))
}

#[cfg(windows)]
fn terminate_after_backup_cleanup_if_requested() {
    #[cfg(test)]
    if std::env::var_os(WINDOWS_COMMIT_EXIT_AFTER_BACKUP_ENV).is_some() {
        // This is deliberately a process boundary rather than a panic. It
        // proves the pre-mutation journal survives when no stack guard or
        // cleanup worker gets a chance to run after the fixed backup is gone.
        std::process::exit(WINDOWS_COMMIT_EXIT_AFTER_BACKUP_CODE);
    }
}

fn inject_commit_fault(point: CommitFaultPoint) {
    #[cfg(test)]
    if COMMIT_FAULT.with(|fault| {
        let matches = fault.get() == Some(point);
        if matches {
            fault.set(None);
        }
        matches
    }) {
        panic!("injected executable commit failure at {point:?}");
    }
    #[cfg(not(test))]
    let _ = point;
}

fn panic_payload_description(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

#[cfg(not(windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnixReplaceFaultPoint {
    CurrentExecutableChangedBeforePublish,
    ConcurrentExecutableClaimedPublicPath,
    AfterPublishBeforeClassification,
    #[cfg(test)]
    AfterPublishBeforeClassificationError,
}

#[cfg(all(not(windows), test))]
thread_local! {
    static UNIX_REPLACE_FAULT: std::cell::Cell<Option<UnixReplaceFaultPoint>> = const {
        std::cell::Cell::new(None)
    };
}

#[derive(Debug)]
struct PendingReplacement {
    lease: Option<UpdateLease>,
    state: ReplacementState,
    completion: Option<SelfUpdateReplacementState>,
    executable: PathBuf,
    backup: PathBuf,
    backup_token: crate::fs_ops::FileToken,
    previous_token: crate::fs_ops::FileToken,
    published_token: crate::fs_ops::FileToken,
    displaced_previous: PathBuf,
    #[cfg(windows)]
    failed_candidate: PathBuf,
}

impl PendingReplacement {
    fn public_state(&self) -> SelfUpdateReplacementState {
        match self.state {
            ReplacementState::Pending => SelfUpdateReplacementState::Pending,
            ReplacementState::Preserved => SelfUpdateReplacementState::Preserved,
            ReplacementState::Finished => self
                .completion
                .expect("a finished executable replacement has a completion state"),
        }
    }

    fn recovery_paths(&self) -> ReplacementRecoveryPaths {
        let mut entries = vec![observe_recovery_entry(
            "independent previous-executable backup",
            &self.backup,
            &self.backup_token,
        )];
        entries.push(observe_recovery_entry(
            "displaced previous executable",
            &self.displaced_previous,
            &self.previous_token,
        ));
        #[cfg(windows)]
        entries.push(observe_recovery_entry(
            "failed candidate executable",
            &self.failed_candidate,
            &self.published_token,
        ));
        ReplacementRecoveryPaths {
            executable: self.executable.clone(),
            entries,
        }
    }

    fn commit(&mut self) -> Result<()> {
        if self.state != ReplacementState::Pending {
            return Ok(());
        }
        let result = (|| -> Result<Vec<String>> {
            require_file_token(
                &self.executable,
                &self.published_token,
                "published self-update candidate at commit",
            )?;
            #[cfg(windows)]
            let (external_cleanup, prepared_cleanup) = {
                #[cfg(not(test))]
                let external_cleanup = true;
                #[cfg(test)]
                let external_cleanup = take_external_windows_cleanup_worker();
                let prepared_cleanup = if external_cleanup {
                    Some(
                        cleanup_worker::prepare(
                            &self.executable,
                            &self.published_token,
                            &self.backup,
                            &self.backup_token,
                            &self.displaced_previous,
                            &self.previous_token,
                        )
                        .context(
                            "durably recording exact executable cleanup before commit mutation",
                        )?,
                    )
                } else {
                    None
                };
                (external_cleanup, prepared_cleanup)
            };
            // From the first recovery deletion onward automatic rollback is
            // no longer sound. Publish that fact before any mutation so an
            // unwind can never interpret a reduced recovery set as Pending.
            self.completion = Some(SelfUpdateReplacementState::Committed);
            self.state = ReplacementState::Preserved;
            #[cfg(windows)]
            {
                // The fixed-name backup is not mapped. Remove it first so a
                // successful update never leaves a name that blocks the next
                // update while this old updater image is still shutting down.
                let backup_cleanup = remove_exact_transaction_path(
                    &self.backup,
                    &self.backup_token,
                    "old executable backup",
                )?;
                terminate_after_backup_cleanup_if_requested();
                inject_commit_fault(CommitFaultPoint::BeforeFinalRecoveryCleanup);
                let displaced_cleanup = if external_cleanup {
                    cleanup_worker::spawn(
                        &self.executable,
                        &self.published_token,
                        &self.displaced_previous,
                        &self.previous_token,
                        prepared_cleanup.context(
                            "self-update cleanup journal was not retained through commit mutation",
                        )?,
                    )
                    .context("starting exact previous-image cleanup after updater exit")?;
                    None
                } else {
                    // Unit fixtures are inert files in this process. Keep them
                    // synchronous; the copied-running-EXE regression opts into
                    // the production worker through the one-shot hook above.
                    Some(remove_exact_transaction_path(
                        &self.displaced_previous,
                        &self.previous_token,
                        "displaced previous executable",
                    )?)
                };
                // Readiness means the new public executable has opened this
                // updater's process object and verified both exact file tokens.
                // It removes the mapped old image only after updater exit.
                self.state = ReplacementState::Finished;
                inject_commit_fault(CommitFaultPoint::AfterFinalRecoveryCleanup);
                let unconfirmed = backup_cleanup
                    .unconfirmed_note()
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let mut unconfirmed = unconfirmed;
                if let Some(note) = displaced_cleanup
                    .as_ref()
                    .and_then(ExactRemovalOutcome::unconfirmed_note)
                {
                    unconfirmed.push(note.to_string());
                }
                Ok(unconfirmed)
            }
            #[cfg(not(windows))]
            {
                let displaced_cleanup = remove_exact_transaction_path(
                    &self.displaced_previous,
                    &self.previous_token,
                    "displaced previous executable",
                )?;
                inject_commit_fault(CommitFaultPoint::BeforeFinalRecoveryCleanup);
                let backup_cleanup = remove_exact_transaction_path(
                    &self.backup,
                    &self.backup_token,
                    "old executable backup",
                )?;
                // Both recovery names are gone. Set the live state immediately,
                // before formatting or allocating cleanup diagnostics.
                self.state = ReplacementState::Finished;
                inject_commit_fault(CommitFaultPoint::AfterFinalRecoveryCleanup);
                let mut unconfirmed = Vec::new();
                if let Some(note) = displaced_cleanup.unconfirmed_note() {
                    unconfirmed.push(note.to_string());
                }
                if let Some(note) = backup_cleanup.unconfirmed_note() {
                    unconfirmed.push(note.to_string());
                }
                Ok(unconfirmed)
            }
        })();
        let result = match result {
            Ok(unconfirmed) => {
                if unconfirmed.is_empty() {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "the executable replacement was committed and its recovery entries were removed, but cleanup durability remained unconfirmed: {}",
                        unconfirmed.join("; ")
                    ))
                }
            }
            Err(error) => Err(error),
        };
        self.lease.take();
        result
    }

    fn rollback(&mut self) -> Result<()> {
        if self.state != ReplacementState::Pending {
            return Ok(());
        }
        // A failed rollback must leave the backup untouched for manual recovery;
        // Drop must never reinterpret that state as permission to delete it.
        self.completion = Some(SelfUpdateReplacementState::RolledBack);
        self.state = ReplacementState::Preserved;
        #[cfg(windows)]
        let rollback = rollback_windows_replacement(
            &self.executable,
            &self.backup,
            &self.backup_token,
            &self.previous_token,
            &self.published_token,
            &self.displaced_previous,
            &self.failed_candidate,
        );
        #[cfg(not(windows))]
        let rollback = rollback_unix_replacement(
            &self.executable,
            &self.backup,
            &self.backup_token,
            &self.previous_token,
            &self.published_token,
            &self.displaced_previous,
        );
        rollback?;
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

enum PublicationFailureRecovery {
    Restored,
    Preserved {
        paths: ReplacementRecoveryPaths,
        error: String,
    },
}

struct PublicationRecoveryOwner {
    pending: Option<PendingReplacement>,
    failure_recovery: std::sync::Arc<std::sync::Mutex<Option<PublicationFailureRecovery>>>,
}

impl PublicationRecoveryOwner {
    fn new(
        pending: PendingReplacement,
        failure_recovery: std::sync::Arc<std::sync::Mutex<Option<PublicationFailureRecovery>>>,
    ) -> Self {
        Self {
            pending: Some(pending),
            failure_recovery,
        }
    }

    #[cfg(target_os = "windows")]
    fn pending_mut(&mut self) -> &mut PendingReplacement {
        self.pending
            .as_mut()
            .expect("publication recovery owner was already consumed")
    }

    fn into_pending(mut self) -> PendingReplacement {
        self.pending
            .take()
            .expect("publication recovery owner was already consumed")
    }

    fn preserve_classified_error(&mut self) {
        if let Some(pending) = self.pending.as_mut() {
            pending.preserve();
        }
    }
}

impl Drop for PublicationRecoveryOwner {
    fn drop(&mut self) {
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        if pending.state != ReplacementState::Pending {
            return;
        }
        let rollback = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(test)]
            if PUBLICATION_ROLLBACK_PANIC.with(|fault| fault.replace(false)) {
                panic!("injected unwind inside publication rollback");
            }
            pending.rollback()
        }));
        let recovery = match rollback {
            Ok(Ok(())) => PublicationFailureRecovery::Restored,
            Ok(Err(error)) => PublicationFailureRecovery::Preserved {
                paths: pending.recovery_paths(),
                error: format!("{error:#}"),
            },
            Err(payload) => {
                pending.preserve();
                PublicationFailureRecovery::Preserved {
                    paths: pending.recovery_paths(),
                    error: format!(
                        "rollback itself unwound: {}",
                        panic_payload_description(payload.as_ref())
                    ),
                }
            }
        };
        let mut report = self
            .failure_recovery
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *report = Some(recovery);
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
                replacement_state: SelfUpdateReplacementState::NotReplaced,
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
            replacement_state: SelfUpdateReplacementState::NotReplaced,
            transaction_lease: Some(update_lease),
        });
    }

    // Keep publication-to-owner transfer free of user code: once the await
    // returns its guarded PendingReplacement, only infallible moves construct
    // the result handed to the orchestration coordinator.
    Ok(SelfUpdateResult {
        current_version,
        latest_version,
        install_source,
        updated: true,
        replacement: Some(download_and_replace(&release, show_progress, "", update_lease).await?),
        replacement_state: SelfUpdateReplacementState::Pending,
        transaction_lease: None,
    })
}

pub(crate) fn record_successful_self_update(result: &SelfUpdateResult) {
    if result.updated && result.replacement_state == SelfUpdateReplacementState::Committed {
        save_update_cache(&UpdateCache {
            checked_at: crate::auth::now_unix_secs(),
            latest_version: result.latest_version.clone(),
        });
    }
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
            replacement_state: SelfUpdateReplacementState::NotReplaced,
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
        replacement_state: SelfUpdateReplacementState::Pending,
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

#[cfg(windows)]
fn random_windows_recovery_sibling_path(
    executable: &Path,
    prefix: &str,
    purpose: &str,
) -> Result<PathBuf> {
    windows_recovery_sibling_path_with_nonce_source(executable, prefix, purpose, || {
        let mut nonce = [0_u8; 16];
        rand::rng().fill_bytes(&mut nonce);
        nonce
    })
}

#[cfg(windows)]
fn windows_recovery_sibling_path_with_nonce_source<F>(
    executable: &Path,
    prefix: &str,
    purpose: &str,
    mut next_nonce: F,
) -> Result<PathBuf>
where
    F: FnMut() -> [u8; 16],
{
    for _ in 0..WINDOWS_RECOVERY_PATH_COLLISION_RETRY_LIMIT {
        let suffix = format!("{prefix}{}", hex::encode(next_nonce()));
        let candidate = transaction_sibling_path(executable, &suffix)?;
        match fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspecting randomized {purpose} path {}",
                        candidate.display()
                    )
                });
            }
        }
    }
    anyhow::bail!(
        "could not allocate a randomized {purpose} path after {WINDOWS_RECOVERY_PATH_COLLISION_RETRY_LIMIT} collisions"
    )
}

pub(crate) fn acquire_self_update_lease() -> Result<SelfUpdateLease> {
    let executable =
        fs::canonicalize(std::env::current_exe().context("locating current executable")?)
            .context("resolving current executable")?;
    Ok(SelfUpdateLease(acquire_update_lease(&executable)?))
}

fn normalize_update_lock_target(destination: &Path) -> Result<PathBuf> {
    match fs::metadata(destination) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                anyhow::bail!(
                    "update destination is not a regular file: {}",
                    destination.display()
                );
            }
            fs::canonicalize(destination)
                .with_context(|| format!("resolving update destination {}", destination.display()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = destination.parent().with_context(|| {
                format!(
                    "update destination has no parent directory: {}",
                    destination.display()
                )
            })?;
            let file_name = destination.file_name().with_context(|| {
                format!(
                    "update destination has no file name: {}",
                    destination.display()
                )
            })?;
            let canonical_parent = fs::canonicalize(parent).with_context(|| {
                format!("resolving update destination parent {}", parent.display())
            })?;
            let normalized = canonical_parent.join(file_name);

            // Close the create-between-checks race without inventing a second
            // lock location: if the destination appeared, resolve it exactly as
            // the existing-file branch does.
            match fs::metadata(&normalized) {
                Ok(metadata) => {
                    if !metadata.file_type().is_file() {
                        anyhow::bail!(
                            "update destination is not a regular file: {}",
                            normalized.display()
                        );
                    }
                    fs::canonicalize(&normalized).with_context(|| {
                        format!("resolving update destination {}", normalized.display())
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(normalized),
                Err(error) => Err(error).with_context(|| {
                    format!("inspecting update destination {}", normalized.display())
                }),
            }
        }
        Err(error) => Err(error)
            .with_context(|| format!("inspecting update destination {}", destination.display())),
    }
}

pub(crate) fn hold_update_lock_from_env() -> Result<()> {
    let destination = std::env::var_os(UPDATE_LOCK_TARGET_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .with_context(|| format!("{UPDATE_LOCK_TARGET_ENV} is required"))?;
    let destination = normalize_update_lock_target(&destination)?;
    let _lease = acquire_update_lease(&destination)?;

    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{UPDATE_LOCK_READY_MARKER}").context("writing update-lock ready marker")?;
    stdout
        .flush()
        .context("flushing update-lock ready marker")?;

    // The parent owns the lease lifetime through this pipe. EOF is the only
    // release signal; there is no timeout or alternate lock path.
    let mut stdin = io::stdin().lock();
    let mut buffer = [0_u8; 1024];
    while stdin
        .read(&mut buffer)
        .context("waiting for update-lock release")?
        != 0
    {}
    Ok(())
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

fn replace_candidate(
    executable: &Path,
    candidate: &Path,
    update_lease: UpdateLease,
    platform: UpdatePlatform,
    release_tag: &str,
) -> Result<PendingReplacement> {
    let failure_recovery = std::sync::Arc::new(std::sync::Mutex::new(None));
    let replacement = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        replace_candidate_inner(
            executable,
            candidate,
            update_lease,
            platform,
            release_tag,
            failure_recovery.clone(),
        )
    }));
    match replacement {
        Ok(Ok(pending)) => Ok(pending),
        Ok(Err(error)) => {
            let recovery = failure_recovery
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            match recovery {
                Some(PublicationFailureRecovery::Restored) => Err(error.context(
                    "post-publication processing failed; the exact previous executable was restored before publication authority was released",
                )),
                Some(PublicationFailureRecovery::Preserved { paths, error: rollback }) => {
                    Err(error.context(format!(
                        "post-publication processing failed and exact rollback was incomplete ({rollback}); {}",
                        paths.describe()
                    )))
                }
                None => Err(error),
            }
        }
        Err(payload) => {
            let panic = panic_payload_description(payload.as_ref());
            let recovery = failure_recovery
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            match recovery {
                Some(PublicationFailureRecovery::Restored) => Err(anyhow::anyhow!(
                    "self-update publication unwound ({panic}); the exact previous executable was restored before publication authority was released"
                )),
                Some(PublicationFailureRecovery::Preserved { paths, error }) => {
                    Err(anyhow::anyhow!(
                        "self-update publication unwound ({panic}) and exact rollback failed ({error}); {}",
                        paths.describe()
                    ))
                }
                None => Err(anyhow::anyhow!(
                    "self-update preparation unwound before executable publication ({panic}); no publication recovery owner was armed"
                )),
            }
        }
    }
}

#[cfg(not(windows))]
fn replace_candidate_inner(
    executable: &Path,
    candidate: &Path,
    update_lease: UpdateLease,
    platform: UpdatePlatform,
    release_tag: &str,
    failure_recovery: std::sync::Arc<std::sync::Mutex<Option<PublicationFailureRecovery>>>,
) -> Result<PendingReplacement> {
    let backup = transaction_sibling_path(executable, ".self-update-backup")?;
    let staged = transaction_sibling_path(executable, ".self-update-candidate")?;
    require_transaction_path_absent(&backup, "self-update backup")?;
    require_transaction_path_absent(&staged, "staged self-update candidate")?;

    let previous_token = crate::fs_ops::token_for_path(executable)
        .context("binding the current executable before self-update")?;
    let backup_token = create_exclusive_copy_durable(
        executable,
        &backup,
        &previous_token,
        "independent previous-executable backup",
    )
    .with_context(|| {
        format!(
            "preserving an independent copy of the current executable at {}",
            backup.display()
        )
    })?;

    let candidate_source_token = crate::fs_ops::token_for_path(candidate)
        .context("binding the verified self-update candidate")?;
    let published_token = match create_exclusive_copy_durable(
        candidate,
        &staged,
        &candidate_source_token,
        "staged self-update candidate",
    ) {
        Ok(token) => token,
        Err(error) => {
            let backup_cleanup =
                remove_exact_transaction_path(&backup, &backup_token, "unused self-update backup");
            return Err(error.context(format!(
                "staging the self-update candidate failed; backup cleanup was {}",
                cleanup_result(backup_cleanup)
            )));
        }
    };

    inject_unix_replace_fault(
        executable,
        UnixReplaceFaultPoint::CurrentExecutableChangedBeforePublish,
    )?;
    if let Err(error) = require_file_token(
        executable,
        &previous_token,
        "current executable immediately before publication",
    ) {
        let staged_cleanup = remove_exact_transaction_path(
            &staged,
            &published_token,
            "unused staged self-update candidate",
        );
        let backup_cleanup =
            remove_exact_transaction_path(&backup, &backup_token, "unused self-update backup");
        return Err(error.context(format!(
            "self-update stopped before publication; candidate cleanup was {} and backup cleanup was {}",
            cleanup_result(staged_cleanup),
            cleanup_result(backup_cleanup)
        )));
    }
    require_file_token(
        &backup,
        &backup_token,
        "independent previous-executable backup",
    )?;

    // This second test hook models a non-cooperating writer that wins after
    // the last observation but before the only namespace publication call.
    inject_unix_replace_fault(
        executable,
        UnixReplaceFaultPoint::ConcurrentExecutableClaimedPublicPath,
    )?;

    // Construct the recovery owner before the only publication syscall. If
    // classification unwinds after exchange, its Drop retains the independent
    // backup and exact displaced path instead of losing recovery ownership.
    let mut publication_owner = PublicationRecoveryOwner::new(
        PendingReplacement {
            lease: Some(update_lease),
            state: ReplacementState::Pending,
            completion: None,
            executable: executable.to_path_buf(),
            backup: backup.clone(),
            backup_token: backup_token.clone(),
            previous_token: previous_token.clone(),
            published_token: published_token.clone(),
            displaced_previous: staged.clone(),
        },
        failure_recovery,
    );
    let exchange_result = crate::fs_ops::exchange(&staged, executable);
    inject_unix_replace_fault(
        executable,
        UnixReplaceFaultPoint::AfterPublishBeforeClassification,
    )?;
    #[cfg(test)]
    inject_unix_replace_fault(
        executable,
        UnixReplaceFaultPoint::AfterPublishBeforeClassificationError,
    )?;
    let public_token = crate::fs_ops::token_if_present(executable).with_context(|| {
        format!(
            "classifying published executable {}; exact recovery operands remain at {} and {}",
            executable.display(),
            backup.display(),
            staged.display()
        )
    })?;
    let displaced_token = crate::fs_ops::token_if_present(&staged).with_context(|| {
        format!(
            "classifying displaced executable {}; exact backup remains at {}",
            staged.display(),
            backup.display()
        )
    })?;

    if public_token.as_ref() == Some(&published_token)
        && displaced_token.as_ref() == Some(&previous_token)
    {
        if let Err(error) = confirm_unix_namespace_durability(
            exchange_result,
            executable,
            "self-update candidate publication",
        ) {
            let rollback = rollback_uncommitted_unix_exchange(
                executable,
                &staged,
                &backup,
                &backup_token,
                &previous_token,
                &published_token,
            );
            publication_owner.preserve_classified_error();
            return Err(error.context(format!(
                "candidate and previous executable were exchanged, but publication durability was not confirmed; rollback was {}",
                operation_result(rollback)
            )));
        }
        return Ok(publication_owner.into_pending());
    }

    let state = format!(
        "public={}, displaced={}",
        describe_token_state(public_token.as_ref(), &published_token, &previous_token),
        describe_token_state(displaced_token.as_ref(), &published_token, &previous_token)
    );
    if public_token.as_ref() == Some(&published_token) {
        let restoration = restore_displaced_unix_writer(
            executable,
            &staged,
            displaced_token.as_ref(),
            &published_token,
        );
        if restoration.is_ok() {
            let staged_cleanup = remove_exact_transaction_path(
                &staged,
                &published_token,
                "refused self-update candidate",
            );
            let backup_cleanup =
                remove_exact_transaction_path(&backup, &backup_token, "unused self-update backup");
            publication_owner.preserve_classified_error();
            return Err(anyhow::anyhow!(
                "self-update exchanged a file that no longer matched the authorized executable; the displaced writer was restored. Candidate cleanup was {} and backup cleanup was {}",
                cleanup_result(staged_cleanup),
                cleanup_result(backup_cleanup)
            ));
        }
        publication_owner.preserve_classified_error();
        return Err(anyhow::anyhow!(
            "self-update publication was not authorized ({state}); no file was deleted. Candidate/public path {}, displaced path {}, previous backup {}. Restoration failed: {}",
            executable.display(),
            staged.display(),
            backup.display(),
            operation_result(restoration)
        ));
    }

    if public_token.as_ref() == Some(&previous_token)
        && displaced_token.as_ref() == Some(&published_token)
    {
        let staged_cleanup = remove_exact_transaction_path(
            &staged,
            &published_token,
            "unpublished self-update candidate",
        );
        let backup_cleanup =
            remove_exact_transaction_path(&backup, &backup_token, "unused self-update backup");
        let exchange_error = exchange_result
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_else(|| "exchange result did not match its postcondition".to_string());
        publication_owner.preserve_classified_error();
        return Err(anyhow::anyhow!(
            "self-update candidate was not published: {exchange_error}. Candidate cleanup was {} and backup cleanup was {}",
            cleanup_result(staged_cleanup),
            cleanup_result(backup_cleanup)
        ));
    }

    publication_owner.preserve_classified_error();
    Err(anyhow::anyhow!(
        "self-update publication ended in an unclassified external-writer state ({state}); no file was deleted. Public path {}, displaced path {}, previous backup {}. {}",
        executable.display(),
        staged.display(),
        backup.display(),
        replacement_permission_hint(executable, platform, release_tag)
    ))
}

#[cfg(not(windows))]
fn inject_unix_replace_fault(executable: &Path, point: UnixReplaceFaultPoint) -> Result<()> {
    #[cfg(test)]
    if UNIX_REPLACE_FAULT.with(|fault| {
        let matches = fault.get() == Some(point);
        if matches {
            fault.set(None);
        }
        matches
    }) {
        #[cfg(test)]
        match point {
            UnixReplaceFaultPoint::AfterPublishBeforeClassification => {
                panic!("injected unwind after Unix executable publication");
            }
            UnixReplaceFaultPoint::AfterPublishBeforeClassificationError => {
                anyhow::bail!("injected error after Unix executable publication");
            }
            _ => {}
        }
        let external = transaction_sibling_path(executable, ".injected-external")?;
        require_transaction_path_absent(&external, "injected external executable")?;
        fs::write(&external, b"external executable")?;
        fs::rename(&external, executable)?;
    }
    let _ = (executable, point);
    Ok(())
}

#[cfg(not(windows))]
fn restore_displaced_unix_writer(
    executable: &Path,
    displaced: &Path,
    displaced_token: Option<&crate::fs_ops::FileToken>,
    published_token: &crate::fs_ops::FileToken,
) -> Result<()> {
    let displaced_token =
        displaced_token.context("the exchanged executable has no displaced recovery entry")?;
    require_file_token(
        executable,
        published_token,
        "candidate before restoring the displaced writer",
    )?;
    require_file_token(
        displaced,
        displaced_token,
        "displaced writer before restoration",
    )?;

    let exchange_result = crate::fs_ops::exchange(displaced, executable);
    let public_after = crate::fs_ops::token_if_present(executable)?;
    let displaced_after = crate::fs_ops::token_if_present(displaced)?;
    if public_after.as_ref() != Some(displaced_token)
        || displaced_after.as_ref() != Some(published_token)
    {
        anyhow::bail!(
            "the displaced writer could not be restored without changing another writer; public and displaced entries were preserved"
        );
    }
    confirm_unix_namespace_durability(
        exchange_result,
        executable,
        "restoring the displaced executable writer",
    )
}

#[cfg(not(windows))]
fn rollback_uncommitted_unix_exchange(
    executable: &Path,
    displaced: &Path,
    backup: &Path,
    backup_token: &crate::fs_ops::FileToken,
    previous_token: &crate::fs_ops::FileToken,
    published_token: &crate::fs_ops::FileToken,
) -> Result<()> {
    require_file_token(
        backup,
        backup_token,
        "independent previous-executable backup before publication rollback",
    )?;
    require_file_token(
        executable,
        published_token,
        "candidate before publication rollback",
    )?;
    require_file_token(
        displaced,
        previous_token,
        "displaced previous executable before publication rollback",
    )?;
    let exchange_result = crate::fs_ops::exchange(displaced, executable);
    let public_after = crate::fs_ops::token_if_present(executable)?;
    let displaced_after = crate::fs_ops::token_if_present(displaced)?;
    if public_after.as_ref() != Some(previous_token)
        || displaced_after.as_ref() != Some(published_token)
    {
        anyhow::bail!(
            "publication rollback ended in an unclassified state; public {}, candidate {}, backup {} were preserved",
            executable.display(),
            displaced.display(),
            backup.display()
        );
    }
    confirm_unix_namespace_durability(
        exchange_result,
        executable,
        "restoring the previous executable during rollback",
    )?;
    remove_exact_transaction_path(
        displaced,
        published_token,
        "rolled-back self-update candidate",
    )?
    .require_durable()?;
    remove_exact_transaction_path(backup, backup_token, "redundant self-update backup")?
        .require_durable()
}

#[cfg(not(windows))]
fn rollback_unix_replacement(
    executable: &Path,
    backup: &Path,
    backup_token: &crate::fs_ops::FileToken,
    previous_token: &crate::fs_ops::FileToken,
    published_token: &crate::fs_ops::FileToken,
    displaced_previous: &Path,
) -> Result<()> {
    rollback_uncommitted_unix_exchange(
        executable,
        displaced_previous,
        backup,
        backup_token,
        previous_token,
        published_token,
    )
}

#[derive(Debug)]
enum ExactRemovalOutcome {
    Durable,
    DurabilityUnconfirmed(String),
}

impl ExactRemovalOutcome {
    fn unconfirmed_note(&self) -> Option<&str> {
        match self {
            Self::Durable => None,
            Self::DurabilityUnconfirmed(note) => {
                #[cfg(windows)]
                {
                    let _ = note;
                    None
                }
                #[cfg(not(windows))]
                {
                    Some(note)
                }
            }
        }
    }

    fn require_durable(self) -> Result<()> {
        match self {
            Self::Durable => Ok(()),
            Self::DurabilityUnconfirmed(note) => {
                #[cfg(windows)]
                {
                    let _ = note;
                    Ok(())
                }
                #[cfg(not(windows))]
                {
                    Err(anyhow::anyhow!(note))
                }
            }
        }
    }
}

fn cleanup_result(result: Result<ExactRemovalOutcome>) -> String {
    match result {
        Ok(ExactRemovalOutcome::Durable) => "complete and durable".to_string(),
        Ok(ExactRemovalOutcome::DurabilityUnconfirmed(note)) => {
            format!("applied, but durability is unconfirmed: {note}")
        }
        Err(error) => format!("incomplete: {error:#}"),
    }
}

fn operation_result(result: Result<()>) -> String {
    match result {
        Ok(()) => "complete".to_string(),
        Err(error) => format!("incomplete: {error:#}"),
    }
}

#[cfg(not(windows))]
fn confirm_unix_namespace_durability(
    boundary: Result<()>,
    path: &Path,
    operation: &str,
) -> Result<()> {
    match crate::fs_ops::sync_parent(path) {
        Ok(()) => Ok(()),
        Err(sync_error) => match boundary {
            Ok(()) => Err(sync_error).with_context(|| {
                format!(
                    "{operation} reached its verified namespace state, but directory durability was not confirmed"
                )
            }),
            Err(boundary_error) => Err(boundary_error).context(format!(
                "{operation} reached its verified namespace state after the namespace call reported an error, and retrying directory durability also failed: {sync_error:#}"
            )),
        },
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsReplaceFaultPoint {
    BeforePublish,
    AfterPublish,
    AfterPublishBeforeClassification,
    #[cfg(test)]
    AfterPublishBeforeClassificationError,
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
        if point == WindowsReplaceFaultPoint::AfterPublishBeforeClassification {
            panic!("injected unwind after Windows executable publication");
        }
        anyhow::bail!("injected Windows replacement failure at {point:?}");
    }
    let _ = point;
    Ok(())
}

#[cfg(windows)]
fn inject_windows_replace_api_fault(replaced: &Path, displaced: &Path) -> Option<Result<()>> {
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
            fs::rename(replaced, displaced).expect("inject 1177 original-to-displaced move");
            if fault == WindowsReplaceFaultPoint::OriginalMovedToBackupAndBlockRestore {
                fs::create_dir(replaced).expect("inject a blocker at the executable path");
            }
            return Some(Err(anyhow::Error::new(io::Error::from_raw_os_error(
                windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 as i32,
            ))
            .context("injected ReplaceFileW partial failure (1177)")));
        }
    }
    let _ = (replaced, displaced);
    None
}

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

fn require_file_token(
    path: &Path,
    expected: &crate::fs_ops::FileToken,
    purpose: &str,
) -> Result<()> {
    let observed = crate::fs_ops::token_for_path(path)
        .with_context(|| format!("binding {purpose} at {}", path.display()))?;
    if &observed != expected {
        anyhow::bail!(
            "{purpose} changed at {}; expected token {}, observed {}",
            path.display(),
            expected,
            observed
        );
    }
    Ok(())
}

fn describe_token_state(
    observed: Option<&crate::fs_ops::FileToken>,
    published: &crate::fs_ops::FileToken,
    previous: &crate::fs_ops::FileToken,
) -> &'static str {
    match observed {
        None => "absent",
        Some(token) if token == published => "published candidate",
        Some(token) if token == previous => "previous executable",
        Some(_) => "foreign file",
    }
}

fn remove_exact_transaction_path(
    path: &Path,
    expected: &crate::fs_ops::FileToken,
    purpose: &str,
) -> Result<ExactRemovalOutcome> {
    let outcome = crate::fs_ops::remove_exact(path, expected)
        .with_context(|| format!("removing {purpose} at {}", path.display()))?;
    match outcome {
        crate::fs_ops::RemoveExactOutcome::Removed => Ok(ExactRemovalOutcome::Durable),
        crate::fs_ops::RemoveExactOutcome::RemovedNamespaceDurabilityUnconfirmed => {
            #[cfg(windows)]
            {
                Ok(ExactRemovalOutcome::DurabilityUnconfirmed(format!(
                    "the exact {purpose} was removed from {} and the resulting names were verified; Windows exposes no supported directory-fsync boundary for this operation",
                    path.display()
                )))
            }
            #[cfg(not(windows))]
            {
                match crate::fs_ops::sync_parent(path) {
                    Ok(()) => Ok(ExactRemovalOutcome::Durable),
                    Err(error) => Ok(ExactRemovalOutcome::DurabilityUnconfirmed(format!(
                        "the exact {purpose} was removed from {}, but retrying parent-directory durability failed: {error:#}",
                        path.display()
                    ))),
                }
            }
        }
    }
}

fn create_exclusive_copy_durable(
    source: &Path,
    destination: &Path,
    expected: &crate::fs_ops::FileToken,
    purpose: &str,
) -> Result<crate::fs_ops::FileToken> {
    let outcome = crate::fs_ops::create_exclusive_copy(source, destination, expected)
        .with_context(|| format!("creating {purpose} at {}", destination.display()))?;
    match outcome {
        crate::fs_ops::CreateExactOutcome::Created(token) => Ok(token),
        crate::fs_ops::CreateExactOutcome::CreatedNamespaceDurabilityUnconfirmed(token) => {
            #[cfg(windows)]
            {
                // Windows has already flushed and rebound the exact file handle.
                // It has no supported directory-fsync contract to retry here.
                Ok(token)
            }
            #[cfg(not(windows))]
            {
                match crate::fs_ops::sync_parent(destination) {
                    Ok(()) => Ok(token),
                    Err(sync_error) => {
                        let cleanup = remove_exact_transaction_path(destination, &token, purpose);
                        anyhow::bail!(
                            "{purpose} was created at {}, but retrying parent-directory durability failed: {sync_error:#}. Exact cleanup was {}",
                            destination.display(),
                            cleanup_result(cleanup)
                        )
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
fn replace_file_windows(replaced: &Path, replacement: &Path, displaced: &Path) -> Result<()> {
    if let Some(result) = inject_windows_replace_api_fault(replaced, displaced) {
        return result;
    }
    crate::fs_ops::replace_with_displaced(replacement, replaced, displaced).with_context(|| {
        format!(
            "atomically replacing {} with {} while preserving the displaced entry at {}",
            replaced.display(),
            replacement.display(),
            displaced.display()
        )
    })
}

#[cfg(windows)]
fn restore_windows_displaced_to_empty_public(
    executable: &Path,
    displaced: &Path,
    displaced_token: &crate::fs_ops::FileToken,
) -> Result<()> {
    if crate::fs_ops::token_if_present(executable)?.is_some() {
        anyhow::bail!(
            "public executable path was claimed before displaced-file recovery: {}",
            executable.display()
        );
    }
    require_file_token(
        displaced,
        displaced_token,
        "displaced executable before no-replace recovery",
    )?;
    let restore_result = crate::fs_ops::rename_noreplace(displaced, executable);
    let public_after = crate::fs_ops::token_if_present(executable)?;
    let displaced_after = crate::fs_ops::token_if_present(displaced)?;
    if public_after.as_ref() != Some(displaced_token) || displaced_after.is_some() {
        anyhow::bail!(
            "displaced executable recovery ended in an unclassified state; public {} and displaced {} were preserved",
            executable.display(),
            displaced.display()
        );
    }
    restore_result.context(
        "the displaced Windows executable reached the public path, but MoveFileExW did not confirm its write-through boundary",
    )?;
    require_file_token(
        executable,
        displaced_token,
        "restored executable after partial Windows replacement",
    )
}

#[cfg(windows)]
fn replace_windows_public_with_displaced(
    executable: &Path,
    displaced: &Path,
    desired_token: &crate::fs_ops::FileToken,
    current_token: &crate::fs_ops::FileToken,
    failed_current: &Path,
) -> Result<()> {
    require_transaction_path_absent(failed_current, "failed Windows replacement candidate")?;
    require_file_token(
        executable,
        current_token,
        "current public executable before restoration",
    )?;
    require_file_token(
        displaced,
        desired_token,
        "displaced executable before restoration",
    )?;

    let replace_result = replace_file_windows(executable, displaced, failed_current);
    let public_after = crate::fs_ops::token_if_present(executable)?;
    let desired_after = crate::fs_ops::token_if_present(displaced)?;
    let failed_after = crate::fs_ops::token_if_present(failed_current)?;

    if public_after.as_ref() == Some(desired_token)
        && desired_after.is_none()
        && failed_after.as_ref() == Some(current_token)
    {
        if let Err(error) = replace_result {
            return Err(error.context(
                "the desired executable reached the public path, but ReplaceFileW did not confirm restoration",
            ));
        }
        return remove_exact_transaction_path(
            failed_current,
            current_token,
            "displaced failed Windows candidate",
        )?
        .require_durable();
    }

    // ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 leaves the public name empty after
    // moving the current file to the backup name. The post-state, not merely
    // the numeric error, decides whether no-replace recovery is authorized.
    if public_after.is_none()
        && desired_after.as_ref() == Some(desired_token)
        && failed_after.as_ref() == Some(current_token)
    {
        restore_windows_displaced_to_empty_public(executable, displaced, desired_token)?;
        remove_exact_transaction_path(
            failed_current,
            current_token,
            "displaced failed Windows candidate",
        )?
        .require_durable()?;
        return Ok(());
    }

    let boundary_error = replace_result
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_else(|| {
            "ReplaceFileW returned success with an unexpected post-state".to_string()
        });
    anyhow::bail!(
        "Windows restoration ended in an unclassified state: public={}, desired={}, failed-current={}; no recovery entry was deleted ({boundary_error})",
        describe_token_state(public_after.as_ref(), current_token, desired_token),
        describe_token_state(desired_after.as_ref(), current_token, desired_token),
        describe_token_state(failed_after.as_ref(), current_token, desired_token)
    )
}

#[cfg(windows)]
fn replace_candidate_inner(
    executable: &Path,
    candidate: &Path,
    update_lease: UpdateLease,
    platform: UpdatePlatform,
    release_tag: &str,
    failure_recovery: std::sync::Arc<std::sync::Mutex<Option<PublicationFailureRecovery>>>,
) -> Result<PendingReplacement> {
    let backup = transaction_sibling_path(executable, ".self-update-backup")?;
    let staged = transaction_sibling_path(executable, ".self-update-candidate")?;
    let displaced_previous = random_windows_recovery_sibling_path(
        executable,
        WINDOWS_DISPLACED_RECOVERY_PREFIX,
        "displaced previous executable",
    )?;
    let failed_candidate = random_windows_recovery_sibling_path(
        executable,
        WINDOWS_FAILED_RECOVERY_PREFIX,
        "failed self-update candidate",
    )?;
    require_transaction_path_absent(&backup, "self-update backup")?;
    require_transaction_path_absent(&staged, "staged self-update candidate")?;

    let previous_token = crate::fs_ops::token_for_path(executable)
        .context("binding the current Windows executable before self-update")?;
    let backup_token = create_exclusive_copy_durable(
        executable,
        &backup,
        &previous_token,
        "independent Windows previous-executable backup",
    )
    .with_context(|| {
        format!(
            "preserving an independent copy of the current executable at {}",
            backup.display()
        )
    })?;
    let candidate_source_token = crate::fs_ops::token_for_path(candidate)
        .context("binding the verified Windows self-update candidate")?;
    let published_token = match create_exclusive_copy_durable(
        candidate,
        &staged,
        &candidate_source_token,
        "staged Windows self-update candidate",
    ) {
        Ok(token) => token,
        Err(error) => {
            let backup_cleanup = remove_exact_transaction_path(
                &backup,
                &backup_token,
                "unused Windows self-update backup",
            );
            return Err(error.context(format!(
                "staging the Windows self-update candidate failed; backup cleanup was {}",
                cleanup_result(backup_cleanup)
            )));
        }
    };

    let prepublication = (|| {
        require_file_token(
            executable,
            &previous_token,
            "current Windows executable immediately before publication",
        )?;
        require_file_token(
            &backup,
            &backup_token,
            "independent Windows previous-executable backup",
        )?;
        require_file_token(
            &staged,
            &published_token,
            "staged Windows self-update candidate",
        )?;
        inject_windows_replace_fault(WindowsReplaceFaultPoint::BeforePublish)
    })();
    if let Err(error) = prepublication {
        let staged_cleanup = remove_exact_transaction_path(
            &staged,
            &published_token,
            "unpublished Windows self-update candidate",
        );
        let backup_cleanup = remove_exact_transaction_path(
            &backup,
            &backup_token,
            "unused Windows self-update backup",
        );
        return Err(error.context(format!(
            "Windows self-update stopped before publication; candidate cleanup was {} and backup cleanup was {}",
            cleanup_result(staged_cleanup),
            cleanup_result(backup_cleanup)
        )));
    }

    // Own every exact recovery operand before ReplaceFileW. This guard is the
    // publication-to-result handoff: an unwind during post-state probes keeps
    // the backup and randomized displaced identities available for recovery.
    let mut publication_owner = PublicationRecoveryOwner::new(
        PendingReplacement {
            lease: Some(update_lease),
            state: ReplacementState::Pending,
            completion: None,
            executable: executable.to_path_buf(),
            backup: backup.clone(),
            backup_token: backup_token.clone(),
            previous_token: previous_token.clone(),
            published_token: published_token.clone(),
            displaced_previous: displaced_previous.clone(),
            failed_candidate: failed_candidate.clone(),
        },
        failure_recovery,
    );
    let recovery_context = || {
        format!(
            "classifying Windows replacement recovery paths: public {}, staged {}, displaced {}, independent backup {}",
            executable.display(),
            staged.display(),
            displaced_previous.display(),
            backup.display()
        )
    };
    let replace_result = replace_file_windows(executable, &staged, &displaced_previous);
    inject_windows_replace_fault(WindowsReplaceFaultPoint::AfterPublishBeforeClassification)?;
    #[cfg(test)]
    inject_windows_replace_fault(WindowsReplaceFaultPoint::AfterPublishBeforeClassificationError)?;
    let public_after =
        crate::fs_ops::token_if_present(executable).with_context(recovery_context)?;
    let staged_after = crate::fs_ops::token_if_present(&staged).with_context(recovery_context)?;
    let displaced_after =
        crate::fs_ops::token_if_present(&displaced_previous).with_context(recovery_context)?;
    let replace_detail = replace_result
        .as_ref()
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_else(|| "ReplaceFileW reported success".to_string());

    if public_after.as_ref() == Some(&published_token)
        && staged_after.is_none()
        && displaced_after.as_ref() == Some(&previous_token)
    {
        if let Err(error) = replace_result {
            let restoration = replace_windows_public_with_displaced(
                executable,
                &displaced_previous,
                &previous_token,
                &published_token,
                &failed_candidate,
            );
            let backup_cleanup = if restoration.is_ok() {
                remove_exact_transaction_path(
                    &backup,
                    &backup_token,
                    "unused Windows self-update backup",
                )
            } else {
                Ok(ExactRemovalOutcome::Durable)
            };
            publication_owner.preserve_classified_error();
            anyhow::bail!(
                "Windows replacement reached the published layout but the operating system did not confirm it ({error:#}; namespace result: {replace_detail}); restoration was {} and backup cleanup was {}",
                operation_result(restoration),
                cleanup_result(backup_cleanup)
            );
        }
        if let Err(error) = inject_windows_replace_fault(WindowsReplaceFaultPoint::AfterPublish) {
            if let Err(rollback_error) = publication_owner.pending_mut().rollback() {
                return Err(error.context(format!(
                    "Windows replacement failed after publication and exact rollback also failed: {rollback_error:#}"
                )));
            }
            return Err(error.context(
                "Windows replacement failed after publication; the exact previous executable was restored",
            ));
        }
        return Ok(publication_owner.into_pending());
    }

    if public_after.as_ref() == Some(&previous_token)
        && staged_after.as_ref() == Some(&published_token)
        && displaced_after.is_none()
    {
        let staged_cleanup = remove_exact_transaction_path(
            &staged,
            &published_token,
            "unpublished Windows self-update candidate",
        );
        let backup_cleanup = remove_exact_transaction_path(
            &backup,
            &backup_token,
            "unused Windows self-update backup",
        );
        publication_owner.preserve_classified_error();
        anyhow::bail!(
            "Windows self-update candidate was not published ({replace_detail}); candidate cleanup was {} and backup cleanup was {}. {}",
            cleanup_result(staged_cleanup),
            cleanup_result(backup_cleanup),
            replacement_permission_hint(executable, platform, release_tag)
        );
    }

    if public_after.is_none()
        && staged_after.as_ref() == Some(&published_token)
        && let Some(displaced_token) = displaced_after.as_ref()
    {
        let restoration = restore_windows_displaced_to_empty_public(
            executable,
            &displaced_previous,
            displaced_token,
        );
        if restoration.is_ok() {
            let staged_cleanup = remove_exact_transaction_path(
                &staged,
                &published_token,
                "unpublished Windows self-update candidate",
            );
            let backup_cleanup = remove_exact_transaction_path(
                &backup,
                &backup_token,
                "unused Windows self-update backup",
            );
            publication_owner.preserve_classified_error();
            anyhow::bail!(
                "Windows replacement stopped after displacing the public executable ({replace_detail}); the actual displaced file was restored without replacement. Candidate cleanup was {} and backup cleanup was {}",
                cleanup_result(staged_cleanup),
                cleanup_result(backup_cleanup)
            );
        }
        publication_owner.preserve_classified_error();
        anyhow::bail!(
            "Windows replacement stopped with an empty public path ({replace_detail}); no recovery file was deleted. Public {}, staged {}, displaced {}, independent backup {}. Restoration failed: {}",
            executable.display(),
            staged.display(),
            displaced_previous.display(),
            backup.display(),
            operation_result(restoration)
        );
    }

    if public_after.as_ref() == Some(&published_token)
        && staged_after.is_none()
        && let Some(displaced_token) = displaced_after.as_ref()
        && displaced_token != &previous_token
    {
        let restoration = replace_windows_public_with_displaced(
            executable,
            &displaced_previous,
            displaced_token,
            &published_token,
            &failed_candidate,
        );
        if restoration.is_ok() {
            let backup_cleanup = remove_exact_transaction_path(
                &backup,
                &backup_token,
                "unused Windows self-update backup",
            );
            publication_owner.preserve_classified_error();
            anyhow::bail!(
                "Windows self-update displaced a file that no longer matched the authorized executable; the actual displaced writer was restored. Backup cleanup was {}",
                cleanup_result(backup_cleanup)
            );
        }
        publication_owner.preserve_classified_error();
        anyhow::bail!(
            "Windows self-update displaced a foreign file and exact restoration failed; no recovery entry was deleted. Public {}, displaced {}, independent backup {}. Restoration failed: {}",
            executable.display(),
            displaced_previous.display(),
            backup.display(),
            operation_result(restoration)
        );
    }

    publication_owner.preserve_classified_error();
    anyhow::bail!(
        "Windows self-update ended in an unclassified external-writer state: public={}, staged={}, displaced={} ({replace_detail}); no recovery entry was deleted. Public {}, staged {}, displaced {}, independent backup {}",
        describe_token_state(public_after.as_ref(), &published_token, &previous_token),
        describe_token_state(staged_after.as_ref(), &published_token, &previous_token),
        describe_token_state(displaced_after.as_ref(), &published_token, &previous_token),
        executable.display(),
        staged.display(),
        displaced_previous.display(),
        backup.display()
    );
}

#[cfg(windows)]
fn rollback_windows_replacement(
    executable: &Path,
    backup: &Path,
    backup_token: &crate::fs_ops::FileToken,
    previous_token: &crate::fs_ops::FileToken,
    published_token: &crate::fs_ops::FileToken,
    displaced_previous: &Path,
    failed_candidate: &Path,
) -> Result<()> {
    require_file_token(
        backup,
        backup_token,
        "independent previous-executable backup before rollback",
    )?;
    replace_windows_public_with_displaced(
        executable,
        displaced_previous,
        previous_token,
        published_token,
        failed_candidate,
    )
    .with_context(|| {
        format!(
            "rolling back Windows replacement: public {}, displaced previous {}, failed candidate {}, independent backup {}",
            executable.display(),
            displaced_previous.display(),
            failed_candidate.display(),
            backup.display()
        )
    })?;
    remove_exact_transaction_path(
        backup,
        backup_token,
        "redundant Windows previous-executable backup",
    )?
    .require_durable()
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

/// Returns true only for the rolling development prerelease identifier.
///
/// Other valid prereleases such as `-development` or `-rc.1` are independent
/// tagged releases and must not silently opt the installation into the bare
/// `dev` channel.
pub fn is_dev_version(version: &str) -> bool {
    let Ok(version) = Version::parse(&normalize_version(version)) else {
        return false;
    };
    let prerelease = version.pre.as_str();
    prerelease == "dev" || prerelease.starts_with("dev.")
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

    let actual = sha256_file(archive_path)?;

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

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
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

    fn set_commit_fault(point: CommitFaultPoint) {
        COMMIT_FAULT.with(|fault| {
            assert!(fault.replace(Some(point)).is_none());
        });
    }

    fn set_publication_rollback_panic() {
        PUBLICATION_ROLLBACK_PANIC.with(|fault| assert!(!fault.replace(true)));
    }

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

    #[test]
    fn update_lock_target_canonicalizes_existing_and_missing_destinations() {
        let temp = tempfile::tempdir().expect("create update-lock target fixture");
        let install_dir = temp.path().join("install");
        fs::create_dir(&install_dir).expect("create install directory");

        let existing = install_dir.join("existing-binary");
        fs::write(&existing, b"fixture").expect("create existing binary");
        assert_eq!(
            normalize_update_lock_target(
                &install_dir
                    .join("..")
                    .join("install")
                    .join("existing-binary"),
            )
            .expect("normalize existing destination"),
            fs::canonicalize(&existing).expect("canonicalize existing destination")
        );

        let missing = install_dir.join("new-binary");
        assert_eq!(
            normalize_update_lock_target(
                &install_dir.join("..").join("install").join("new-binary"),
            )
            .expect("normalize missing destination"),
            fs::canonicalize(&install_dir)
                .expect("canonicalize install directory")
                .join("new-binary")
        );
        assert!(!missing.exists());
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

    #[cfg(windows)]
    const RUNNING_IMAGE_TEST_ROLE: &str = "CSGP_RUNNING_IMAGE_TEST_ROLE";
    #[cfg(windows)]
    const RUNNING_IMAGE_TEST_TARGET: &str = "CSGP_RUNNING_IMAGE_TEST_TARGET";
    #[cfg(windows)]
    const RUNNING_IMAGE_TEST_CANDIDATE: &str = "CSGP_RUNNING_IMAGE_TEST_CANDIDATE";
    #[cfg(windows)]
    const RUNNING_IMAGE_TEST_FAIL_CLEANUP_SENTINEL: &str =
        "CSGP_RUNNING_IMAGE_TEST_FAIL_CLEANUP_SENTINEL";
    #[cfg(windows)]
    const RUNNING_IMAGE_TEST_HOLDER_EXIT_SENTINEL: &str =
        "CSGP_RUNNING_IMAGE_TEST_HOLDER_EXIT_SENTINEL";
    #[cfg(windows)]
    const RUNNING_IMAGE_TEST_NAME: &str =
        "update::tests::windows_commit_cleans_a_running_old_image_after_process_exit";

    #[cfg(windows)]
    fn typed_result_fixture() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        PathBuf,
        SelfUpdateResult,
    ) {
        let (temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        let replacement = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .expect("publish candidate");
        let backup = replacement.backup.clone();
        let displaced = replacement.displaced_previous.clone();
        let result = SelfUpdateResult {
            current_version: "1.0.0".to_string(),
            latest_version: "2.0.0".to_string(),
            install_source: InstallSource::Direct,
            updated: true,
            replacement: Some(replacement),
            replacement_state: SelfUpdateReplacementState::Pending,
            transaction_lease: None,
        };
        (temp, executable, backup, displaced, result)
    }

    #[cfg(windows)]
    fn windows_recovery_entries(executable: &Path, prefix: &str) -> Vec<PathBuf> {
        let name_prefix = transaction_sibling_path(executable, prefix)
            .expect("build recovery prefix")
            .file_name()
            .expect("recovery prefix file name")
            .to_string_lossy()
            .into_owned();
        let mut entries = fs::read_dir(executable.parent().unwrap())
            .expect("read replacement fixture")
            .map(|entry| entry.expect("read replacement entry").path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&name_prefix))
            })
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    #[cfg(windows)]
    fn only_windows_recovery_entry(executable: &Path, prefix: &str) -> PathBuf {
        let entries = windows_recovery_entries(executable, prefix);
        assert_eq!(entries.len(), 1, "expected one {prefix} recovery entry");
        entries.into_iter().next().unwrap()
    }

    #[cfg(windows)]
    fn assert_random_windows_recovery_name(executable: &Path, path: &Path, prefix: &str) {
        let name_prefix = transaction_sibling_path(executable, prefix)
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let name = path.file_name().unwrap().to_string_lossy();
        let nonce = name
            .strip_prefix(&name_prefix)
            .expect("recovery name must use its transaction prefix");
        assert_eq!(nonce.len(), 32, "recovery nonce must encode 128 bits");
        assert_eq!(hex::decode(nonce).unwrap().len(), 16);
    }

    #[cfg(windows)]
    #[test]
    fn windows_random_recovery_path_retries_collisions_with_a_fixed_bound() {
        let (temp, executable, _candidate) = windows_replacement_fixture();
        let colliding_nonce = [0x11; 16];
        let available_nonce = [0x22; 16];
        let collision = transaction_sibling_path(
            &executable,
            &format!(
                "{WINDOWS_DISPLACED_RECOVERY_PREFIX}{}",
                hex::encode(colliding_nonce)
            ),
        )
        .unwrap();
        fs::write(&collision, b"foreign recovery entry").unwrap();

        let mut calls = 0;
        let allocated = windows_recovery_sibling_path_with_nonce_source(
            &executable,
            WINDOWS_DISPLACED_RECOVERY_PREFIX,
            "test displaced executable",
            || {
                calls += 1;
                if calls == 1 {
                    colliding_nonce
                } else {
                    available_nonce
                }
            },
        )
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(
            allocated,
            transaction_sibling_path(
                &executable,
                &format!(
                    "{WINDOWS_DISPLACED_RECOVERY_PREFIX}{}",
                    hex::encode(available_nonce)
                )
            )
            .unwrap()
        );

        let error = windows_recovery_sibling_path_with_nonce_source(
            &executable,
            WINDOWS_DISPLACED_RECOVERY_PREFIX,
            "test displaced executable",
            || colliding_nonce,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&WINDOWS_RECOVERY_PATH_COLLISION_RETRY_LIMIT.to_string()),
            "{error:#}"
        );
        drop(temp);
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

    #[cfg(not(windows))]
    fn set_unix_replace_fault(point: UnixReplaceFaultPoint) {
        UNIX_REPLACE_FAULT.with(|fault| {
            assert!(fault.replace(Some(point)).is_none());
        });
    }

    #[cfg(not(windows))]
    fn unix_pending_replacement_fixture()
    -> (tempfile::TempDir, PathBuf, PathBuf, PendingReplacement) {
        let temp = tempfile::tempdir().expect("create Unix replacement fixture");
        let executable = temp.path().join("codex-switch-global-pace");
        let candidate = temp.path().join("candidate");
        let backup = transaction_sibling_path(&executable, ".self-update-backup").unwrap();
        let displaced_previous = temp.path().join("displaced-previous");
        fs::write(&executable, b"old executable").expect("write old executable");
        fs::write(&candidate, b"new executable").expect("write candidate executable");
        let previous_token = crate::fs_ops::token_for_path(&executable).unwrap();
        let published_token = crate::fs_ops::token_for_path(&candidate).unwrap();
        let backup_token =
            crate::fs_ops::create_exclusive_copy(&executable, &backup, &previous_token)
                .unwrap()
                .token()
                .clone();
        fs::rename(&executable, &displaced_previous).expect("displace old executable");
        fs::rename(&candidate, &executable).expect("publish candidate");
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        let pending = PendingReplacement {
            lease: Some(lease),
            state: ReplacementState::Pending,
            completion: None,
            executable: executable.clone(),
            backup: backup.clone(),
            backup_token,
            previous_token,
            published_token,
            displaced_previous,
        };
        (temp, executable, backup, pending)
    }

    #[cfg(not(windows))]
    fn typed_result_fixture() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        PathBuf,
        SelfUpdateResult,
    ) {
        let (temp, executable, backup, pending) = unix_pending_replacement_fixture();
        let displaced = pending.displaced_previous.clone();
        let result = SelfUpdateResult {
            current_version: "1.0.0".to_string(),
            latest_version: "2.0.0".to_string(),
            install_source: InstallSource::Direct,
            updated: true,
            replacement: Some(pending),
            replacement_state: SelfUpdateReplacementState::Pending,
            transaction_lease: None,
        };
        (temp, executable, backup, displaced, result)
    }

    #[test]
    fn pending_commit_failure_preserves_and_describes_actual_recovery_paths() {
        let (_temp, executable, backup, displaced, mut result) = typed_result_fixture();
        fs::write(&executable, b"externally changed executable")
            .expect("inject a public executable token mismatch");

        let commit_error = result
            .commit_replacement()
            .expect_err("a changed published executable must fail commit");
        assert!(
            format!("{commit_error:#}").contains("published self-update candidate at commit"),
            "{commit_error:#}"
        );
        assert_eq!(
            result.replacement_state(),
            SelfUpdateReplacementState::Pending,
            "pre-cleanup commit failure must retain the pending recovery owner"
        );

        let recovery = result
            .preserve_failed_commit_for_recovery()
            .expect("preserve the failed commit recovery set")
            .expect("a pending failed commit must expose recovery paths");
        assert_eq!(
            result.replacement_state(),
            SelfUpdateReplacementState::Preserved
        );
        let description = recovery.describe();
        for path in [&executable, &backup, &displaced] {
            assert!(
                description.contains(&path.display().to_string()),
                "missing actual recovery path {} in {description}",
                path.display()
            );
        }
        for path in [&backup, &displaced] {
            let entry = recovery
                .entries
                .iter()
                .find(|entry| &entry.path == path)
                .expect("recovery path observation");
            assert_eq!(entry.state, ReplacementRecoveryEntryState::ExactPresent);
        }
    }

    #[test]
    fn panic_before_final_recovery_cleanup_never_claims_old_binary_restoration() {
        let (_temp, executable, backup, displaced, mut result) = typed_result_fixture();
        set_commit_fault(CommitFaultPoint::BeforeFinalRecoveryCleanup);

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = result.commit_replacement();
        }));

        assert!(panic.is_err());
        assert_eq!(
            result.replacement_state(),
            SelfUpdateReplacementState::Preserved
        );
        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
        #[cfg(not(windows))]
        {
            assert_eq!(fs::read(&backup).unwrap(), b"old executable");
            assert!(
                !displaced.exists(),
                "the first exact recovery entry was already removed, so automatic rollback must not be claimed"
            );
        }
        #[cfg(windows)]
        {
            assert!(
                !backup.exists(),
                "the fixed backup is consumed before mapped-image cleanup"
            );
            assert_eq!(fs::read(&displaced).unwrap(), b"old executable");
        }
        let recovery = result
            .preserve_replacement_for_recovery()
            .expect("classify the remaining recovery entries");
        let backup_entry = recovery
            .entries
            .iter()
            .find(|entry| entry.path == backup)
            .expect("backup observation");
        #[cfg(not(windows))]
        assert_eq!(
            backup_entry.state,
            ReplacementRecoveryEntryState::ExactPresent
        );
        #[cfg(windows)]
        assert_eq!(backup_entry.state, ReplacementRecoveryEntryState::Absent);
        let displaced_entry = recovery
            .entries
            .iter()
            .find(|entry| entry.path == displaced)
            .expect("displaced observation");
        #[cfg(not(windows))]
        assert_eq!(displaced_entry.state, ReplacementRecoveryEntryState::Absent);
        #[cfg(windows)]
        assert_eq!(
            displaced_entry.state,
            ReplacementRecoveryEntryState::ExactPresent
        );
        let description = recovery.describe();
        assert!(description.contains("independent previous-executable backup"));
        assert!(description.contains("exact entry is present"));
        assert!(description.contains("displaced previous executable"));
        assert!(description.contains("entry is absent"));
    }

    #[test]
    fn panic_after_final_recovery_cleanup_reports_live_committed_state() {
        let (_temp, executable, backup, displaced, mut result) = typed_result_fixture();
        set_commit_fault(CommitFaultPoint::AfterFinalRecoveryCleanup);

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = result.commit_replacement();
        }));

        assert!(panic.is_err());
        assert_eq!(
            result.replacement_state(),
            SelfUpdateReplacementState::Committed
        );
        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
        assert!(!backup.exists());
        assert!(!displaced.exists());
        let recovery = result
            .preserve_replacement_for_recovery()
            .expect("observe the finished replacement without changing its live state");
        assert_eq!(
            result.replacement_state(),
            SelfUpdateReplacementState::Committed
        );
        assert!(
            recovery
                .entries
                .iter()
                .filter(|entry| entry.path == backup || entry.path == displaced)
                .all(|entry| entry.state == ReplacementRecoveryEntryState::Absent)
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_pending_replacement_preserves_backup_until_explicit_commit() {
        let (_temp, executable, backup, mut pending) = unix_pending_replacement_fixture();

        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
        assert_eq!(fs::read(&backup).unwrap(), b"old executable");
        pending.commit().expect("commit replacement");

        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
        assert!(!backup.exists());
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_daemon_failure_restores_the_exact_previous_executable() {
        let (_temp, executable, backup, mut pending) = unix_pending_replacement_fixture();

        pending.rollback().expect("rollback failed candidate");

        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        assert!(!backup.exists());
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_rollback_refuses_to_overwrite_a_changed_executable() {
        let (_temp, executable, backup, mut pending) = unix_pending_replacement_fixture();
        let external = executable.with_extension("external");
        fs::write(&external, b"external executable").unwrap();
        fs::rename(&external, &executable).unwrap();

        let error = pending.rollback().unwrap_err();

        assert!(format!("{error:#}").contains("changed"));
        assert_eq!(fs::read(&executable).unwrap(), b"external executable");
        assert_eq!(fs::read(&backup).unwrap(), b"old executable");
    }

    #[cfg(not(windows))]
    #[test]
    fn dropping_uncommitted_unix_replacement_keeps_manual_recovery_data() {
        let (_temp, executable, backup, pending) = unix_pending_replacement_fixture();

        drop(pending);

        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
        assert_eq!(fs::read(&backup).unwrap(), b"old executable");
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_commit_does_not_delete_a_replaced_backup_path() {
        let (temp, executable, backup, mut pending) = unix_pending_replacement_fixture();
        let preserved = temp.path().join("actual-old-executable");
        fs::rename(&backup, &preserved).unwrap();
        fs::write(&backup, b"external file").unwrap();

        let error = pending
            .commit()
            .expect_err("changed backup path must fail commit");

        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
        assert_eq!(fs::read(&preserved).unwrap(), b"old executable");
        assert_eq!(fs::read(&backup).unwrap(), b"external file");
        assert!(format!("{error:#}").contains("old executable backup"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_publication_refuses_to_overwrite_an_executable_changed_during_staging() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("codex-switch-global-pace");
        let candidate = temp.path().join("candidate");
        fs::write(&executable, b"old executable").unwrap();
        fs::write(&candidate, b"new executable").unwrap();
        let lease = acquire_update_lease(&executable).unwrap();
        set_unix_replace_fault(UnixReplaceFaultPoint::CurrentExecutableChangedBeforePublish);

        let error = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Unix,
            "v1.2.3",
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("changed"), "{error:#}");
        assert_eq!(fs::read(&executable).unwrap(), b"external executable");
        let backup = transaction_sibling_path(&executable, ".self-update-backup").unwrap();
        assert!(!backup.exists());
        assert!(
            !transaction_sibling_path(&executable, ".self-update-candidate")
                .unwrap()
                .exists()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_publication_preserves_a_writer_that_claims_the_atomic_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("codex-switch-global-pace");
        let candidate = temp.path().join("candidate");
        fs::write(&executable, b"old executable").unwrap();
        fs::write(&candidate, b"new executable").unwrap();
        let lease = acquire_update_lease(&executable).unwrap();
        set_unix_replace_fault(UnixReplaceFaultPoint::ConcurrentExecutableClaimedPublicPath);

        let error = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Unix,
            "v1.2.3",
        )
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("displaced writer was restored"),
            "{error:#}"
        );
        assert_eq!(fs::read(&executable).unwrap(), b"external executable");
        let backup = transaction_sibling_path(&executable, ".self-update-backup").unwrap();
        assert!(!backup.exists());
        assert!(
            !transaction_sibling_path(&executable, ".self-update-candidate")
                .unwrap()
                .exists()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_unwind_immediately_after_exchange_restores_the_old_executable() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("codex-switch-global-pace");
        let candidate = temp.path().join("candidate");
        fs::write(&executable, b"old executable").unwrap();
        fs::write(&candidate, b"new executable").unwrap();
        let lease = acquire_update_lease(&executable).unwrap();
        set_unix_replace_fault(UnixReplaceFaultPoint::AfterPublishBeforeClassification);

        let error = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Unix,
            "v1.2.3",
        )
        .expect_err("the injected post-exchange unwind must be recovered");

        assert!(format!("{error:#}").contains("exact previous executable was restored"));
        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        assert!(
            !transaction_sibling_path(&executable, ".self-update-backup")
                .unwrap()
                .exists()
        );
        assert!(
            !transaction_sibling_path(&executable, ".self-update-candidate")
                .unwrap()
                .exists()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_error_immediately_after_exchange_reports_exact_restoration() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("codex-switch-global-pace");
        let candidate = temp.path().join("candidate");
        fs::write(&executable, b"old executable").unwrap();
        fs::write(&candidate, b"new executable").unwrap();
        let lease = acquire_update_lease(&executable).unwrap();
        set_unix_replace_fault(UnixReplaceFaultPoint::AfterPublishBeforeClassificationError);

        let error = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Unix,
            "v1.2.3",
        )
        .expect_err("the injected post-exchange error must be recovered");

        let message = format!("{error:#}");
        assert!(message.contains("injected error after Unix executable publication"));
        assert!(message.contains("exact previous executable was restored"));
        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        assert!(
            !transaction_sibling_path(&executable, ".self-update-backup")
                .unwrap()
                .exists()
        );
        assert!(
            !transaction_sibling_path(&executable, ".self-update-candidate")
                .unwrap()
                .exists()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn nested_unwind_during_unix_publication_rollback_preserves_exact_paths() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("codex-switch-global-pace");
        let candidate = temp.path().join("candidate");
        fs::write(&executable, b"old executable").unwrap();
        fs::write(&candidate, b"new executable").unwrap();
        let lease = acquire_update_lease(&executable).unwrap();
        set_unix_replace_fault(UnixReplaceFaultPoint::AfterPublishBeforeClassification);
        set_publication_rollback_panic();

        let error = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Unix,
            "v1.2.3",
        )
        .expect_err("nested rollback unwind must be classified without process abort");
        let message = format!("{error:#}");
        let backup = transaction_sibling_path(&executable, ".self-update-backup").unwrap();
        let displaced = transaction_sibling_path(&executable, ".self-update-candidate").unwrap();
        assert!(message.contains("rollback itself unwound"), "{message}");
        assert!(message.contains(&backup.display().to_string()));
        assert!(message.contains(&displaced.display().to_string()));
        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
        assert_eq!(fs::read(&backup).unwrap(), b"old executable");
        assert_eq!(fs::read(&displaced).unwrap(), b"old executable");
    }

    #[cfg(not(windows))]
    #[test]
    fn typed_result_distinguishes_pending_committed_and_rolled_back_states() {
        let (_temp, executable, _backup, pending) = unix_pending_replacement_fixture();
        let mut result = SelfUpdateResult {
            current_version: "1.0.0".to_string(),
            latest_version: "2.0.0".to_string(),
            install_source: InstallSource::Direct,
            updated: true,
            replacement: Some(pending),
            replacement_state: SelfUpdateReplacementState::Pending,
            transaction_lease: None,
        };
        assert_eq!(
            result.replacement_state(),
            SelfUpdateReplacementState::Pending
        );
        result
            .rollback_replacement()
            .expect("roll back pending replacement");
        assert_eq!(
            result.replacement_state(),
            SelfUpdateReplacementState::RolledBack
        );
        assert_eq!(fs::read(&executable).unwrap(), b"old executable");

        let (_temp, executable, _backup, pending) = unix_pending_replacement_fixture();
        let mut committed = SelfUpdateResult {
            current_version: "1.0.0".to_string(),
            latest_version: "2.0.0".to_string(),
            install_source: InstallSource::Direct,
            updated: true,
            replacement: Some(pending),
            replacement_state: SelfUpdateReplacementState::Pending,
            transaction_lease: None,
        };
        committed
            .commit_replacement()
            .expect("commit pending replacement");
        assert_eq!(
            committed.replacement_state(),
            SelfUpdateReplacementState::Committed
        );
        committed
            .rollback_replacement()
            .expect("committed rollback is a no-op");
        assert_eq!(
            committed.replacement_state(),
            SelfUpdateReplacementState::Committed
        );
        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_no_replace_rename_never_overwrites_the_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::write(&source, b"source").unwrap();
        fs::write(&destination, b"destination").unwrap();

        crate::fs_ops::rename_noreplace(&source, &destination).unwrap_err();

        assert_eq!(fs::read(&source).unwrap(), b"source");
        assert_eq!(fs::read(&destination).unwrap(), b"destination");
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
            !transaction_sibling_path(&executable, ".self-update-backup")
                .unwrap()
                .exists()
        );
        assert!(
            windows_recovery_entries(&executable, WINDOWS_DISPLACED_RECOVERY_PREFIX).is_empty()
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
            error.to_string().contains("partial failure (1177)"),
            "{error:#}"
        );
        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        assert!(
            !transaction_sibling_path(&executable, ".self-update-backup")
                .unwrap()
                .exists()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_1177_publish_recovery_failure_preserves_every_recovery_path() {
        let (_temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        let backup = transaction_sibling_path(&executable, ".self-update-backup").unwrap();
        let staged = transaction_sibling_path(&executable, ".self-update-candidate").unwrap();
        set_windows_replace_fault(WindowsReplaceFaultPoint::OriginalMovedToBackupAndBlockRestore);

        let error = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .expect_err("the blocked 1177 recovery must fail closed");
        let message = format!("{error:#}");
        assert!(message.contains("not a direct regular file"), "{error:#}");
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
        let displaced = only_windows_recovery_entry(&executable, WINDOWS_DISPLACED_RECOVERY_PREFIX);
        assert_eq!(fs::read(&displaced).unwrap(), b"old executable");

        fs::remove_dir(&executable).unwrap();
        fs::remove_file(&backup).unwrap();
        fs::remove_file(&staged).unwrap();
        fs::remove_file(&displaced).unwrap();
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
            !transaction_sibling_path(&executable, ".self-update-backup")
                .unwrap()
                .exists()
        );
        assert!(windows_recovery_entries(&executable, WINDOWS_FAILED_RECOVERY_PREFIX).is_empty());
        assert!(
            windows_recovery_entries(&executable, WINDOWS_DISPLACED_RECOVERY_PREFIX).is_empty()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_unwind_immediately_after_publication_restores_the_old_executable() {
        let (_temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        set_windows_replace_fault(WindowsReplaceFaultPoint::AfterPublishBeforeClassification);

        let error = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .expect_err("publication seam unwind must be converted to exact recovery");

        assert!(format!("{error:#}").contains("exact previous executable was restored"));
        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        assert!(
            !transaction_sibling_path(&executable, ".self-update-backup")
                .unwrap()
                .exists()
        );
        assert!(
            windows_recovery_entries(&executable, WINDOWS_DISPLACED_RECOVERY_PREFIX).is_empty()
        );
        assert!(windows_recovery_entries(&executable, WINDOWS_FAILED_RECOVERY_PREFIX).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_error_immediately_after_publication_reports_exact_restoration() {
        let (_temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        set_windows_replace_fault(WindowsReplaceFaultPoint::AfterPublishBeforeClassificationError);

        let error = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .expect_err("publication seam error must be converted to exact recovery");

        let message = format!("{error:#}");
        assert!(message.contains("AfterPublishBeforeClassificationError"));
        assert!(message.contains("exact previous executable was restored"));
        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        assert!(
            !transaction_sibling_path(&executable, ".self-update-backup")
                .unwrap()
                .exists()
        );
        assert!(
            windows_recovery_entries(&executable, WINDOWS_DISPLACED_RECOVERY_PREFIX).is_empty()
        );
        assert!(windows_recovery_entries(&executable, WINDOWS_FAILED_RECOVERY_PREFIX).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn nested_unwind_during_windows_publication_rollback_preserves_exact_paths() {
        let (_temp, executable, candidate) = windows_replacement_fixture();
        let lease = acquire_update_lease(&executable).expect("acquire update lease");
        set_windows_replace_fault(WindowsReplaceFaultPoint::AfterPublishBeforeClassification);
        set_publication_rollback_panic();

        let error = replace_candidate(
            &executable,
            &candidate,
            lease,
            UpdatePlatform::Windows,
            "v1.2.3",
        )
        .expect_err("nested rollback unwind must be classified without process abort");
        let message = format!("{error:#}");
        let backup = transaction_sibling_path(&executable, ".self-update-backup").unwrap();
        let displaced = only_windows_recovery_entry(&executable, WINDOWS_DISPLACED_RECOVERY_PREFIX);
        assert!(message.contains("rollback itself unwound"), "{message}");
        assert!(message.contains(&backup.display().to_string()));
        assert!(message.contains(&displaced.display().to_string()));
        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
        assert_eq!(fs::read(&backup).unwrap(), b"old executable");
        assert_eq!(fs::read(&displaced).unwrap(), b"old executable");
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
        let failed_candidate = replacement.failed_candidate.clone();
        let displaced_previous = replacement.displaced_previous.clone();
        assert_eq!(fs::read(&executable).unwrap(), b"new executable");

        replacement.rollback().expect("restore old executable");

        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        assert!(
            !transaction_sibling_path(&executable, ".self-update-backup")
                .unwrap()
                .exists()
        );
        assert!(!failed_candidate.exists());
        assert!(!displaced_previous.exists());
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
        let failed_candidate = replacement.failed_candidate.clone();
        let displaced_previous = replacement.displaced_previous.clone();
        set_windows_replace_fault(WindowsReplaceFaultPoint::OriginalMovedToBackup);

        replacement
            .rollback()
            .expect("1177 rollback recovery must restore the previous executable");

        assert_eq!(fs::read(&executable).unwrap(), b"old executable");
        assert!(
            !transaction_sibling_path(&executable, ".self-update-backup")
                .unwrap()
                .exists()
        );
        assert!(
            !failed_candidate.exists(),
            "the exact randomized failed-candidate path must be removed"
        );
        assert!(
            !displaced_previous.exists(),
            "the exact randomized displaced path must be removed"
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
        let backup = transaction_sibling_path(&executable, ".self-update-backup").unwrap();
        let failed = replacement.failed_candidate.clone();
        let displaced = replacement.displaced_previous.clone();
        set_windows_replace_fault(WindowsReplaceFaultPoint::OriginalMovedToBackupAndBlockRestore);

        let error = replacement
            .rollback()
            .expect_err("the blocked rollback recovery must fail closed");

        let message = format!("{error:#}");
        assert!(message.contains("not a direct regular file"), "{error:#}");
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
        assert_eq!(fs::read(&displaced).unwrap(), b"old executable");

        fs::remove_dir(&executable).unwrap();
        fs::remove_file(&backup).unwrap();
        fs::remove_file(&failed).unwrap();
        fs::remove_file(&displaced).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn typed_rollback_error_reports_only_observed_exact_recovery_entries() {
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
        let backup = replacement.backup.clone();
        let displaced = replacement.displaced_previous.clone();
        let failed = replacement.failed_candidate.clone();
        let mut result = SelfUpdateResult {
            current_version: "1.0.0".to_string(),
            latest_version: "2.0.0".to_string(),
            install_source: InstallSource::Direct,
            updated: true,
            replacement: Some(replacement),
            replacement_state: SelfUpdateReplacementState::Pending,
            transaction_lease: None,
        };
        set_windows_replace_fault(WindowsReplaceFaultPoint::OriginalMovedToBackupAndBlockRestore);

        let error = result
            .rollback_replacement()
            .expect_err("the injected rollback must retain a typed recovery observation");
        let message = format!("{error:#}");
        for path in [&backup, &displaced, &failed] {
            assert!(message.contains(&path.display().to_string()), "{message}");
        }
        assert!(message.contains("exact entry is present"), "{message}");
        assert_eq!(
            result.replacement_state(),
            SelfUpdateReplacementState::Preserved
        );

        fs::remove_dir(&executable).unwrap();
        fs::remove_file(&backup).unwrap();
        fs::remove_file(&failed).unwrap();
        fs::remove_file(&displaced).unwrap();
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
        let backup = transaction_sibling_path(&executable, ".self-update-backup").unwrap();

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
        let backup = transaction_sibling_path(&executable, ".self-update-backup").unwrap();
        let displaced = replacement.displaced_previous.clone();
        let failed = replacement.failed_candidate.clone();
        assert_random_windows_recovery_name(
            &executable,
            &displaced,
            WINDOWS_DISPLACED_RECOVERY_PREFIX,
        );
        assert_random_windows_recovery_name(&executable, &failed, WINDOWS_FAILED_RECOVERY_PREFIX);
        let mut result = SelfUpdateResult {
            current_version: "1.0.0".to_string(),
            latest_version: "2.0.0".to_string(),
            install_source: InstallSource::Direct,
            updated: true,
            replacement: Some(replacement),
            replacement_state: SelfUpdateReplacementState::Pending,
            transaction_lease: None,
        };

        let recovery = result
            .preserve_replacement_for_recovery()
            .expect("preserve pending replacement");

        assert_eq!(recovery.executable, executable);
        assert_eq!(recovery.entries[0].path, backup);
        assert_eq!(
            recovery.entries[0].state,
            ReplacementRecoveryEntryState::ExactPresent
        );
        assert_eq!(recovery.entries[1].path, displaced);
        assert_eq!(
            recovery.entries[1].state,
            ReplacementRecoveryEntryState::ExactPresent
        );
        assert_eq!(recovery.entries[2].path, failed);
        assert_eq!(
            recovery.entries[2].state,
            ReplacementRecoveryEntryState::Absent
        );
        assert_eq!(fs::read(&recovery.executable).unwrap(), b"new executable");
        assert_eq!(
            fs::read(&recovery.entries[0].path).unwrap(),
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
        let backup = transaction_sibling_path(&executable, ".self-update-backup").unwrap();
        let displaced = replacement.displaced_previous.clone();
        assert!(backup.exists());

        replacement.commit().expect("commit replacement");

        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
        assert!(!backup.exists());
        assert!(!displaced.exists());
    }

    #[cfg(windows)]
    #[test]
    fn windows_self_update_cleanup_worker_process_entry() {
        if cleanup_worker::run_from_test_env()
            .expect("run exact self-update cleanup worker fixture")
        {
            std::process::exit(0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_commit_cleans_a_running_old_image_after_process_exit() {
        use std::io::{BufRead as _, Read as _, Write as _};
        use std::process::Stdio;

        match std::env::var(RUNNING_IMAGE_TEST_ROLE).as_deref() {
            Ok("holder") => {
                println!("running image holder ready");
                std::io::stdout().flush().expect("flush holder readiness");
                let mut input = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut input)
                    .expect("wait for updater release");
                if let Some(sentinel) = std::env::var_os(RUNNING_IMAGE_TEST_HOLDER_EXIT_SENTINEL) {
                    fs::write(sentinel, b"mapped-image holder exited")
                        .expect("publish holder-exit sentinel");
                }
                return;
            }
            Ok("updater") => {
                let target = fs::canonicalize(PathBuf::from(
                    std::env::var_os(RUNNING_IMAGE_TEST_TARGET).expect("running image target"),
                ))
                .expect("resolve running image target");
                let candidate = fs::canonicalize(PathBuf::from(
                    std::env::var_os(RUNNING_IMAGE_TEST_CANDIDATE)
                        .expect("running image candidate"),
                ))
                .expect("resolve running image candidate");
                if let Some(sentinel) = std::env::var_os(RUNNING_IMAGE_TEST_FAIL_CLEANUP_SENTINEL) {
                    cleanup_worker::fail_after_parent_exit_once(PathBuf::from(sentinel));
                }
                let mut holder_command = std::process::Command::new(&target);
                holder_command
                    .arg(RUNNING_IMAGE_TEST_NAME)
                    .arg("--exact")
                    .arg("--nocapture")
                    .env(RUNNING_IMAGE_TEST_ROLE, "holder")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit());
                let mut holder = holder_command.spawn().expect("spawn mapped-image holder");
                let holder_input = holder.stdin.take().expect("holder stdin");
                let mut holder_output =
                    std::io::BufReader::new(holder.stdout.take().expect("holder readiness output"));
                let mut marker = String::new();
                loop {
                    marker.clear();
                    let read = holder_output
                        .read_line(&mut marker)
                        .expect("read holder readiness");
                    assert_ne!(read, 0, "holder exited before readiness");
                    if marker.contains("running image holder ready") {
                        break;
                    }
                }

                let lease = acquire_update_lease(&target).expect("acquire updater lease");
                let mut replacement = replace_candidate(
                    &target,
                    &candidate,
                    lease,
                    UpdatePlatform::Windows,
                    "v-running-image-test",
                )
                .expect("publish over the running copied executable");
                let backup = replacement.backup.clone();
                use_external_windows_cleanup_worker_once();
                replacement
                    .commit()
                    .expect("commit running-image replacement");
                assert!(
                    !backup.exists(),
                    "the fixed backup must not survive a successful commit"
                );

                drop(holder_input);
                assert!(
                    holder
                        .wait()
                        .expect("wait for mapped-image holder")
                        .success(),
                    "mapped-image holder did not exit cleanly"
                );
                return;
            }
            Ok("recover") => {
                assert!(
                    recover_pending_self_update_cleanup_on_startup()
                        .expect("recover journaled self-update cleanup"),
                    "the next execution did not observe its pending cleanup journal"
                );
                return;
            }
            Ok(other) => panic!("unexpected running-image helper role {other}"),
            Err(_) => {}
        }

        let temp = tempfile::tempdir().expect("create running-image replacement fixture");
        let source = std::env::current_exe().expect("locate test executable");
        let target = temp.path().join("running-self-updater.exe");
        let candidate = temp.path().join("replacement-candidate.exe");
        fs::copy(&source, &target).expect("copy running updater image");
        fs::hard_link(&source, &candidate).expect("link replacement candidate fixture");
        let target = fs::canonicalize(target).expect("resolve copied updater image");
        let candidate = fs::canonicalize(candidate).expect("resolve first update candidate");
        let crash_holder_exit_sentinel = temp.path().join("crash-holder-exited");

        let wait_for_fixture = |mut child: std::process::Child, purpose: &str| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
            loop {
                if let Some(status) = child.try_wait().expect("poll copied executable fixture") {
                    return status;
                }
                if std::time::Instant::now() >= deadline {
                    child
                        .kill()
                        .expect("terminate hung copied executable fixture");
                    child.wait().expect("reap hung copied executable fixture");
                    panic!("{purpose} did not exit within 240 seconds");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let run_updater =
            |candidate: &Path, failure_sentinel: Option<&Path>, exit_after_backup: bool| {
                let mut updater_command = std::process::Command::new(&target);
                updater_command
                    .arg(RUNNING_IMAGE_TEST_NAME)
                    .arg("--exact")
                    .arg("--nocapture")
                    .env(RUNNING_IMAGE_TEST_ROLE, "updater")
                    .env(RUNNING_IMAGE_TEST_TARGET, &target)
                    .env(RUNNING_IMAGE_TEST_CANDIDATE, candidate)
                    .env_remove(RUNNING_IMAGE_TEST_FAIL_CLEANUP_SENTINEL)
                    .env_remove(RUNNING_IMAGE_TEST_HOLDER_EXIT_SENTINEL)
                    .env_remove(WINDOWS_COMMIT_EXIT_AFTER_BACKUP_ENV)
                    .stdin(Stdio::null())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
                if let Some(sentinel) = failure_sentinel {
                    updater_command.env(RUNNING_IMAGE_TEST_FAIL_CLEANUP_SENTINEL, sentinel);
                }
                if exit_after_backup {
                    updater_command
                        .env(WINDOWS_COMMIT_EXIT_AFTER_BACKUP_ENV, "1")
                        .env(
                            RUNNING_IMAGE_TEST_HOLDER_EXIT_SENTINEL,
                            &crash_holder_exit_sentinel,
                        );
                }
                let updater = updater_command.spawn().expect("run copied self-updater");
                wait_for_fixture(updater, "copied self-updater")
            };

        let pending_journal = cleanup_worker::journal_path(&target)
            .expect("derive pending self-update cleanup journal");
        let fixed_backup = transaction_sibling_path(&target, ".self-update-backup").unwrap();
        let run_recovery = || {
            let mut recovery_command = std::process::Command::new(&target);
            recovery_command
                .arg(RUNNING_IMAGE_TEST_NAME)
                .arg("--exact")
                .arg("--nocapture")
                .env(RUNNING_IMAGE_TEST_ROLE, "recover")
                .env_remove(RUNNING_IMAGE_TEST_FAIL_CLEANUP_SENTINEL)
                .env_remove(RUNNING_IMAGE_TEST_HOLDER_EXIT_SENTINEL)
                .env_remove(WINDOWS_COMMIT_EXIT_AFTER_BACKUP_ENV)
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            let recovery = recovery_command
                .spawn()
                .expect("run the next executable for journal recovery");
            let recovery_status = wait_for_fixture(recovery, "cleanup recovery execution");
            assert!(
                recovery_status.success(),
                "the next executable did not recover pending cleanup: {recovery_status}"
            );
        };

        // The durable journal must exist before the first cleanup mutation.
        // Terminate the actual updater immediately after it deletes the fixed
        // backup, before it can spawn a worker, then prove the next public
        // execution has enough exact authority to finish and unblock another
        // independent ReplaceFileW transaction.
        let crash_status = run_updater(&candidate, None, true);
        assert_eq!(
            crash_status.code(),
            Some(WINDOWS_COMMIT_EXIT_AFTER_BACKUP_CODE),
            "copied updater did not terminate at the post-backup crash boundary: {crash_status}"
        );
        let holder_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !crash_holder_exit_sentinel.exists() {
            assert!(
                std::time::Instant::now() < holder_deadline,
                "mapped-image holder did not finalize after its updater died"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            pending_journal.exists(),
            "post-backup process death lost its pre-mutation cleanup journal"
        );
        assert!(
            !fixed_backup.exists(),
            "crash fixture did not cross the fixed-backup deletion boundary"
        );
        assert_eq!(
            windows_recovery_entries(&target, WINDOWS_DISPLACED_RECOVERY_PREFIX).len(),
            1,
            "post-backup process death did not retain exactly one displaced image"
        );
        run_recovery();
        assert!(
            !pending_journal.exists(),
            "crash recovery left its journal behind"
        );
        assert!(
            windows_recovery_entries(&target, WINDOWS_DISPLACED_RECOVERY_PREFIX).is_empty(),
            "crash recovery left the exact displaced executable behind"
        );

        // A worker can acknowledge readiness and then encounter an AV or
        // permissions failure only after this updater exits. The successful
        // publication remains committed, while its exact durable journal must
        // make that cleanup recoverable by the next execution.
        let failure_candidate = temp.path().join("post-readiness-failure-candidate.exe");
        fs::copy(&source, &failure_candidate)
            .expect("copy a distinct post-readiness failure candidate");
        let failure_candidate =
            fs::canonicalize(failure_candidate).expect("resolve post-readiness failure candidate");
        let failure_sentinel = temp.path().join("post-readiness-cleanup-failed");
        let failure_status = run_updater(&failure_candidate, Some(&failure_sentinel), false);
        assert!(
            failure_status.success(),
            "copied self-updater failed before its deferred cleanup: {failure_status}"
        );

        let failure_deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while !failure_sentinel.exists() {
            assert!(
                std::time::Instant::now() < failure_deadline,
                "cleanup worker did not reach the injected post-parent-exit failure"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(pending_journal.exists(), "cleanup failure lost its journal");
        assert_eq!(
            windows_recovery_entries(&target, WINDOWS_DISPLACED_RECOVERY_PREFIX).len(),
            1,
            "cleanup failure did not retain exactly one displaced image"
        );
        assert!(
            !fixed_backup.exists(),
            "fixed backup residue must not return when deferred cleanup fails"
        );
        run_recovery();
        assert!(
            !pending_journal.exists(),
            "recovery left its journal behind"
        );
        assert!(
            windows_recovery_entries(&target, WINDOWS_DISPLACED_RECOVERY_PREFIX).is_empty(),
            "recovery left the exact displaced executable behind"
        );

        // Exercise the actual next running-image publication boundary. The
        // candidate is a copy, not a hard link to the original test image, so
        // this proves a distinct second ReplaceFileW transaction enters and
        // its normal post-exit worker consumes both exact recovery entries.
        let second_candidate = temp.path().join("second-candidate.exe");
        fs::copy(&source, &second_candidate)
            .expect("copy a distinct second update candidate fixture");
        let second_candidate =
            fs::canonicalize(second_candidate).expect("resolve second update candidate");
        let second_status = run_updater(&second_candidate, None, false);
        assert!(
            second_status.success(),
            "second copied self-updater failed: {second_status}"
        );

        let completed_journal =
            cleanup_worker::journal_path(&target).expect("derive completed cleanup journal");
        let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            if windows_recovery_entries(&target, WINDOWS_DISPLACED_RECOVERY_PREFIX).is_empty()
                && !completed_journal.exists()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < cleanup_deadline,
                "mapped previous image or its cleanup journal remained after updater and holder exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !transaction_sibling_path(&target, ".self-update-backup")
                .unwrap()
                .exists(),
            "fixed backup residue would block the next update"
        );
    }

    #[cfg(windows)]
    #[test]
    fn committed_result_state_cannot_be_misreported_as_rolled_back() {
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
        let mut result = SelfUpdateResult {
            current_version: "1.0.0".to_string(),
            latest_version: "2.0.0".to_string(),
            install_source: InstallSource::Direct,
            updated: true,
            replacement: Some(replacement),
            replacement_state: SelfUpdateReplacementState::Pending,
            transaction_lease: None,
        };

        assert_eq!(
            result.replacement_state(),
            SelfUpdateReplacementState::Pending
        );
        result.commit_replacement().expect("commit replacement");
        assert_eq!(
            result.replacement_state(),
            SelfUpdateReplacementState::Committed
        );
        result
            .rollback_replacement()
            .expect("committed rollback is a no-op");
        assert_eq!(
            result.replacement_state(),
            SelfUpdateReplacementState::Committed
        );
        assert_eq!(fs::read(&executable).unwrap(), b"new executable");
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
    fn is_dev_version_detects_only_the_rolling_dev_identifier() {
        assert!(is_dev_version("1.2.3-dev"));
        assert!(is_dev_version("1.2.3-dev.20260408143000"));
        assert!(is_dev_version("1.2.3-dev+abc1234"));
        assert!(is_dev_version("v1.2.3-dev"));
        assert!(!is_dev_version("1.2.3"));
        assert!(!is_dev_version("1.2.3-development"));
        assert!(!is_dev_version("1.2.3-rc.dev"));
        assert!(!is_dev_version("1.2.3+dev"));
        assert!(!is_dev_version("not-a-version-dev"));
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

        let resolved = canonical_executable_path(symlink_path).expect("resolve Homebrew symlink");
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
    fn executable_resolution_and_system_marker_checks_fail_closed() {
        let temp = tempfile::tempdir().expect("create temp directory");
        let missing_executable = temp.path().join("missing-executable");
        let error = canonical_executable_path(missing_executable).unwrap_err();
        assert!(
            error.to_string().contains("resolving executable path"),
            "{error:#}"
        );

        let marker = temp.path().join(SYSTEM_INSTALL_MARKER_NAME);
        assert!(!checked_regular_marker(&marker).unwrap());
        fs::write(&marker, b"").unwrap();
        assert!(checked_regular_marker(&marker).unwrap());
        fs::remove_file(&marker).unwrap();
        fs::create_dir(&marker).unwrap();
        let error = checked_regular_marker(&marker).unwrap_err();
        assert!(
            error.to_string().contains("not a regular file"),
            "{error:#}"
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
