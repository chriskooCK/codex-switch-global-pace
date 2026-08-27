use anyhow::{Context, Result};
use chrono::{Days, Local, NaiveDate};
use fs4::FileExt;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
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
    shared: Arc<SharedLogState>,
}

struct SharedLogState {
    directory: LogDirectory,
    state: Mutex<FileLogState>,
    initialized: Condvar,
    #[cfg(test)]
    initialization_attempts: std::sync::atomic::AtomicUsize,
}

enum LogDirectory {
    AppHome,
    #[cfg(test)]
    Exact(PathBuf),
}

enum FileLogState {
    /// File logging is installed in the subscriber, but secure path discovery
    /// has deliberately not started yet. This is the bare-TUI pre-frame state:
    /// records still reach stderr and their file copies are retained until the
    /// post-frame initializer runs. The pending copy is capped by the same
    /// byte budget as the complete log set, so delayed initialization cannot
    /// grow memory without bound.
    Deferred {
        pending: Vec<u8>,
    },
    /// Secure path discovery is allowed, but no enabled record has requested
    /// it yet. Ordinary CLI writers begin here; a no-log command therefore
    /// performs no log filesystem or ACL work.
    Uninitialized,
    Initializing,
    Ready(LogState),
    /// The caller that performed initialization receives the error directly.
    /// A long-lived owner can also take the stored detail exactly once, which
    /// keeps a late first-write failure observable after tracing's independent
    /// stderr branch has run. Later file writes become clean no-ops.
    Disabled {
        error: String,
        reported: bool,
    },
}

struct LogState {
    dir: PathBuf,
    /// Keeps every Windows ancestor pinned for the lifetime of the cached
    /// handles. On Unix this witnesses the owner/mode ancestry contract.
    _directory_guard: crate::auth::PrivateDirectoryGuard,
    /// One writer-lifetime handle retains the cross-process serialization
    /// inode/file identity and avoids reopening and rehardening `.lock` for
    /// every tracing record.
    lock_file: fs::File,
    /// Reused for every append on the same calendar day. It is dropped before
    /// rollover maintenance so Windows retention can delete the prior file.
    daily_file: Option<DailyLogFile>,
    /// When retention was last enforced; `None` until the first record.
    last_maintenance: Option<Instant>,
    /// Bytes appended since that enforcement.
    bytes_since_maintenance: u64,
    #[cfg(test)]
    daily_file_opens: usize,
}

struct DailyLogFile {
    date: NaiveDate,
    file: fs::File,
}

pub(crate) fn file_log_writer() -> FileLogWriter {
    FileLogWriter::new(LogDirectory::AppHome, FileLogState::Uninitialized)
}

/// File writer that defers initialization until the bare TUI's first frame.
pub(crate) fn deferred_file_log_writer() -> FileLogWriter {
    FileLogWriter::deferred(LogDirectory::AppHome)
}

