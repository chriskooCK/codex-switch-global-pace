use anyhow::{Context, Result};

const READY_PREFIX: &str = "codex-switch-global-pace self-update cleanup ready";
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const READY_TOTAL_MAX_BYTES: usize = 4 * 1024;
const JOURNAL_VERSION: u8 = 1;
const JOURNAL_MAX_BYTES: u64 = 16 * 1024;
const JOURNAL_SUFFIX: &str = ".self-update-cleanup-journal";
#[cfg(test)]
const WORKER_TEST_NAME: &str = "update::tests::windows_self_update_cleanup_worker_process_entry";
#[cfg(test)]
const TEST_MODE_ENV: &str = "CSGP_SELF_UPDATE_CLEANUP_TEST_MODE";
#[cfg(test)]
const PARENT_PID_ENV: &str = "CSGP_SELF_UPDATE_CLEANUP_PARENT_PID";
#[cfg(test)]
const DISPLACED_ENV: &str = "CSGP_SELF_UPDATE_CLEANUP_DISPLACED";
#[cfg(test)]
const EXPECTED_TOKEN_ENV: &str = "CSGP_SELF_UPDATE_CLEANUP_EXPECTED_TOKEN";
#[cfg(test)]
const EXECUTABLE_TOKEN_ENV: &str = "CSGP_SELF_UPDATE_CLEANUP_EXECUTABLE_TOKEN";
#[cfg(test)]
const READY_NONCE_ENV: &str = "CSGP_SELF_UPDATE_CLEANUP_READY_NONCE";
#[cfg(test)]
const JOURNAL_PATH_ENV: &str = "CSGP_SELF_UPDATE_CLEANUP_JOURNAL_PATH";
#[cfg(test)]
const JOURNAL_TOKEN_ENV: &str = "CSGP_SELF_UPDATE_CLEANUP_JOURNAL_TOKEN";
#[cfg(test)]
const FAIL_AFTER_PARENT_SENTINEL_ENV: &str = "CSGP_SELF_UPDATE_CLEANUP_FAIL_AFTER_PARENT_SENTINEL";

