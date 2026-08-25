use anyhow::{Context, Result};
use chrono::{Days, Local, NaiveDate};
use fs4::FileExt;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing_subscriber::fmt::MakeWriter;

const LOG_PREFIX: &str = "codex-switch-global-pace";
const MAX_LOG_AGE_DAYS: u64 = 3;
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
const APPLICATION_TRACING_TARGET: &str = env!("CARGO_CRATE_NAME");

pub(crate) fn application_filter(level: &str) -> String {
    format!("{APPLICATION_TRACING_TARGET}={level}")
}

/// How long retention may go unenforced, and how many bytes may be appended in
/// the meantime.
///
/// `tracing` calls `Write::write` once per record and the retention scan walks
/// the log directory, so running it per record made every debug-level log line
/// a directory walk. Retention only has to be approximately timely: whichever
/// of these two is reached first triggers the next scan, which bounds how far
/// the directory can drift past [`MAX_LOG_BYTES`] to one byte budget.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);
const MAINTENANCE_BYTE_BUDGET: u64 = 1024 * 1024;

#[derive(Clone)]
pub(crate) struct FileLogWriter {
    state: Arc<Mutex<LogState>>,
}

struct LogState {
    dir: PathBuf,
    /// Keeps every Windows ancestor pinned while daily files are reopened by
    /// path. On Unix this witnesses the owner/mode ancestry contract.
    directory_guard: crate::auth::PrivateDirectoryGuard,
    /// When retention was last enforced; `None` until the first record.
    last_maintenance: Option<Instant>,
    /// Bytes appended since that enforcement.
    bytes_since_maintenance: u64,
}

pub(crate) fn file_log_writer() -> Result<FileLogWriter> {
    let dir = crate::auth::app_home()?.join("logs");
    let directory_guard = create_private_log_dir(&dir)
        .with_context(|| format!("creating log directory {}", dir.display()))?;
    Ok(FileLogWriter {
        state: Arc::new(Mutex::new(LogState {
            dir,
            directory_guard,
            last_maintenance: None,
            bytes_since_maintenance: 0,
        })),
    })
}

fn create_private_log_dir(dir: &Path) -> Result<crate::auth::PrivateDirectoryGuard> {
    crate::auth::acquire_private_directory(dir)
}

#[cfg(unix)]
fn tighten_file_permissions(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
}

fn open_private_regular(path: &Path, options: &mut OpenOptions) -> io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "private log path is not a direct regular file: {}",
                path.display()
            ),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "private log file must be owned by the effective user (uid {effective_uid}): {} is owned by uid {}",
                    path.display(),
                    metadata.uid()
                ),
            ));
        }
        tighten_file_permissions(&file)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("private log path is a reparse point: {}", path.display()),
            ));
        }
        tighten_file_permissions(path)?;
    }
    Ok(file)
}

#[cfg(windows)]
fn tighten_file_permissions(path: &Path) -> io::Result<()> {
    crate::auth::harden_windows_private_file(path).map_err(|error| {
        io::Error::other(format!(
            "hardening private ACL on {}: {error:#}",
            path.display()
        ))
    })
}

impl<'a> MakeWriter<'a> for FileLogWriter {
    type Writer = LogFile;

    fn make_writer(&'a self) -> Self::Writer {
        LogFile {
            state: Arc::clone(&self.state),
        }
    }
}

pub(crate) struct LogFile {
    state: Arc<Mutex<LogState>>,
}