impl FileLogWriter {
    fn new(directory: LogDirectory, state: FileLogState) -> Self {
        Self {
            shared: Arc::new(SharedLogState {
                directory,
                state: Mutex::new(state),
                initialized: Condvar::new(),
                #[cfg(test)]
                initialization_attempts: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    fn deferred(directory: LogDirectory) -> Self {
        Self::new(
            directory,
            FileLogState::Deferred {
                pending: Vec::new(),
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn deferred_for_directory(directory: PathBuf) -> Self {
        Self::deferred(LogDirectory::Exact(directory))
    }

    #[cfg(test)]
    pub(crate) fn lazy_for_directory(directory: PathBuf) -> Self {
        Self::new(LogDirectory::Exact(directory), FileLogState::Uninitialized)
    }

    /// End the bare-TUI pre-frame deferral after the first frame is visible.
    /// If an enabled record arrived, initialize once and persist the bounded
    /// pending copy. Otherwise merely arm first-write initialization, keeping a
    /// no-log TUI free of log-directory and ACL work.
    pub(crate) fn finish_deferred_initialization(&self) -> Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("log writer lock poisoned"))?;
        loop {
            match &mut *state {
                FileLogState::Deferred { pending } => {
                    if pending.is_empty() {
                        *state = FileLogState::Uninitialized;
                        return Ok(());
                    }
                    let pending = std::mem::take(pending);
                    *state = FileLogState::Initializing;
                    drop(state);
                    return complete_initialization(&self.shared, pending);
                }
                FileLogState::Initializing => {
                    state = self
                        .shared
                        .initialized
                        .wait(state)
                        .map_err(|_| anyhow::anyhow!("log writer lock poisoned"))?;
                }
                FileLogState::Uninitialized => return Ok(()),
                FileLogState::Ready(_) => return Ok(()),
                FileLogState::Disabled { error, .. } => anyhow::bail!(error.clone()),
            }
        }
    }

    /// Establish the complete file-log readiness required by a long-lived
    /// process before it advertises itself as running. Unlike ordinary lazy
    /// writers, this validates the directory, lock file, and current daily log
    /// without emitting an artificial record or running retention.
    pub(crate) fn ensure_initialized(&self) -> Result<()> {
        loop {
            let mut state = self
                .shared
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("log writer lock poisoned"))?;
            let pending = match &mut *state {
                FileLogState::Deferred { pending } => {
                    let pending = std::mem::take(pending);
                    *state = FileLogState::Initializing;
                    pending
                }
                FileLogState::Uninitialized => {
                    *state = FileLogState::Initializing;
                    Vec::new()
                }
                FileLogState::Initializing => {
                    state = self
                        .shared
                        .initialized
                        .wait(state)
                        .map_err(|_| anyhow::anyhow!("log writer lock poisoned"))?;
                    drop(state);
                    continue;
                }
                FileLogState::Ready(ready) => {
                    let readiness = append_log(ready, Local::now().date_naive(), &[], false)
                        .context("opening the current daily log for write readiness");
                    if let Err(error) = readiness {
                        let detail = format!("{error:#}");
                        *state = FileLogState::Disabled {
                            error: detail,
                            reported: false,
                        };
                        self.shared.initialized.notify_all();
                        return Err(error);
                    }
                    return Ok(());
                }
                FileLogState::Disabled { error, .. } => anyhow::bail!(error.clone()),
            };
            drop(state);
            complete_initialization(&self.shared, pending)?;
        }
    }

    /// Return a failed initialization detail to one long-lived observer. The
    /// synchronous initializer still receives its own `Err`; this one-shot
    /// view exists for failures raised later by tracing's `Write` boundary.
    pub(crate) fn take_initialization_error(&self) -> Result<Option<String>> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("log writer lock poisoned"))?;
        match &mut *state {
            FileLogState::Disabled { error, reported } if !*reported => {
                *reported = true;
                Ok(Some(error.clone()))
            }
            _ => Ok(None),
        }
    }

    /// A failed blocking task can stop outside the normal `Result` path (for
    /// example, a panic before it finishes initialization). The tracked TUI
    /// owner uses this to close the file branch instead of leaving it in a
    /// queueing state forever.
    pub(crate) fn disable_initialization_after_task_failure(&self, error: String) {
        let Ok(mut state) = self.shared.state.lock() else {
            return;
        };
        if matches!(
            &*state,
            FileLogState::Deferred { .. }
                | FileLogState::Uninitialized
                | FileLogState::Initializing
        ) {
            *state = FileLogState::Disabled {
                error,
                reported: false,
            };
            self.shared.initialized.notify_all();
        }
    }

    #[cfg(test)]
    fn initialization_attempts(&self) -> usize {
        self.shared
            .initialization_attempts
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

fn initialize_directory(
    shared: &SharedLogState,
) -> Result<(PathBuf, crate::auth::PrivateDirectoryGuard)> {
    let dir = match &shared.directory {
        LogDirectory::AppHome => crate::auth::app_home()?.join("logs"),
        #[cfg(test)]
        LogDirectory::Exact(path) => path.clone(),
    };
    let guard = create_private_log_dir(&dir)
        .with_context(|| format!("creating log directory {}", dir.display()))?;
    Ok((dir, guard))
}

impl LogState {
    fn open(dir: PathBuf, directory_guard: crate::auth::PrivateDirectoryGuard) -> Result<Self> {
        let mut lock_options = OpenOptions::new();
        lock_options.create(true).truncate(false).write(true);
        let lock_path = dir.join(".lock");
        let lock_file =
            open_private_regular(&lock_path, &mut lock_options, PrivateLogFileRole::Lock)
                .with_context(|| format!("opening private log lock {}", lock_path.display()))?;
        Ok(Self {
            dir,
            _directory_guard: directory_guard,
            lock_file,
            daily_file: None,
            last_maintenance: None,
            bytes_since_maintenance: 0,
            #[cfg(test)]
            daily_file_opens: 0,
        })
    }
}

/// Complete an initialization whose caller already changed the shared state
/// to `Initializing`. This is the sole filesystem/ACL initialization path.
fn complete_initialization(shared: &SharedLogState, pending: Vec<u8>) -> Result<()> {
    #[cfg(test)]
    shared
        .initialization_attempts
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let initialized = initialize_directory(shared)
        .and_then(|(dir, directory_guard)| LogState::open(dir, directory_guard));
    let mut state = shared
        .state
        .lock()
        .map_err(|_| anyhow::anyhow!("log writer lock poisoned"))?;
    debug_assert!(matches!(&*state, FileLogState::Initializing));
    let mut ready = match initialized {
        Ok(initialized) => initialized,
        Err(error) => {
            let detail = format!("{error:#}");
            *state = FileLogState::Disabled {
                error: detail,
                reported: false,
            };
            shared.initialized.notify_all();
            return Err(error);
        }
    };
    if !pending.is_empty()
        && let Err(error) = write_log_record(&mut ready, &pending)
    {
        let error = anyhow::Error::new(error)
            .context("writing records buffered before file-log initialization");
        let detail = format!("{error:#}");
        *state = FileLogState::Disabled {
            error: detail,
            reported: false,
        };
        shared.initialized.notify_all();
        return Err(error);
    }
    *state = FileLogState::Ready(ready);
    shared.initialized.notify_all();
    Ok(())
}

fn retain_bounded_pending(pending: &mut Vec<u8>, record: &[u8]) {
    let limit = MAX_LOG_BYTES as usize;
    if record.len() >= limit {
        pending.clear();
        pending.extend_from_slice(&record[record.len() - limit..]);
        return;
    }
    let overflow = pending
        .len()
        .saturating_add(record.len())
        .saturating_sub(limit);
    if overflow != 0 {
        pending.drain(..overflow);
    }
    pending.extend_from_slice(record);
}

fn create_private_log_dir(dir: &Path) -> Result<crate::auth::PrivateDirectoryGuard> {
    crate::auth::acquire_private_directory(dir)
}

#[cfg(unix)]
fn tighten_file_permissions(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[derive(Clone, Copy)]
enum PrivateLogFileRole {
    /// The lock path is the shared serialization identity, so Windows handles
    /// must prevent it from being deleted or replaced while a writer exists.
    Lock,
    /// A peer may retain yesterday's cached handle after this process rolls to
    /// a new date, so Windows retention must be allowed to delete daily logs.
    DailyLog,
}

fn open_private_regular(
    path: &Path,
    options: &mut OpenOptions,
    role: PrivateLogFileRole,
) -> io::Result<fs::File> {
    #[cfg(not(windows))]
    let _ = role;
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
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let delete_sharing = match role {
            PrivateLogFileRole::Lock => 0,
            PrivateLogFileRole::DailyLog => FILE_SHARE_DELETE,
        };
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | delete_sharing)
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
            shared: Arc::clone(&self.shared),
        }
    }
}

pub(crate) struct LogFile {
    shared: Arc<SharedLogState>,
}

impl Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let retained = if buf.len() as u64 > MAX_LOG_BYTES {
            &buf[buf.len() - MAX_LOG_BYTES as usize..]
        } else {
            buf
        };
        loop {
            let mut state = self
                .shared
                .state
                .lock()
                .map_err(|_| io::Error::other("log writer lock poisoned"))?;
            match &mut *state {
                FileLogState::Deferred { pending } => {
                    retain_bounded_pending(pending, retained);
                    return Ok(buf.len());
                }
                FileLogState::Uninitialized => {
                    *state = FileLogState::Initializing;
                    drop(state);
                    complete_initialization(&self.shared, Vec::new())
                        .map_err(|error| io::Error::other(format!("{error:#}")))?;
                }
                FileLogState::Initializing => {
                    state = self
                        .shared
                        .initialized
                        .wait(state)
                        .map_err(|_| io::Error::other("log writer lock poisoned"))?;
                    drop(state);
                }
                FileLogState::Ready(state) => {
                    write_log_record(state, retained)?;
                    return Ok(buf.len());
                }
                FileLogState::Disabled { .. } => return Ok(buf.len()),
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_log_record(state: &mut LogState, bytes: &[u8]) -> io::Result<()> {
    state.bytes_since_maintenance = state
        .bytes_since_maintenance
        .saturating_add(bytes.len() as u64);
    let now = Instant::now();
    let run_maintenance =
        maintenance_due(state.last_maintenance, now, state.bytes_since_maintenance);
    append_log(state, Local::now().date_naive(), bytes, run_maintenance)?;
    if run_maintenance {
        state.last_maintenance = Some(now);
        state.bytes_since_maintenance = 0;
    }
    Ok(())
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
    state: &mut LogState,
    today: NaiveDate,
    bytes: &[u8],
    run_maintenance: bool,
) -> io::Result<()> {
    FileExt::lock(&state.lock_file)?;
    let result = (|| {
        if state
            .daily_file
            .as_ref()
            .is_some_and(|daily| daily.date != today)
        {
            state.daily_file = None;
        }
        if run_maintenance {
            run_log_maintenance(&state.dir, today, bytes.len() as u64)?;
        }
        if state.daily_file.is_none() {
            let mut log_options = OpenOptions::new();
            log_options.create(true).append(true);
            let current_log = log_path(&state.dir, today);
            let file =
                open_private_regular(&current_log, &mut log_options, PrivateLogFileRole::DailyLog)?;
            state.daily_file = Some(DailyLogFile { date: today, file });
            #[cfg(test)]
            {
                state.daily_file_opens += 1;
            }
        }
        let daily = state
            .daily_file
            .as_mut()
            .expect("daily log handle is initialized before append");
        daily.file.write_all(bytes)
    })();
    let unlock_result = FileExt::unlock(&state.lock_file);
    result.and(unlock_result)
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
        match open_private_regular(&current, &mut options, PrivateLogFileRole::DailyLog) {
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

    #[test]
    fn deferred_writer_preserves_pre_init_and_concurrent_records_with_one_initialization() {
        use std::sync::Barrier;

        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let dir = root.path().join("logs");
        let writer = FileLogWriter::deferred_for_directory(dir.clone());
        let mut before_init = writer.make_writer();
        before_init.write_all(b"before init\n").unwrap();
        assert!(
            !dir.exists(),
            "a pre-frame record must not initialize the log directory"
        );

        let barrier = Arc::new(Barrier::new(3));
        let first_writer = writer.clone();
        let first_barrier = Arc::clone(&barrier);
        let first = std::thread::spawn(move || {
            first_barrier.wait();
            first_writer.finish_deferred_initialization()
        });
        let second_writer = writer.clone();
        let second_barrier = Arc::clone(&barrier);
        let second = std::thread::spawn(move || {
            second_barrier.wait();
            second_writer.finish_deferred_initialization()
        });
        barrier.wait();
        let mut concurrent = writer.make_writer();
        concurrent.write_all(b"concurrent init\n").unwrap();

        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
        assert_eq!(writer.initialization_attempts(), 1);
        let contents = fs::read(log_path(&dir, Local::now().date_naive())).unwrap();
        assert!(
            contents
                .windows(b"before init\n".len())
                .any(|window| window == b"before init\n")
        );
        assert!(
            contents
                .windows(b"concurrent init\n".len())
                .any(|window| window == b"concurrent init\n")
        );
    }

    #[test]
    fn lazy_writer_construction_and_no_log_path_do_not_touch_the_directory() {
        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let dir = root.path().join("logs");
        let writer = FileLogWriter::lazy_for_directory(dir.clone());

        assert_eq!(writer.initialization_attempts(), 0);
        assert!(!dir.exists());
        drop(writer);
        assert!(!dir.exists());
    }

    #[test]
    fn explicit_readiness_initializes_once_and_opens_the_current_daily_log() {
        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let dir = root.path().join("logs");
        let writer = FileLogWriter::lazy_for_directory(dir.clone());

        writer.ensure_initialized().unwrap();
        writer.ensure_initialized().unwrap();

        assert_eq!(writer.initialization_attempts(), 1);
        assert!(dir.join(".lock").is_file());
        assert!(log_path(&dir, Local::now().date_naive()).is_file());
        let state = writer.shared.state.lock().unwrap();
        let FileLogState::Ready(ready) = &*state else {
            panic!("successful readiness must retain a ready log state")
        };
        assert_eq!(ready.daily_file_opens, 1);
    }

    #[test]
    fn explicit_readiness_rejects_an_invalid_daily_log_before_success() {
        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let dir = root.path().join("logs");
        fs::create_dir(&dir).unwrap();
        fs::create_dir(log_path(&dir, Local::now().date_naive())).unwrap();
        let writer = FileLogWriter::lazy_for_directory(dir);

        let error = writer
            .ensure_initialized()
            .expect_err("a directory cannot be opened as the current daily log");

        assert!(format!("{error:#}").contains("opening the current daily log for write readiness"));
        let reported = writer
            .take_initialization_error()
            .unwrap()
            .expect("the readiness owner can report the stored failure");
        assert!(reported.contains("opening the current daily log for write readiness"));
        assert_eq!(writer.take_initialization_error().unwrap(), None);
    }

    #[test]
    fn failed_initializer_task_is_available_to_one_status_observer() {
        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let writer = FileLogWriter::deferred_for_directory(root.path().join("logs"));

        writer.disable_initialization_after_task_failure("worker panicked".to_string());

        assert_eq!(
            writer.take_initialization_error().unwrap().as_deref(),
            Some("worker panicked")
        );
        assert_eq!(writer.take_initialization_error().unwrap(), None);
        assert_eq!(
            writer.make_writer().write(b"stderr survives\n").unwrap(),
            16
        );
    }

    #[test]
    fn post_frame_without_a_record_only_arms_first_write_initialization() {
        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let dir = root.path().join("logs");
        let writer = FileLogWriter::deferred_for_directory(dir.clone());

        writer.finish_deferred_initialization().unwrap();
        assert_eq!(writer.initialization_attempts(), 0);
        assert!(!dir.exists());

        writer.make_writer().write_all(b"after frame\n").unwrap();
        assert_eq!(writer.initialization_attempts(), 1);
        assert!(dir.exists());
    }

    #[test]
    fn concurrent_first_writes_share_exactly_one_lazy_initialization() {
        use std::sync::Barrier;

        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let dir = root.path().join("logs");
        let writer = FileLogWriter::lazy_for_directory(dir.clone());
        let barrier = Arc::new(Barrier::new(3));
        let first_writer = writer.clone();
        let first_barrier = Arc::clone(&barrier);
        let first = std::thread::spawn(move || {
            first_barrier.wait();
            first_writer.make_writer().write_all(b"first\n")
        });
        let second_writer = writer.clone();
        let second_barrier = Arc::clone(&barrier);
        let second = std::thread::spawn(move || {
            second_barrier.wait();
            second_writer.make_writer().write_all(b"second\n")
        });

        barrier.wait();
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();

        assert_eq!(writer.initialization_attempts(), 1);
        let contents = fs::read(log_path(&dir, Local::now().date_naive())).unwrap();
        assert!(
            contents
                .windows(b"first\n".len())
                .any(|part| part == b"first\n")
        );
        assert!(
            contents
                .windows(b"second\n".len())
                .any(|part| part == b"second\n")
        );
    }

    #[test]
    fn failed_first_write_surfaces_error_and_disables_only_the_file_branch_once() {
        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let occupied = root.path().join("occupied");
        fs::write(&occupied, b"not a directory").unwrap();
        let writer = FileLogWriter::lazy_for_directory(occupied);

        let first_error = writer
            .make_writer()
            .write_all(b"first file record\n")
            .expect_err("a regular file cannot become the secure log directory");
        assert!(first_error.to_string().contains("creating log directory"));

        let mut disabled = writer.make_writer();
        assert_eq!(disabled.write(b"stderr remains independent\n").unwrap(), 27);
        assert_eq!(writer.initialization_attempts(), 1);

        let reported = writer
            .take_initialization_error()
            .unwrap()
            .expect("a long-lived owner must observe the late first-write failure");
        assert!(reported.contains("creating log directory"));
        assert_eq!(writer.take_initialization_error().unwrap(), None);
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

    fn open_test_log_state(dir: &Path) -> LogState {
        let directory_guard = create_private_log_dir(dir).unwrap();
        LogState::open(dir.to_path_buf(), directory_guard).unwrap()
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
    fn same_day_appends_reuse_one_validated_log_handle() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        let mut state = open_test_log_state(dir.path());

        append_log(&mut state, today, b"first\n", true).unwrap();
        append_log(&mut state, today, b"second\n", false).unwrap();

        assert_eq!(state.daily_file_opens, 1);
        assert_eq!(
            fs::read(log_path(dir.path(), today)).unwrap(),
            b"first\nsecond\n"
        );
    }

    #[test]
    fn date_rollover_replaces_the_handle_before_retention() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let first_day = NaiveDate::from_ymd_opt(2026, 7, 9).unwrap();
        let next_day = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        let mut state = open_test_log_state(dir.path());

        append_log(&mut state, first_day, b"first day\n", false).unwrap();
        assert_eq!(state.daily_file_opens, 1);
        append_log(&mut state, next_day, b"next day\n", true).unwrap();

        assert_eq!(state.daily_file_opens, 2);
        assert!(
            !log_path(dir.path(), first_day).exists(),
            "rollover must release the prior handle before pruning its file"
        );
        assert_eq!(
            fs::read(log_path(dir.path(), next_day)).unwrap(),
            b"next day\n"
        );
    }

    #[cfg(windows)]
    #[test]
    fn peer_cached_prior_day_handle_does_not_block_rollover_retention() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let first_day = NaiveDate::from_ymd_opt(2026, 7, 9).unwrap();
        let next_day = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        let mut advancing = open_test_log_state(dir.path());
        let mut idle_peer = open_test_log_state(dir.path());

        append_log(&mut advancing, first_day, b"advancing\n", false).unwrap();
        append_log(&mut idle_peer, first_day, b"idle peer\n", false).unwrap();
        append_log(&mut advancing, next_day, b"next day\n", true).unwrap();

        assert_eq!(
            idle_peer.daily_file.as_ref().map(|daily| daily.date),
            Some(first_day),
            "the peer must still own its prior-day handle during rollover"
        );
        assert_eq!(
            fs::read(log_path(dir.path(), next_day)).unwrap(),
            b"next day\n"
        );
        drop(idle_peer);
        assert!(
            !log_path(dir.path(), first_day).exists(),
            "the retained prior-day file must disappear when its final peer handle closes"
        );
    }

    #[test]
    fn size_maintenance_truncates_through_a_cached_append_handle() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        create_log(dir.path(), today, MAX_LOG_BYTES);
        let mut state = open_test_log_state(dir.path());

        append_log(&mut state, today, b"over budget", false).unwrap();
        assert_eq!(state.daily_file_opens, 1);
        append_log(&mut state, today, b"next event", true).unwrap();

        assert_eq!(state.daily_file_opens, 1);
        assert!(fs::metadata(log_path(dir.path(), today)).unwrap().len() <= MAX_LOG_BYTES);
    }

    #[test]
    fn independent_writers_serialize_records_with_cached_lock_handles() {
        use std::sync::Barrier;

        const CONCURRENT_WRITES: usize = 64;
        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let dir = root.path().join("logs");
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        let mut left = open_test_log_state(&dir);
        let mut right = open_test_log_state(&dir);

        append_log(&mut left, today, b"left\n", false).unwrap();
        append_log(&mut right, today, b"right\n", false).unwrap();

        let barrier = Arc::new(Barrier::new(3));
        let left_barrier = Arc::clone(&barrier);
        let left_thread = std::thread::spawn(move || {
            left_barrier.wait();
            for _ in 0..CONCURRENT_WRITES {
                append_log(&mut left, today, b"left\n", false).unwrap();
            }
            left.daily_file_opens
        });
        let right_barrier = Arc::clone(&barrier);
        let right_thread = std::thread::spawn(move || {
            right_barrier.wait();
            for _ in 0..CONCURRENT_WRITES {
                append_log(&mut right, today, b"right\n", false).unwrap();
            }
            right.daily_file_opens
        });
        barrier.wait();
        assert_eq!(left_thread.join().unwrap(), 1);
        assert_eq!(right_thread.join().unwrap(), 1);

        let contents = fs::read_to_string(log_path(&dir, today)).unwrap();
        assert_eq!(
            contents.lines().filter(|line| *line == "left").count(),
            CONCURRENT_WRITES + 1
        );
        assert_eq!(
            contents.lines().filter(|line| *line == "right").count(),
            CONCURRENT_WRITES + 1
        );
        assert_eq!(contents.lines().count(), 2 * (CONCURRENT_WRITES + 1));
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
        let mut state = open_test_log_state(dir.path());

        append_log(&mut state, today, b"skipped\n", false).unwrap();
        assert!(
            log_path(dir.path(), expired).exists(),
            "a write that is not due for maintenance must not walk the log directory"
        );

        append_log(&mut state, today, b"due\n", true).unwrap();
        assert!(
            !log_path(dir.path(), expired).exists(),
            "a write that is due must still apply retention"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cached_log_state_tightens_directory_lock_and_log_permissions() {
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
        let mut state = LogState::open(dir.path().to_path_buf(), directory_guard).unwrap();

        append_log(&mut state, today, b"private event", true).unwrap();

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
    fn cached_log_state_never_follows_preexisting_file_links() {
        use std::os::unix::fs::symlink;

        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let dir = root.path().join("logs");
        fs::create_dir(&dir).unwrap();
        let target = root.path().join("unrelated");
        fs::write(&target, b"unchanged").unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        let directory_guard = create_private_log_dir(&dir).unwrap();

        symlink(&target, dir.join(".lock")).unwrap();
        match LogState::open(dir.clone(), directory_guard) {
            Ok(_) => panic!("the lock path must never follow a symbolic link"),
            Err(error) => assert!(error.to_string().contains("private log lock")),
        }
        fs::remove_file(dir.join(".lock")).unwrap();

        let directory_guard = create_private_log_dir(&dir).unwrap();
        let mut state = LogState::open(dir.clone(), directory_guard).unwrap();
        symlink(&target, log_path(&dir, today)).unwrap();
        append_log(&mut state, today, b"private event", false)
            .expect_err("the daily log path must never follow a symbolic link");

        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
    }

    #[cfg(windows)]
    #[test]
    fn cached_log_state_hardens_directory_lock_and_log_acls() {
        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let dir = root.path().join("logs");
        let today = NaiveDate::from_ymd_opt(2026, 7, 12).unwrap();
        let lock_path = dir.join(".lock");
        let current_log = log_path(&dir, today);

        let directory_guard = create_private_log_dir(&dir).unwrap();
        fs::File::create(&lock_path).unwrap();
        fs::File::create(&current_log).unwrap();
        let mut state = LogState::open(dir.clone(), directory_guard).unwrap();
        append_log(&mut state, today, b"private event", true).unwrap();

        assert!(has_protected_dacl(&dir));
        assert!(has_protected_dacl(&lock_path));
        assert!(has_protected_dacl(&current_log));
    }

    #[cfg(windows)]
    #[test]
    fn cached_log_state_never_follows_preexisting_file_reparse_points() {
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

        match LogState::open(dir.clone(), directory_guard) {
            Ok(_) => panic!("the lock path must never follow a reparse point"),
            Err(error) => assert!(error.to_string().contains("private log lock")),
        }
        fs::remove_file(dir.join(".lock")).unwrap();

        let directory_guard = create_private_log_dir(&dir).unwrap();
        let mut state = LogState::open(dir.clone(), directory_guard).unwrap();
        symlink_file(&target, log_path(&dir, today)).unwrap();
        append_log(&mut state, today, b"private event", false)
            .expect_err("the daily log path must never follow a reparse point");

        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
    }
}