#[cfg(test)]
thread_local! {
    static FAIL_AFTER_PARENT_SENTINEL: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn fail_after_parent_exit_once(sentinel: std::path::PathBuf) {
    FAIL_AFTER_PARENT_SENTINEL.with(|slot| {
        assert!(slot.borrow_mut().replace(sentinel).is_none());
    });
}

#[cfg(test)]
fn take_failure_sentinel() -> Option<std::path::PathBuf> {
    FAIL_AFTER_PARENT_SENTINEL.with(|slot| slot.borrow_mut().take())
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupJournal {
    version: u8,
    public_token: String,
    backup_file_name: Vec<u16>,
    backup_token: String,
    displaced_file_name: Vec<u16>,
    previous_token: String,
}

pub(super) struct PreparedCleanup {
    journal_path: std::path::PathBuf,
    journal_token: crate::fs_ops::FileToken,
}

struct ValidatedCleanup {
    journal_path: std::path::PathBuf,
    journal_token: crate::fs_ops::FileToken,
    public_executable: std::path::PathBuf,
    public_token: crate::fs_ops::FileToken,
    backup: std::path::PathBuf,
    backup_token: crate::fs_ops::FileToken,
    displaced_previous: std::path::PathBuf,
    previous_token: crate::fs_ops::FileToken,
}

pub(super) fn journal_path(public_executable: &std::path::Path) -> Result<std::path::PathBuf> {
    if !public_executable.is_absolute() {
        anyhow::bail!(
            "self-update cleanup public executable is not absolute: {}",
            public_executable.display()
        );
    }
    super::transaction_sibling_path(public_executable, JOURNAL_SUFFIX)
}

fn validate_displaced_sibling(
    public_executable: &std::path::Path,
    displaced_previous: &std::path::Path,
) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    if displaced_previous.parent() != public_executable.parent() {
        anyhow::bail!(
            "journaled displaced executable is not a sibling of {}: {}",
            public_executable.display(),
            displaced_previous.display()
        );
    }
    let expected_prefix = super::transaction_sibling_path(
        public_executable,
        super::WINDOWS_DISPLACED_RECOVERY_PREFIX,
    )?;
    let expected_prefix = expected_prefix
        .file_name()
        .context("self-update cleanup prefix has no file name")?
        .encode_wide()
        .collect::<Vec<_>>();
    let displaced_name = displaced_previous
        .file_name()
        .context("journaled displaced executable has no file name")?
        .encode_wide()
        .collect::<Vec<_>>();
    let nonce = displaced_name
        .strip_prefix(expected_prefix.as_slice())
        .context("journaled displaced executable does not use the transaction prefix")?;
    if nonce.len() != 32
        || !nonce
            .iter()
            .all(|unit| u8::try_from(*unit).is_ok_and(|byte| byte.is_ascii_hexdigit()))
    {
        anyhow::bail!("journaled displaced nonce must encode exactly 128 bits");
    }
    Ok(())
}

fn create_journal(
    public_executable: &std::path::Path,
    public_token: &crate::fs_ops::FileToken,
    backup: &std::path::Path,
    backup_token: &crate::fs_ops::FileToken,
    displaced_previous: &std::path::Path,
    previous_token: &crate::fs_ops::FileToken,
) -> Result<(std::path::PathBuf, crate::fs_ops::FileToken)> {
    use std::io::Write as _;
    use std::os::windows::ffi::OsStrExt as _;

    validate_displaced_sibling(public_executable, displaced_previous)?;
    let expected_backup =
        super::transaction_sibling_path(public_executable, ".self-update-backup")?;
    if backup != expected_backup {
        anyhow::bail!(
            "self-update backup is not the fixed transaction sibling of {}: {}",
            public_executable.display(),
            backup.display()
        );
    }
    require_file_token(public_executable, public_token, "published executable")?;
    require_file_token(
        backup,
        backup_token,
        "independent previous-executable backup",
    )?;
    require_file_token(
        displaced_previous,
        previous_token,
        "displaced previous executable",
    )?;
    let path = journal_path(public_executable)?;
    let journal = CleanupJournal {
        version: JOURNAL_VERSION,
        public_token: public_token.to_string(),
        backup_file_name: backup
            .file_name()
            .context("self-update backup has no file name")?
            .encode_wide()
            .collect(),
        backup_token: backup_token.to_string(),
        displaced_file_name: displaced_previous
            .file_name()
            .context("displaced executable has no file name")?
            .encode_wide()
            .collect(),
        previous_token: previous_token.to_string(),
    };
    let bytes =
        serde_json::to_vec_pretty(&journal).context("serializing self-update cleanup journal")?;
    let mut file = crate::fs_ops::create_new_file(&path, 0o600)
        .with_context(|| format!("exclusively creating cleanup journal {}", path.display()))?;
    let creation = (|| -> Result<crate::fs_ops::FileToken> {
        file.write_all(&bytes)
            .with_context(|| format!("writing cleanup journal {}", path.display()))?;
        file.flush()
            .with_context(|| format!("flushing cleanup journal {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing cleanup journal {}", path.display()))?;
        let token = crate::fs_ops::token_for_file(&mut file)
            .with_context(|| format!("binding cleanup journal {}", path.display()))?;
        if crate::fs_ops::token_for_path(&path)? != token {
            anyhow::bail!(
                "cleanup journal path changed before readiness: {}",
                path.display()
            );
        }
        Ok(token)
    })();
    let cleanup_token = crate::fs_ops::token_for_file(&mut file).ok();
    drop(file);
    match creation {
        Ok(token) => Ok((path, token)),
        Err(error) => {
            let cleanup = cleanup_token
                .as_ref()
                .map(|token| crate::fs_ops::remove_exact(&path, token));
            let cleanup_detail = match cleanup {
                Some(Ok(_)) => "the exact incomplete journal was removed".to_string(),
                Some(Err(cleanup_error)) => format!(
                    "the incomplete journal was preserved after exact cleanup failed: {cleanup_error:#}"
                ),
                None => {
                    "the incomplete journal could not be token-bound and was preserved".to_string()
                }
            };
            Err(error.context(cleanup_detail))
        }
    }
}

pub(super) fn prepare(
    public_executable: &std::path::Path,
    public_token: &crate::fs_ops::FileToken,
    backup: &std::path::Path,
    backup_token: &crate::fs_ops::FileToken,
    displaced_previous: &std::path::Path,
    previous_token: &crate::fs_ops::FileToken,
) -> Result<PreparedCleanup> {
    let (journal_path, journal_token) = create_journal(
        public_executable,
        public_token,
        backup,
        backup_token,
        displaced_previous,
        previous_token,
    )?;
    Ok(PreparedCleanup {
        journal_path,
        journal_token,
    })
}

fn read_journal(
    path: &std::path::Path,
    expected_token: Option<&crate::fs_ops::FileToken>,
) -> Result<(CleanupJournal, crate::fs_ops::FileToken)> {
    use std::io::{Read as _, Seek as _};

    let mut file = crate::fs_ops::open_direct_regular(path)
        .with_context(|| format!("opening cleanup journal {}", path.display()))?;
    let token = crate::fs_ops::token_for_file(&mut file)
        .with_context(|| format!("binding cleanup journal {}", path.display()))?;
    if expected_token.is_some_and(|expected| expected != &token) {
        anyhow::bail!(
            "self-update cleanup journal changed before worker readiness: {}",
            path.display()
        );
    }
    file.seek(std::io::SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    let read = std::io::Read::take(&mut file, JOURNAL_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading cleanup journal {}", path.display()))?;
    if read as u64 > JOURNAL_MAX_BYTES {
        anyhow::bail!(
            "self-update cleanup journal exceeds {JOURNAL_MAX_BYTES} bytes: {}",
            path.display()
        );
    }
    let after = crate::fs_ops::token_for_file(&mut file)
        .with_context(|| format!("rechecking cleanup journal {}", path.display()))?;
    if after != token || crate::fs_ops::token_for_path(path)? != token {
        anyhow::bail!(
            "self-update cleanup journal changed while it was read: {}",
            path.display()
        );
    }
    let journal = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing cleanup journal {}", path.display()))?;
    Ok((journal, token))
}

fn load_validated_cleanup(
    public_executable: &std::path::Path,
    path: &std::path::Path,
    expected_journal_token: Option<&crate::fs_ops::FileToken>,
) -> Result<ValidatedCleanup> {
    use std::os::windows::ffi::OsStringExt as _;

    let expected_path = journal_path(public_executable)?;
    if path != expected_path {
        anyhow::bail!(
            "self-update cleanup journal is not the path derived from the public executable: {}",
            path.display()
        );
    }
    let (journal, journal_token) = read_journal(path, expected_journal_token)?;
    if journal.version != JOURNAL_VERSION {
        anyhow::bail!(
            "unsupported self-update cleanup journal version {}",
            journal.version
        );
    }
    let backup_name = std::ffi::OsString::from_wide(&journal.backup_file_name);
    let backup_relative = std::path::Path::new(&backup_name);
    if backup_relative.components().count() != 1
        || backup_relative.file_name() != Some(backup_relative.as_os_str())
    {
        anyhow::bail!("journaled backup executable name is not one direct file name");
    }
    let backup = public_executable
        .parent()
        .context("public executable has no parent for cleanup recovery")?
        .join(backup_relative);
    let expected_backup =
        super::transaction_sibling_path(public_executable, ".self-update-backup")?;
    if backup != expected_backup {
        anyhow::bail!(
            "journaled backup executable is not the fixed transaction sibling of {}: {}",
            public_executable.display(),
            backup.display()
        );
    }
    let displaced_name = std::ffi::OsString::from_wide(&journal.displaced_file_name);
    let displaced_relative = std::path::Path::new(&displaced_name);
    if displaced_relative.components().count() != 1
        || displaced_relative.file_name() != Some(displaced_relative.as_os_str())
    {
        anyhow::bail!("journaled displaced executable name is not one direct file name");
    }
    let displaced_previous = public_executable
        .parent()
        .context("public executable has no parent for cleanup recovery")?
        .join(displaced_relative);
    validate_displaced_sibling(public_executable, &displaced_previous)?;
    let public_token = journal
        .public_token
        .parse::<crate::fs_ops::FileToken>()
        .context("parsing journaled public executable token")?;
    let backup_token = journal
        .backup_token
        .parse::<crate::fs_ops::FileToken>()
        .context("parsing journaled backup executable token")?;
    let previous_token = journal
        .previous_token
        .parse::<crate::fs_ops::FileToken>()
        .context("parsing journaled previous executable token")?;
    require_file_token(
        public_executable,
        &public_token,
        "journaled public executable",
    )?;
    match crate::fs_ops::token_if_present(&backup)? {
        Some(observed) if observed == backup_token => {}
        Some(_) => anyhow::bail!(
            "journaled backup executable changed before cleanup: {}",
            backup.display()
        ),
        None => {}
    }
    match crate::fs_ops::token_if_present(&displaced_previous)? {
        Some(observed) if observed == previous_token => {}
        Some(_) => anyhow::bail!(
            "journaled displaced executable changed before cleanup: {}",
            displaced_previous.display()
        ),
        None => {}
    }
    Ok(ValidatedCleanup {
        journal_path: path.to_path_buf(),
        journal_token,
        public_executable: public_executable.to_path_buf(),
        public_token,
        backup,
        backup_token,
        displaced_previous,
        previous_token,
    })
}

fn complete_cleanup(cleanup: &ValidatedCleanup) -> Result<()> {
    require_file_token(
        &cleanup.public_executable,
        &cleanup.public_token,
        "journaled public executable",
    )?;
    match crate::fs_ops::token_if_present(&cleanup.backup)? {
        Some(observed) if observed == cleanup.backup_token => {
            crate::fs_ops::remove_exact(&cleanup.backup, &cleanup.backup_token).with_context(
                || {
                    format!(
                        "removing journaled previous-executable backup {}",
                        cleanup.backup.display()
                    )
                },
            )?;
        }
        Some(_) => anyhow::bail!(
            "journaled backup executable changed before exact removal: {}",
            cleanup.backup.display()
        ),
        None => {}
    }
    match crate::fs_ops::token_if_present(&cleanup.displaced_previous)? {
        Some(observed) if observed == cleanup.previous_token => {
            crate::fs_ops::remove_exact(&cleanup.displaced_previous, &cleanup.previous_token)
                .with_context(|| {
                    format!(
                        "removing journaled displaced executable {}",
                        cleanup.displaced_previous.display()
                    )
                })?;
        }
        Some(_) => anyhow::bail!(
            "journaled displaced executable changed before exact removal: {}",
            cleanup.displaced_previous.display()
        ),
        None => {}
    }
    crate::fs_ops::remove_exact(&cleanup.journal_path, &cleanup.journal_token)
        .with_context(|| {
            format!(
                "removing completed self-update cleanup journal {}",
                cleanup.journal_path.display()
            )
        })
        .map(drop)
}

fn complete_after_revalidation(cleanup: &ValidatedCleanup) -> Result<()> {
    match crate::fs_ops::token_if_present(&cleanup.journal_path)? {
        Some(observed) if observed == cleanup.journal_token => {
            let current = load_validated_cleanup(
                &cleanup.public_executable,
                &cleanup.journal_path,
                Some(&cleanup.journal_token),
            )?;
            complete_cleanup(&current)
        }
        Some(_) => anyhow::bail!(
            "self-update cleanup journal changed after worker readiness: {}",
            cleanup.journal_path.display()
        ),
        None => {
            require_file_token(
                &cleanup.public_executable,
                &cleanup.public_token,
                "journaled public executable",
            )?;
            if crate::fs_ops::token_if_present(&cleanup.backup)?.is_none()
                && crate::fs_ops::token_if_present(&cleanup.displaced_previous)?.is_none()
            {
                // Another startup can win the update lease immediately after
                // the updater exits. Both exact absences prove that recovery
                // consumed this same journal before this worker acquired it.
                Ok(())
            } else {
                anyhow::bail!(
                    "self-update cleanup journal disappeared while a recovery executable remained: {}",
                    cleanup.journal_path.display()
                )
            }
        }
    }
}

fn path_entry_exists(path: &std::path::Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("inspecting cleanup journal {}", path.display()))
        }
    }
}