impl Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let retained = if buf.len() as u64 > MAX_LOG_BYTES {
            &buf[buf.len() - MAX_LOG_BYTES as usize..]
        } else {
            buf
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("log writer lock poisoned"))?;
        state.bytes_since_maintenance = state
            .bytes_since_maintenance
            .saturating_add(retained.len() as u64);
        let now = Instant::now();
        let run_maintenance =
            maintenance_due(state.last_maintenance, now, state.bytes_since_maintenance);
        append_log(
            &state.dir,
            &state.directory_guard,
            Local::now().date_naive(),
            retained,
            run_maintenance,
        )?;
        if run_maintenance {
            state.last_maintenance = Some(now);
            state.bytes_since_maintenance = 0;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Whether this record should also pay for a retention scan.
///
/// The first record of a process always does — nothing earlier can have done
/// it — and after that whichever of the byte budget or the interval arrives
/// first.
fn maintenance_due(last: Option<Instant>, now: Instant, bytes_since: u64) -> bool {
    let Some(last) = last else {
        return true;
    };
    bytes_since >= MAINTENANCE_BYTE_BUDGET || now.duration_since(last) >= MAINTENANCE_INTERVAL
}

fn append_log(
    dir: &Path,
    _directory_guard: &crate::auth::PrivateDirectoryGuard,
    today: NaiveDate,
    bytes: &[u8],
    run_maintenance: bool,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let mut lock_options = OpenOptions::new();
    lock_options.create(true).truncate(false).write(true);
    let lock_path = dir.join(".lock");
    let lock = open_private_regular(&lock_path, &mut lock_options)?;
    FileExt::lock(&lock)?;
    let result = (|| {
        if run_maintenance {
            run_log_maintenance(dir, today, bytes.len() as u64)?;
        }
        let mut log_options = OpenOptions::new();
        log_options.create(true).append(true);
        let current_log = log_path(dir, today);
        let mut file = open_private_regular(&current_log, &mut log_options)?;
        file.write_all(bytes)
    })();
    FileExt::unlock(&lock)?;
    result
}

/// Drop log files outside the retention window.
///
/// Size enforcement used to be nested at the end of this, which meant a single
/// append ran three directory scans: this one, the nested one, and the caller's.
/// The two passes are now siblings under [`run_log_maintenance`], so an append
/// that does maintenance costs two scans and one that does not costs none.
fn prune_expired_log_files(dir: &Path, today: NaiveDate) -> io::Result<()> {
    let oldest = today - Days::new(MAX_LOG_AGE_DAYS - 1);
    for (path, date, _) in log_files(dir)? {
        if date < oldest {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Age retention, then size retention accounting for the record about to be
/// written.
fn run_log_maintenance(dir: &Path, today: NaiveDate, incoming: u64) -> io::Result<()> {
    prune_expired_log_files(dir, today)?;
    enforce_log_size_limit(dir, today, incoming)
}

fn enforce_log_size_limit(dir: &Path, today: NaiveDate, incoming: u64) -> io::Result<()> {
    let current = log_path(dir, today);
    let mut files = log_files(dir)?;
    files.sort_by_key(|(_, date, _)| *date);
    let mut total = files.iter().map(|(_, _, size)| *size).sum::<u64>();

    for (path, _, size) in &files {
        if total.saturating_add(incoming) <= MAX_LOG_BYTES {
            return Ok(());
        }
        if *path != current {
            fs::remove_file(path)?;
            total = total.saturating_sub(*size);
        }
    }

    if total.saturating_add(incoming) > MAX_LOG_BYTES {
        let mut options = fs::OpenOptions::new();
        options.write(true);
        match open_private_regular(&current, &mut options) {
            Ok(file) => file.set_len(0)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn log_files(dir: &Path) -> io::Result<Vec<(PathBuf, NaiveDate, u64)>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(date) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(log_date)
        else {
            continue;
        };
        if entry.file_type()?.is_file() {
            files.push((path, date, entry.metadata()?.len()));
        }
    }
    Ok(files)
}

fn log_path(dir: &Path, date: NaiveDate) -> PathBuf {
    dir.join(format!("{LOG_PREFIX}.{date}.log"))
}

fn log_date(filename: &str) -> Option<NaiveDate> {
    filename
        .strip_prefix(&format!("{LOG_PREFIX}."))?
        .strip_suffix(".log")
        .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_filter_tracks_the_compiled_library_target() {
        let module_target = module_path!()
            .split("::")
            .next()
            .expect("module path contains the crate target");

        assert_eq!(APPLICATION_TRACING_TARGET, module_target);
        assert_eq!(
            application_filter("debug"),
            format!("{module_target}=debug")
        );
        tracing_subscriber::EnvFilter::try_new(application_filter("debug"))
            .expect("derived application filter must be valid");
    }

    #[cfg(windows)]
    fn has_protected_dacl(path: &Path) -> bool {
        use std::os::windows::ffi::OsStrExt;
        use std::ptr::null_mut;

        use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
        use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR,
            SE_DACL_PROTECTED,
        };

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `wide` is NUL-terminated and `descriptor` points to writable
        // storage for the LocalAlloc-owned security descriptor returned by the
        // API. The owner/group/DACL/SACL outputs are intentionally omitted.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(
            status,
            ERROR_SUCCESS,
            "reading DACL for {} failed: {}",
            path.display(),
            io::Error::from_raw_os_error(status as i32)
        );

        let mut control = 0;
        let mut revision = 0;
        // SAFETY: a successful GetNamedSecurityInfoW initialized `descriptor`;
        // the control outputs point to writable local variables.
        let control_ok =
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
        // SAFETY: GetNamedSecurityInfoW allocates the descriptor with LocalAlloc
        // and this is its sole release.
        unsafe {
            LocalFree(descriptor);
        }
        assert_ne!(
            control_ok,
            0,
            "reading security descriptor control for {} failed: {}",
            path.display(),
            io::Error::last_os_error()
        );
        control & SE_DACL_PROTECTED != 0
    }

    fn create_log(dir: &Path, day: NaiveDate, bytes: u64) {
        let file = fs::File::create(log_path(dir, day)).unwrap();
        file.set_len(bytes).unwrap();
    }

    #[test]
    fn retains_only_the_latest_three_calendar_days() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        for day in 8..=12 {
            create_log(
                dir.path(),
                NaiveDate::from_ymd_opt(2026, 7, day).unwrap(),
                1,
            );
        }

        prune_expired_log_files(dir.path(), today).unwrap();

        assert!(!log_path(dir.path(), NaiveDate::from_ymd_opt(2026, 7, 8).unwrap()).exists());
        assert!(!log_path(dir.path(), NaiveDate::from_ymd_opt(2026, 7, 9).unwrap()).exists());
        assert!(log_path(dir.path(), NaiveDate::from_ymd_opt(2026, 7, 10).unwrap()).exists());
        assert!(log_path(dir.path(), today).exists());
    }

    #[test]
    fn removes_oldest_logs_to_keep_total_at_ten_mebibytes() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        for day in 10..=12 {
            create_log(
                dir.path(),
                NaiveDate::from_ymd_opt(2026, 7, day).unwrap(),
                5 * 1024 * 1024,
            );
        }

        run_log_maintenance(dir.path(), today, 0).unwrap();

        assert!(!log_path(dir.path(), NaiveDate::from_ymd_opt(2026, 7, 10).unwrap()).exists());
        assert!(log_path(dir.path(), NaiveDate::from_ymd_opt(2026, 7, 11).unwrap()).exists());
        assert!(log_path(dir.path(), today).exists());
    }

    #[test]
    fn appending_never_exceeds_ten_mebibytes() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        create_log(dir.path(), today, MAX_LOG_BYTES);
        let directory_guard = create_private_log_dir(dir.path()).unwrap();

        append_log(dir.path(), &directory_guard, today, b"next event", true).unwrap();

        assert!(fs::metadata(log_path(dir.path(), today)).unwrap().len() <= MAX_LOG_BYTES);
    }

    // ── retention runs on a budget, not on every record ────────
    //
    // `tracing` calls `write` once per log record, and the retention scan is
    // several `read_dir` passes over the log directory. At debug level that
    // turned every single log line into a directory walk.

    /// The scan has to happen on the first write of a process — there is no
    /// earlier one to have done it — and then only when a budget is spent.
    #[test]
    fn the_first_write_of_a_process_always_runs_maintenance() {
        assert!(maintenance_due(None, Instant::now(), 0));
    }

    #[test]
    fn an_ordinary_record_shortly_after_a_scan_does_not_rescan() {
        let now = Instant::now();
        assert!(
            !maintenance_due(Some(now), now, 64),
            "a handful of bytes moments after a scan must not trigger another one"
        );
    }

    #[test]
    fn maintenance_runs_again_once_the_byte_budget_is_spent() {
        let now = Instant::now();
        assert!(maintenance_due(Some(now), now, MAINTENANCE_BYTE_BUDGET));
    }

    #[test]
    fn maintenance_runs_again_once_the_interval_has_passed() {
        let now = Instant::now();
        let last = now.checked_sub(MAINTENANCE_INTERVAL).unwrap();
        assert!(maintenance_due(Some(last), now, 1));
    }

    /// The wiring, not just the decision: a write that is not due must leave
    /// out-of-retention files alone, and a due one must still collect them.
    #[test]
    fn a_skipped_maintenance_write_does_not_scan_the_directory() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        let expired = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        create_log(dir.path(), expired, 1);
        let directory_guard = create_private_log_dir(dir.path()).unwrap();

        append_log(dir.path(), &directory_guard, today, b"skipped\n", false).unwrap();
        assert!(
            log_path(dir.path(), expired).exists(),
            "a write that is not due for maintenance must not walk the log directory"
        );

        append_log(dir.path(), &directory_guard, today, b"due\n", true).unwrap();
        assert!(
            !log_path(dir.path(), expired).exists(),
            "a write that is due must still apply retention"
        );
    }

    #[cfg(unix)]
    #[test]
    fn append_log_tightens_directory_lock_and_log_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        let lock_path = dir.path().join(".lock");
        let current_log = log_path(dir.path(), today);
        fs::File::create(&lock_path).unwrap();
        fs::File::create(&current_log).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o666)).unwrap();
        fs::set_permissions(&current_log, fs::Permissions::from_mode(0o666)).unwrap();
        let directory_guard = create_private_log_dir(dir.path()).unwrap();

        append_log(dir.path(), &directory_guard, today, b"private event", true).unwrap();

        assert_eq!(
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(current_log).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn append_log_never_follows_preexisting_file_links() {
        use std::os::unix::fs::symlink;

        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let dir = root.path().join("logs");
        fs::create_dir(&dir).unwrap();
        let target = root.path().join("unrelated");
        fs::write(&target, b"unchanged").unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        let directory_guard = create_private_log_dir(&dir).unwrap();

        symlink(&target, dir.join(".lock")).unwrap();
        append_log(&dir, &directory_guard, today, b"private event", false)
            .expect_err("the lock path must never follow a symbolic link");
        fs::remove_file(dir.join(".lock")).unwrap();

        fs::File::create(dir.join(".lock")).unwrap();
        symlink(&target, log_path(&dir, today)).unwrap();
        append_log(&dir, &directory_guard, today, b"private event", false)
            .expect_err("the daily log path must never follow a symbolic link");

        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
    }

    #[cfg(windows)]
    #[test]
    fn append_log_hardens_directory_lock_and_log_acls() {
        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let dir = root.path().join("logs");
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        let lock_path = dir.join(".lock");
        let current_log = log_path(&dir, today);

        let directory_guard = create_private_log_dir(&dir).unwrap();
        fs::File::create(&lock_path).unwrap();
        fs::File::create(&current_log).unwrap();
        append_log(&dir, &directory_guard, today, b"private event", true).unwrap();

        assert!(has_protected_dacl(&dir));
        assert!(has_protected_dacl(&lock_path));
        assert!(has_protected_dacl(&current_log));
    }

    #[cfg(windows)]
    #[test]
    fn append_log_never_follows_preexisting_file_reparse_points() {
        use std::os::windows::fs::symlink_file;

        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let dir = root.path().join("logs");
        fs::create_dir(&dir).unwrap();
        let target = root.path().join("unrelated");
        fs::write(&target, b"unchanged").unwrap();
        if let Err(error) = symlink_file(&target, dir.join(".lock")) {
            if error.kind() == io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("creating file symlink for private log test: {error}");
        }
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        let directory_guard = create_private_log_dir(&dir).unwrap();

        append_log(&dir, &directory_guard, today, b"private event", false)
            .expect_err("the lock path must never follow a reparse point");

        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
    }
}