pub(super) fn recover_pending(public_executable: &std::path::Path) -> Result<bool> {
    let path = journal_path(public_executable)?;
    if !path_entry_exists(&path)? {
        return Ok(false);
    }
    let _lease = super::acquire_update_lease(public_executable)
        .context("locking self-update cleanup recovery")?;
    if !path_entry_exists(&path)? {
        return Ok(false);
    }
    let cleanup = load_validated_cleanup(public_executable, &path, None)?;
    complete_cleanup(&cleanup)?;
    Ok(true)
}

struct OwnedProcessHandle(windows_sys::Win32::Foundation::HANDLE);

impl Drop for OwnedProcessHandle {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: this object uniquely owns the non-null process handle
            // returned by OpenProcess.
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

fn open_parent(parent_pid: u32) -> Result<OwnedProcessHandle> {
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};

    if parent_pid == 0 {
        anyhow::bail!("self-update cleanup parent PID must not be zero");
    }
    let handle = unsafe {
        // SAFETY: OpenProcess receives a scalar PID and requests only the
        // synchronization right needed to wait for that exact process object.
        OpenProcess(PROCESS_SYNCHRONIZE, 0, parent_pid)
    };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!("opening self-update parent process {parent_pid} for exact cleanup")
        });
    }
    Ok(OwnedProcessHandle(handle))
}

fn require_file_token(
    path: &std::path::Path,
    expected: &crate::fs_ops::FileToken,
    purpose: &str,
) -> Result<()> {
    let observed = crate::fs_ops::token_for_path(path)
        .with_context(|| format!("verifying {purpose} at {}", path.display()))?;
    if &observed != expected {
        anyhow::bail!(
            "{purpose} changed before cleanup readiness: {}",
            path.display()
        );
    }
    Ok(())
}

pub(super) fn run(
    parent_pid: u32,
    displaced_previous: &std::path::Path,
    expected_token: &str,
    expected_executable_token: &str,
    journal_path: &std::path::Path,
    expected_journal_token: &str,
    ready_nonce: &str,
) -> Result<()> {
    use std::io::Write as _;
    use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

    if !displaced_previous.is_absolute() {
        anyhow::bail!(
            "self-update cleanup path is not absolute: {}",
            displaced_previous.display()
        );
    }
    crate::daemon::validate_background_ready_nonce(ready_nonce)?;
    let expected_token = expected_token
        .parse::<crate::fs_ops::FileToken>()
        .context("parsing the displaced previous-executable token")?;
    let expected_executable_token = expected_executable_token
        .parse::<crate::fs_ops::FileToken>()
        .context("parsing the cleanup worker executable token")?;
    let expected_journal_token = expected_journal_token
        .parse::<crate::fs_ops::FileToken>()
        .context("parsing the cleanup journal token")?;

    // Open the process object before readiness. The parent therefore knows the
    // worker is bound to this process lifetime rather than a later PID reuse.
    let parent = open_parent(parent_pid)?;
    let worker_executable = std::fs::canonicalize(
        std::env::current_exe()
            .context("locating the executable running the self-update cleanup worker")?,
    )
    .context("resolving the executable running the self-update cleanup worker")?;
    let cleanup = load_validated_cleanup(
        &worker_executable,
        journal_path,
        Some(&expected_journal_token),
    )?;
    if cleanup.public_token != expected_executable_token
        || cleanup.displaced_previous != displaced_previous
        || cleanup.previous_token != expected_token
    {
        anyhow::bail!("self-update cleanup command does not match its exact durable journal");
    }

    println!("{READY_PREFIX} {ready_nonce}");
    std::io::stdout()
        .flush()
        .context("flushing self-update cleanup worker readiness")?;

    let wait = unsafe {
        // SAFETY: `parent` owns a live process handle with SYNCHRONIZE access.
        WaitForSingleObject(parent.0, INFINITE)
    };
    if wait == WAIT_FAILED {
        return Err(std::io::Error::last_os_error())
            .context("waiting for the self-update parent process to exit");
    }
    if wait != WAIT_OBJECT_0 {
        anyhow::bail!("self-update parent wait returned unexpected status {wait}");
    }
    drop(parent);

    // The updater held this same lease until its process exited. Reacquire it
    // before mutating either exact recovery entry so a newly started command
    // and this worker cannot race journal consumption.
    let _lease = super::acquire_update_lease(&cleanup.public_executable)
        .context("locking post-exit self-update cleanup")?;

    #[cfg(test)]
    if let Some(sentinel) = std::env::var_os(FAIL_AFTER_PARENT_SENTINEL_ENV) {
        let sentinel = std::path::PathBuf::from(sentinel);
        let mut file = std::fs::File::create(&sentinel)
            .with_context(|| format!("creating cleanup failure sentinel {}", sentinel.display()))?;
        file.write_all(b"delete failure injected after parent exit")?;
        file.sync_all()?;
        anyhow::bail!("injected displaced-executable deletion failure after parent exit");
    }

    complete_after_revalidation(&cleanup)
}

pub(super) fn spawn(
    public_executable: &std::path::Path,
    published_token: &crate::fs_ops::FileToken,
    displaced_previous: &std::path::Path,
    previous_token: &crate::fs_ops::FileToken,
    prepared: PreparedCleanup,
) -> Result<()> {
    let published_executable_pin =
        crate::daemon::prepare_verified_background_spawn(public_executable, published_token)?;
    let PreparedCleanup {
        journal_path,
        journal_token,
    } = prepared;
    load_validated_cleanup(public_executable, &journal_path, Some(&journal_token))
        .context("revalidating prepared self-update cleanup before worker spawn")?;
    let ready_nonce = published_executable_pin.ready_nonce().to_string();
    let expected_marker = format!("{READY_PREFIX} {ready_nonce}");
    let journal_recovery_context = || {
        format!(
            "durable exact cleanup remains journaled at {} with token {}",
            journal_path.display(),
            journal_token
        )
    };

    let mut command = std::process::Command::new(public_executable);
    #[cfg(not(test))]
    command
        .arg("__cleanup-self-update")
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--displaced")
        .arg(displaced_previous)
        .arg("--expected-token")
        .arg(previous_token.to_string())
        .arg("--expected-executable-token")
        .arg(published_token.to_string())
        .arg("--journal")
        .arg(&journal_path)
        .arg("--expected-journal-token")
        .arg(journal_token.to_string())
        .arg("--ready-nonce")
        .arg(&ready_nonce);
    #[cfg(test)]
    command
        .arg(WORKER_TEST_NAME)
        .arg("--exact")
        .arg("--nocapture")
        .env(TEST_MODE_ENV, "1")
        .env(PARENT_PID_ENV, std::process::id().to_string())
        .env(DISPLACED_ENV, displaced_previous)
        .env(EXPECTED_TOKEN_ENV, previous_token.to_string())
        .env(EXECUTABLE_TOKEN_ENV, published_token.to_string())
        .env(JOURNAL_PATH_ENV, &journal_path)
        .env(JOURNAL_TOKEN_ENV, journal_token.to_string())
        .env(READY_NONCE_ENV, &ready_nonce);
    #[cfg(test)]
    if let Some(sentinel) = take_failure_sentinel() {
        command.env(FAIL_AFTER_PARENT_SENTINEL_ENV, sentinel);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    crate::daemon::isolate_background_child_from_terminal_interrupt(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| {
            format!(
                "starting exact self-update cleanup worker from {}",
                public_executable.display()
            )
        })
        .with_context(journal_recovery_context)?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let error = anyhow::anyhow!("self-update cleanup worker has no readiness channel");
            return Err(crate::daemon::terminate_background_child_on_error(
                &mut child, error,
            ))
            .with_context(journal_recovery_context);
        }
    };
    crate::daemon::await_expected_background_marker(
        &mut child,
        stdout,
        &expected_marker,
        READY_TIMEOUT,
        READY_TOTAL_MAX_BYTES,
        "self-update cleanup worker",
    )
    .with_context(journal_recovery_context)?;

    // Readiness proves this pinned image opened the exact parent handle and
    // verified both file tokens. The namespace pin can now be released;
    // dropping our process handle does not terminate the Windows worker.
    drop(published_executable_pin);
    drop(child);
    Ok(())
}

#[cfg(test)]
pub(super) fn run_from_test_env() -> Result<bool> {
    if std::env::var_os(TEST_MODE_ENV).is_none() {
        return Ok(false);
    }
    let parent_pid = std::env::var(PARENT_PID_ENV)
        .context("reading cleanup test parent PID")?
        .parse::<u32>()
        .context("parsing cleanup test parent PID")?;
    let displaced_previous = std::path::PathBuf::from(
        std::env::var_os(DISPLACED_ENV).context("reading cleanup test displaced path")?,
    );
    let expected_token =
        std::env::var(EXPECTED_TOKEN_ENV).context("reading cleanup test displaced token")?;
    let expected_executable_token =
        std::env::var(EXECUTABLE_TOKEN_ENV).context("reading cleanup test executable token")?;
    let ready_nonce =
        std::env::var(READY_NONCE_ENV).context("reading cleanup test readiness nonce")?;
    let journal_path = std::path::PathBuf::from(
        std::env::var_os(JOURNAL_PATH_ENV).context("reading cleanup test journal path")?,
    );
    let expected_journal_token =
        std::env::var(JOURNAL_TOKEN_ENV).context("reading cleanup test journal token")?;
    run(
        parent_pid,
        &displaced_previous,
        &expected_token,
        &expected_executable_token,
        &journal_path,
        &expected_journal_token,
        &ready_nonce,
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::os::windows::fs::OpenOptionsExt as _;

    #[test]
    fn journal_is_a_stable_executable_transaction_sibling() {
        let temp = tempfile::tempdir().expect("create stable journal fixture");
        let public = temp.path().join("public.exe");

        assert_eq!(
            super::journal_path(&public).expect("derive cleanup journal"),
            temp.path().join(".public.exe.self-update-cleanup-journal")
        );
    }

    #[test]
    fn public_executable_pin_blocks_a_to_b_to_a_spawn_race() {
        let temp = tempfile::tempdir().expect("create pin fixture");
        let public = temp.path().join("public.exe");
        let candidate = temp.path().join("candidate.exe");
        let held = temp.path().join("held-a.exe");
        std::fs::write(&public, b"verified image A").expect("write image A");
        std::fs::write(&candidate, b"unverified image B").expect("write image B");
        let expected = crate::fs_ops::token_for_path(&public).expect("bind image A");

        let pin = crate::daemon::prepare_verified_background_spawn(&public, &expected)
            .expect("pin verified image A");
        let blocked = std::fs::rename(&public, &held)
            .expect_err("the first A->B->A rename must be blocked while A is pinned");
        assert!(public.exists(), "the verified namespace occupant changed");
        assert!(!held.exists(), "the blocked race displaced image A");
        assert_eq!(
            crate::fs_ops::token_for_path(&public).expect("rebind pinned image A"),
            expected,
            "a failed rename must leave the verified image at the public path"
        );
        assert_ne!(blocked.raw_os_error(), Some(0));

        drop(pin);
        std::fs::rename(&public, &held).expect("displace A after releasing its pin");
        std::fs::rename(&candidate, &public).expect("publish B");
        std::fs::rename(&public, &candidate).expect("displace B");
        std::fs::rename(&held, &public).expect("restore A");
        assert_eq!(
            crate::fs_ops::token_for_path(&public).expect("bind restored image A"),
            expected,
            "the complete A->B->A sequence is possible only after readiness releases the pin"
        );
    }

    #[test]
    fn public_executable_pin_blocks_in_place_rewrite_spawn_restore_race() {
        let temp = tempfile::tempdir().expect("create in-place pin fixture");
        let public = temp.path().join("public.exe");
        std::fs::write(&public, b"verified image A").expect("write image A");
        let expected = crate::fs_ops::token_for_path(&public).expect("bind image A");

        let pin = crate::daemon::prepare_verified_background_spawn(&public, &expected)
            .expect("pin verified image A");
        let blocked = std::fs::write(&public, b"unverified image B")
            .expect_err("in-place A->B rewrite must be blocked through spawn readiness");
        assert_ne!(blocked.raw_os_error(), Some(0));
        assert_eq!(
            crate::fs_ops::token_for_path(&public).expect("rebind pinned image A"),
            expected,
            "the failed rewrite must leave the spawn image token unchanged"
        );

        drop(pin);
        std::fs::write(&public, b"unverified image B").expect("rewrite after releasing pin");
        assert_ne!(
            crate::fs_ops::token_for_path(&public).expect("bind image B"),
            expected
        );
        std::fs::write(&public, b"verified image A").expect("restore A after simulated spawn");
        assert_eq!(
            crate::fs_ops::token_for_path(&public).expect("bind restored image A"),
            expected,
            "the B->spawn->A illusion is possible only after readiness releases the pin"
        );
    }

    #[test]
    fn an_undeletable_journal_remains_exactly_retryable() {
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        let temp = tempfile::tempdir().expect("create cleanup retry fixture");
        let public = temp.path().join("public.exe");
        let backup = temp.path().join(".public.exe.self-update-backup");
        let displaced = temp
            .path()
            .join(".public.exe.self-update-displaced-00112233445566778899aabbccddeeff");
        std::fs::write(&public, b"published executable").expect("write public fixture");
        std::fs::write(&backup, b"independent previous backup").expect("write backup fixture");
        std::fs::write(&displaced, b"previous executable").expect("write displaced fixture");
        let public_token =
            crate::fs_ops::token_for_path(&public).expect("bind published executable");
        let previous_token =
            crate::fs_ops::token_for_path(&displaced).expect("bind displaced executable");
        let backup_token = crate::fs_ops::token_for_path(&backup).expect("bind backup executable");
        let (journal, journal_token) = super::create_journal(
            &public,
            &public_token,
            &backup,
            &backup_token,
            &displaced,
            &previous_token,
        )
        .expect("create exact cleanup journal");

        let pin = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&journal)
            .expect("pin journal against delete");
        let error = super::recover_pending(&public)
            .expect_err("a pinned journal cannot be consumed conclusively");
        assert!(
            format!("{error:#}").contains("cleanup journal"),
            "{error:#}"
        );
        assert!(
            !displaced.exists(),
            "the exact displaced executable was already removed"
        );
        assert!(
            !backup.exists(),
            "the exact fixed backup was already removed"
        );
        assert_eq!(
            crate::fs_ops::token_for_path(&journal).expect("rebind retained journal"),
            journal_token,
            "failed journal removal changed the retry authority"
        );

        drop(pin);
        assert!(
            super::recover_pending(&public).expect("retry exact cleanup"),
            "the retained journal was not retried"
        );
        assert!(!journal.exists(), "retry left the journal behind");
    }

    #[test]
    fn malformed_journal_is_preserved_without_deleting_the_displaced_image() {
        let temp = tempfile::tempdir().expect("create malformed journal fixture");
        let public = temp.path().join("public.exe");
        let displaced = temp
            .path()
            .join(".public.exe.self-update-displaced-00112233445566778899aabbccddeeff");
        std::fs::write(&public, b"published executable").expect("write public fixture");
        std::fs::write(&displaced, b"previous executable").expect("write displaced fixture");
        let journal = super::journal_path(&public).expect("derive journal path");
        std::fs::write(&journal, b"{not valid json").expect("write malformed journal");

        let error = super::recover_pending(&public)
            .expect_err("malformed cleanup authority must fail closed");
        assert!(format!("{error:#}").contains("parsing cleanup journal"));
        assert!(journal.exists(), "malformed journal was silently discarded");
        assert!(
            displaced.exists(),
            "malformed journal authorized a displaced-image deletion"
        );
    }
}
