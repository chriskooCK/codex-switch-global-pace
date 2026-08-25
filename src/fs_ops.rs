use anyhow::{Context, Result};
use rand::Rng as _;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::ffi::CString;
use std::ffi::OsStr;
use std::fmt;
#[cfg(windows)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

const QUARANTINE_PREFIX: &str = ".codex-switch-global-pace.installer-quarantine-";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoveExactOutcome {
    Removed,
    #[cfg_attr(windows, allow(dead_code))]
    RemovedNamespaceDurabilityUnconfirmed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CreateExactOutcome {
    Created(FileToken),
    #[cfg_attr(windows, allow(dead_code))]
    CreatedNamespaceDurabilityUnconfirmed(FileToken),
}

impl CreateExactOutcome {
    pub(crate) fn token(&self) -> &FileToken {
        match self {
            Self::Created(token) | Self::CreatedNamespaceDurabilityUnconfirmed(token) => token,
        }
    }
}

#[cfg(windows)]
pub(crate) struct WindowsTransaction {
    handle: windows_sys::Win32::Foundation::HANDLE,
    committed: bool,
}

#[cfg(windows)]
impl WindowsTransaction {
    pub(crate) fn create(purpose: &str) -> Result<Self> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::CreateTransaction;

        let handle = unsafe {
            // SAFETY: every optional pointer is null and scalar arguments request
            // the documented default KTM transaction behavior.
            CreateTransaction(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                std::ptr::null(),
            )
        };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("creating the required Windows {purpose} transaction"));
        }
        Ok(Self {
            handle,
            committed: false,
        })
    }

    pub(crate) fn handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.handle
    }

    pub(crate) fn commit(mut self, purpose: &str) -> Result<()> {
        use windows_sys::Win32::Storage::FileSystem::CommitTransaction;

        let result = unsafe {
            // SAFETY: this object uniquely owns a live transaction handle.
            CommitTransaction(self.handle)
        };
        if result == 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("committing the Windows {purpose} transaction; no fallback was attempted")
            });
        }
        self.committed = true;
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WindowsTransaction {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Storage::FileSystem::RollbackTransaction;

        unsafe {
            // SAFETY: the object uniquely owns this live transaction handle.
            if !self.committed {
                RollbackTransaction(self.handle);
            }
            CloseHandle(self.handle);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileToken {
    identity_high: u64,
    identity_low: u64,
    digest: [u8; 32],
}

impl FileToken {
    pub(crate) fn same_contents(&self, other: &Self) -> bool {
        self.digest == other.digest
    }

    pub(crate) fn matches_bytes(&self, bytes: &[u8]) -> bool {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        self.digest == digest
    }
}

impl fmt::Display for FileToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}|{}",
            self.identity_high,
            self.identity_low,
            hex::encode(self.digest)
        )
    }
}

impl FromStr for FileToken {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (identity, digest) = value
            .split_once('|')
            .context("file token does not contain a digest separator")?;
        let (identity_high, identity_low) = identity
            .split_once(':')
            .context("file token does not contain an identity separator")?;
        let decoded = hex::decode(digest).context("file token digest is not hexadecimal")?;
        let digest: [u8; 32] = decoded
            .try_into()
            .map_err(|_| anyhow::anyhow!("file token digest is not 32 bytes"))?;
        Ok(Self {
            identity_high: identity_high
                .parse()
                .context("file token identity prefix is not an integer")?,
            identity_low: identity_low
                .parse()
                .context("file token identity suffix is not an integer")?,
            digest,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileSnapshot {
    identity_high: u64,
    identity_low: u64,
    length: u64,
    changed_high: i64,
    changed_low: i64,
}

#[cfg(unix)]
fn file_snapshot(_file: &File, metadata: &fs::Metadata) -> Result<FileSnapshot> {
    Ok(FileSnapshot {
        identity_high: metadata.dev(),
        identity_low: metadata.ino(),
        length: metadata.len(),
        changed_high: metadata.ctime(),
        changed_low: metadata.ctime_nsec(),
    })
}

#[cfg(windows)]
fn file_snapshot(file: &File, metadata: &fs::Metadata) -> Result<FileSnapshot> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let result = unsafe {
        // SAFETY: `file` owns a live handle and information is a correctly
        // sized writable result structure for GetFileInformationByHandle.
        GetFileInformationByHandle(file.as_raw_handle(), &mut information)
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .context("reading stable Windows file identity from an open handle");
    }
    Ok(FileSnapshot {
        identity_high: u64::from(information.dwVolumeSerialNumber),
        identity_low: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
        length: metadata.len(),
        changed_high: i64::from(information.ftLastWriteTime.dwHighDateTime),
        changed_low: i64::from(information.ftLastWriteTime.dwLowDateTime),
    })
}

fn snapshot(file: &File, metadata: &fs::Metadata) -> Result<FileSnapshot> {
    file_snapshot(file, metadata)
}

pub(crate) fn token_for_file(file: &mut File) -> Result<FileToken> {
    let before = snapshot(
        file,
        &file
            .metadata()
            .context("reading transaction file metadata")?,
    )?;
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    file.seek(SeekFrom::Start(0))?;
    let after = snapshot(
        file,
        &file
            .metadata()
            .context("rechecking transaction file metadata")?,
    )?;
    if before != after {
        anyhow::bail!("transaction file changed while it was hashed");
    }
    Ok(FileToken {
        identity_high: after.identity_high,
        identity_low: after.identity_low,
        digest: hasher.finalize().into(),
    })
}

pub(crate) fn token_for_path(path: &Path) -> Result<FileToken> {
    token_for_file(&mut open_direct_regular(path)?)
        .with_context(|| format!("binding transaction path {}", path.display()))
}

pub(crate) fn token_if_present(path: &Path) -> Result<Option<FileToken>> {
    match fs::symlink_metadata(path) {
        Ok(_) => token_for_path(path).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn ensure_path_token(path: &Path, expected: &FileToken, purpose: &str) -> Result<()> {
    let observed = token_for_path(path)?;
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

#[cfg(unix)]
fn direct_parent(path: &Path) -> Result<(PathBuf, &OsStr)> {
    if !path.is_absolute() {
        anyhow::bail!(
            "installer transaction path is not absolute: {}",
            path.display()
        );
    }
    let parent = path
        .parent()
        .with_context(|| format!("transaction path has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .with_context(|| format!("transaction path has no file name: {}", path.display()))?;
    if name.as_bytes().is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
        anyhow::bail!("invalid transaction file name: {}", path.display());
    }
    Ok((parent.to_path_buf(), name))
}

#[cfg(unix)]
fn open_direct_directory(path: &Path) -> Result<File> {
    let before = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting transaction directory {}", path.display()))?;
    if !before.file_type().is_dir() || before.file_type().is_symlink() {
        anyhow::bail!(
            "transaction parent is not a direct directory: {}",
            path.display()
        );
    }
    let directory = File::open(path)
        .with_context(|| format!("opening transaction directory {}", path.display()))?;
    let opened = directory
        .metadata()
        .with_context(|| format!("identifying transaction directory {}", path.display()))?;
    if !opened.file_type().is_dir() || before.dev() != opened.dev() || before.ino() != opened.ino()
    {
        anyhow::bail!(
            "transaction parent changed while it was opened: {}",
            path.display()
        );
    }
    Ok(directory)
}

#[cfg(unix)]
fn component_c_string(name: &OsStr, path: &Path) -> Result<CString> {
    CString::new(name.as_bytes())
        .with_context(|| format!("transaction path contains a NUL byte: {}", path.display()))
}

/// Atomically renames one explicit sibling file without replacing any entry at
/// the destination. Linux and macOS provide the required kernel primitive; no
/// check-then-rename fallback is permitted.
#[cfg(unix)]
pub(crate) fn rename_noreplace(source: &Path, destination: &Path) -> Result<()> {
    let (source_parent, source_name) = direct_parent(source)?;
    let (destination_parent, destination_name) = direct_parent(destination)?;
    if source_parent != destination_parent {
        anyhow::bail!(
            "no-replace rename requires sibling paths: {} -> {}",
            source.display(),
            destination.display()
        );
    }
    let directory = open_direct_directory(&source_parent)?;
    let source_name = component_c_string(source_name, source)?;
    let destination_name = component_c_string(destination_name, destination)?;

    #[cfg(target_os = "linux")]
    let result = unsafe {
        // SAFETY: the directory descriptor and both NUL-terminated names remain
        // valid for this call. RENAME_NOREPLACE is the required atomic boundary.
        libc::renameat2(
            directory.as_raw_fd(),
            source_name.as_ptr(),
            directory.as_raw_fd(),
            destination_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        // SAFETY: the directory descriptor and both NUL-terminated names remain
        // valid for this call. RENAME_EXCL is the required atomic boundary.
        libc::renameatx_np(
            directory.as_raw_fd(),
            source_name.as_ptr(),
            directory.as_raw_fd(),
            destination_name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let result = { anyhow::bail!("atomic no-replace rename is supported only on Linux and macOS") };

    if result != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "atomically renaming without replacement {} -> {}",
                source.display(),
                destination.display()
            )
        });
    }
    directory.sync_all().with_context(|| {
        format!(
            "persisting no-replace rename in {}",
            source_parent.display()
        )
    })
}

/// Atomically swaps two sibling names without creating a moment at which
/// either public name is absent.
#[cfg(unix)]
pub(crate) fn exchange(source: &Path, destination: &Path) -> Result<()> {
    let (source_parent, source_name) = direct_parent(source)?;
    let (destination_parent, destination_name) = direct_parent(destination)?;
    if source_parent != destination_parent {
        anyhow::bail!(
            "atomic exchange requires sibling paths: {} <-> {}",
            source.display(),
            destination.display()
        );
    }
    let directory = open_direct_directory(&source_parent)?;
    let source_name = component_c_string(source_name, source)?;
    let destination_name = component_c_string(destination_name, destination)?;

    #[cfg(target_os = "linux")]
    let result = unsafe {
        // SAFETY: the names are NUL terminated and relative to the live
        // directory descriptor. RENAME_EXCHANGE is one atomic kernel action.
        libc::renameat2(
            directory.as_raw_fd(),
            source_name.as_ptr(),
            directory.as_raw_fd(),
            destination_name.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        // SAFETY: the names are NUL terminated and relative to the live
        // directory descriptor. RENAME_SWAP is one atomic kernel action.
        libc::renameatx_np(
            directory.as_raw_fd(),
            source_name.as_ptr(),
            directory.as_raw_fd(),
            destination_name.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let result = { anyhow::bail!("atomic exchange is supported only on Linux and macOS") };

    if result != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "atomically exchanging {} and {}",
                source.display(),
                destination.display()
            )
        });
    }
    directory
        .sync_all()
        .with_context(|| format!("persisting atomic exchange in {}", source_parent.display()))
}

/// Creates one regular file with O_EXCL in the already-opened direct parent.
#[cfg(unix)]
pub(crate) fn create_new_file(path: &Path, mode: libc::mode_t) -> Result<File> {
    let (parent, name) = direct_parent(path)?;
    let directory = open_direct_directory(&parent)?;
    let name = component_c_string(name, path)?;
    // macOS mode_t is narrower than C's variadic integer slot, so Rust
    // requires the default integer promotion to be explicit at this call.
    #[cfg(target_os = "macos")]
    let mode = libc::c_uint::from(mode);
    let descriptor = unsafe {
        // SAFETY: directory and name are valid for this openat call. O_EXCL and
        // O_NOFOLLOW ensure an existing file or link is never adopted.
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("exclusively creating {}", path.display()));
    }
    let file = unsafe {
        // SAFETY: descriptor was returned uniquely by openat and is transferred
        // into File exactly once.
        File::from_raw_fd(descriptor)
    };
    Ok(file)
}

#[cfg(unix)]
pub(crate) fn open_direct_regular(path: &Path) -> Result<File> {
    let (parent, name) = direct_parent(path)?;
    let directory = open_direct_directory(&parent)?;
    let name = component_c_string(name, path)?;
    let descriptor = unsafe {
        // SAFETY: the verified parent descriptor and NUL-terminated direct
        // child name remain valid. O_NOFOLLOW rejects a final symlink.
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("opening direct transaction file {}", path.display()));
    }
    let file = unsafe {
        // SAFETY: descriptor is uniquely owned after the successful openat and
        // is transferred into File exactly once.
        File::from_raw_fd(descriptor)
    };
    let opened = file
        .metadata()
        .with_context(|| format!("identifying transaction file {}", path.display()))?;
    let at_path = fs::symlink_metadata(path)
        .with_context(|| format!("rechecking transaction path {}", path.display()))?;
    if !opened.file_type().is_file()
        || !at_path.file_type().is_file()
        || at_path.file_type().is_symlink()
        || at_path.dev() != opened.dev()
        || at_path.ino() != opened.ino()
    {
        anyhow::bail!(
            "transaction file changed while it was opened: {}",
            path.display()
        );
    }
    Ok(file)
}

#[cfg(unix)]
pub(crate) fn sync_parent(path: &Path) -> Result<()> {
    let (parent, _) = direct_parent(path)?;
    open_direct_directory(&parent)?
        .sync_all()
        .with_context(|| format!("persisting transaction directory {}", parent.display()))
}

#[cfg(windows)]
fn direct_parent(path: &Path) -> Result<(PathBuf, &OsStr)> {
    if !path.is_absolute() {
        anyhow::bail!(
            "installer transaction path is not absolute: {}",
            path.display()
        );
    }
    let parent = path
        .parent()
        .with_context(|| format!("transaction path has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .with_context(|| format!("transaction path has no file name: {}", path.display()))?;
    if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
        anyhow::bail!("invalid transaction file name: {}", path.display());
    }
    Ok((parent.to_path_buf(), name))
}

#[cfg(windows)]
fn validate_direct_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting transaction directory {}", path.display()))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        anyhow::bail!(
            "transaction parent is not a direct directory: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        anyhow::bail!(
            "transaction path contains a NUL character: {}",
            path.display()
        );
    }
    wide.push(0);
    Ok(wide)
}

#[cfg(windows)]
pub(crate) fn create_new_file(path: &Path, _mode: u32) -> Result<File> {
    let (parent, _) = direct_parent(path)?;
    validate_direct_directory(&parent)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("exclusively creating {}", path.display()))
}

#[cfg(windows)]
pub(crate) fn open_direct_regular(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let (parent, _) = direct_parent(path)?;
    validate_direct_directory(&parent)?;
    let before = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting transaction file {}", path.display()))?;
    if !before.file_type().is_file() || before.file_type().is_symlink() {
        anyhow::bail!(
            "transaction path is not a direct regular file: {}",
            path.display()
        );
    }
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .with_context(|| format!("opening transaction file {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("identifying transaction file {}", path.display()))?;
    if !opened.file_type().is_file() || opened.file_type().is_symlink() {
        anyhow::bail!(
            "transaction file changed while it was opened: {}",
            path.display()
        );
    }
    Ok(file)
}

#[cfg(windows)]
pub(crate) fn rename_noreplace(source: &Path, destination: &Path) -> Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let (source_parent, _) = direct_parent(source)?;
    let (destination_parent, _) = direct_parent(destination)?;
    if source_parent != destination_parent {
        anyhow::bail!(
            "no-replace rename requires sibling paths: {} -> {}",
            source.display(),
            destination.display()
        );
    }
    validate_direct_directory(&source_parent)?;
    let source = wide_path(source)?;
    let destination = wide_path(destination)?;
    let result = unsafe {
        // SAFETY: both paths are valid NUL-terminated UTF-16 buffers. Omitting
        // MOVEFILE_REPLACE_EXISTING is the required no-replace boundary.
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .context("atomically renaming a Windows transaction file without replacement");
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn replace_with_displaced(
    replacement: &Path,
    destination: &Path,
    displaced: &Path,
) -> Result<()> {
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let (replacement_parent, _) = direct_parent(replacement)?;
    let (destination_parent, _) = direct_parent(destination)?;
    let (displaced_parent, _) = direct_parent(displaced)?;
    if replacement_parent != destination_parent || replacement_parent != displaced_parent {
        anyhow::bail!("ReplaceFileW transaction paths must be siblings");
    }
    validate_direct_directory(&replacement_parent)?;
    let replacement = wide_path(replacement)?;
    let destination = wide_path(destination)?;
    let displaced = wide_path(displaced)?;
    let result = unsafe {
        // SAFETY: all three paths are stable NUL-terminated UTF-16 buffers and
        // the caller supplies a fresh random displaced name. ReplaceFileW is
        // the supported single-file atomic replacement primitive on Windows.
        ReplaceFileW(
            destination.as_ptr(),
            replacement.as_ptr(),
            displaced.as_ptr(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .context("atomically replacing a Windows file with displaced recovery");
    }
    Ok(())
}

pub(crate) fn create_exclusive_copy(
    source: &Path,
    destination: &Path,
    expected: &FileToken,
) -> Result<CreateExactOutcome> {
    let mut source_file = open_direct_regular(source)?;
    let source_token = token_for_file(&mut source_file)?;
    if &source_token != expected {
        anyhow::bail!(
            "copy source changed before the exclusive copy: {}",
            source.display()
        );
    }
    #[cfg(unix)]
    let source_mode = source_file.metadata()?.permissions().mode();
    let mut destination_file = create_new_file(destination, 0o600)?;
    let mut created_token = None;
    let creation = (|| -> Result<FileToken> {
        std::io::copy(&mut source_file, &mut destination_file).with_context(|| {
            format!("copying {} to {}", source.display(), destination.display())
        })?;
        #[cfg(unix)]
        destination_file.set_permissions(fs::Permissions::from_mode(source_mode))?;
        destination_file.flush()?;
        destination_file.sync_all().with_context(|| {
            format!("flushing copied transaction file {}", destination.display())
        })?;
        let destination_token = token_for_file(&mut destination_file)?;
        created_token = Some(destination_token.clone());
        let source_token_after = token_for_file(&mut source_file)?;
        if &source_token_after != expected || &token_for_path(source)? != expected {
            anyhow::bail!(
                "copy source changed while {} was created",
                destination.display()
            );
        }
        if !destination_token.same_contents(expected) {
            anyhow::bail!(
                "exclusive copy at {} does not match the source bytes",
                destination.display()
            );
        }
        if token_for_path(destination)? != destination_token {
            anyhow::bail!(
                "exclusive copy path changed before its creation boundary completed: {}",
                destination.display()
            );
        }
        Ok(destination_token)
    })();
    if creation.is_err() && created_token.is_none() {
        created_token = token_for_file(&mut destination_file).ok();
    }
    drop(destination_file);
    let token = match creation {
        Ok(token) => token,
        Err(error) => {
            return Err(cleanup_failed_creation(
                destination,
                created_token.as_ref(),
                error,
            ));
        }
    };
    #[cfg(unix)]
    let outcome = match sync_parent(destination) {
        Ok(()) => CreateExactOutcome::Created(token),
        Err(_) => CreateExactOutcome::CreatedNamespaceDurabilityUnconfirmed(token),
    };
    #[cfg(windows)]
    // Windows does not support FlushFileBuffers on directory handles. The
    // exact newly-created file handle itself was flushed above; do not invent
    // an unsupported parent-directory flush or silently fall back to one.
    let outcome = CreateExactOutcome::Created(token);
    Ok(outcome)
}

pub(crate) fn create_empty_exclusive(destination: &Path) -> Result<CreateExactOutcome> {
    let mut file = create_new_file(destination, 0o600)?;
    let creation = (|| -> Result<FileToken> {
        file.sync_all().with_context(|| {
            format!("flushing empty transaction file {}", destination.display())
        })?;
        let token = token_for_file(&mut file)?;
        if token_for_path(destination)? != token {
            anyhow::bail!(
                "empty transaction file changed before its creation boundary completed: {}",
                destination.display()
            );
        }
        Ok(token)
    })();
    let cleanup_token = match &creation {
        Ok(token) => Some(token.clone()),
        Err(_) => token_for_file(&mut file).ok(),
    };
    drop(file);
    let token = match creation {
        Ok(token) => token,
        Err(error) => {
            return Err(cleanup_failed_creation(
                destination,
                cleanup_token.as_ref(),
                error,
            ));
        }
    };
    #[cfg(unix)]
    let outcome = match sync_parent(destination) {
        Ok(()) => CreateExactOutcome::Created(token),
        Err(_) => CreateExactOutcome::CreatedNamespaceDurabilityUnconfirmed(token),
    };
    #[cfg(windows)]
    // See create_exclusive_copy: FlushFileBuffers is applied to the exact file
    // handle, and no unsupported directory-flush fallback is claimed.
    let outcome = CreateExactOutcome::Created(token);
    Ok(outcome)
}

fn cleanup_failed_creation(
    destination: &Path,
    created_token: Option<&FileToken>,
    creation_error: anyhow::Error,
) -> anyhow::Error {
    let Some(created_token) = created_token else {
        return creation_error.context(format!(
            "exclusive creation failed and its unclassified residue was preserved at {}",
            destination.display()
        ));
    };
    match remove_exact(destination, created_token) {
        Ok(RemoveExactOutcome::Removed) => creation_error.context(format!(
            "exclusive creation failed; its exact residue was removed from {}",
            destination.display()
        )),
        Ok(RemoveExactOutcome::RemovedNamespaceDurabilityUnconfirmed) => {
            creation_error.context(format!(
            "exclusive creation failed; its exact residue was removed from {}, but cleanup durability was not confirmed",
            destination.display()
        ))
        }
        Err(cleanup_error) => creation_error.context(format!(
            "exclusive creation failed and exact cleanup could not complete at {}: {cleanup_error:#}",
            destination.display()
        )),
    }
}

fn quarantine_path(source: &Path) -> Result<PathBuf> {
    let parent = source
        .parent()
        .with_context(|| format!("transaction path has no parent: {}", source.display()))?;
    let mut nonce = [0_u8; 16];
    rand::rng().fill_bytes(&mut nonce);
    Ok(parent.join(format!("{QUARANTINE_PREFIX}{}", hex::encode(nonce))))
}

#[cfg(unix)]
fn remove_quarantined_exact(path: &Path, expected: &FileToken) -> Result<RemoveExactOutcome> {
    ensure_path_token(path, expected, "quarantined transaction file")?;
    fs::remove_file(path)
        .with_context(|| format!("removing token-bound quarantine {}", path.display()))?;
    Ok(match sync_parent(path) {
        Ok(()) => RemoveExactOutcome::Removed,
        Err(_) => RemoveExactOutcome::RemovedNamespaceDurabilityUnconfirmed,
    })
}

#[cfg(windows)]
fn remove_quarantined_exact(path: &Path, expected: &FileToken) -> Result<RemoveExactOutcome> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileDispositionInfoEx, SetFileInformationByHandle,
    };

    let before = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting quarantined file {}", path.display()))?;
    if !before.file_type().is_file() || before.file_type().is_symlink() {
        anyhow::bail!(
            "quarantine is not a direct regular file: {}",
            path.display()
        );
    }
    let mut file = OpenOptions::new()
        .access_mode(FILE_GENERIC_READ | DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .with_context(|| format!("opening exact quarantined file {}", path.display()))?;
    let observed = token_for_file(&mut file)?;
    if &observed != expected {
        anyhow::bail!(
            "quarantined file changed before handle-bound deletion: {}",
            path.display()
        );
    }
    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    let result = unsafe {
        // SAFETY: `file` owns a live handle and disposition points to the exact
        // structure/size required by FileDispositionInfoEx. Deletion is bound
        // to this verified handle, never to a later path occupant.
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("marking exact quarantine for deletion {}", path.display()));
    }
    drop(file);
    if token_if_present(path)?.is_some() {
        anyhow::bail!(
            "handle-bound deletion did not remove the exact quarantine namespace entry: {}",
            path.display()
        );
    }
    // Windows has no supported FlushFileBuffers boundary for a directory
    // handle. The exact verified file handle has been dispositioned and its
    // namespace absence was confirmed above; do not claim Unix-style parent
    // directory durability or introduce an unsupported fallback.
    Ok(RemoveExactOutcome::Removed)
}

pub(crate) fn remove_exact(source: &Path, expected: &FileToken) -> Result<RemoveExactOutcome> {
    let quarantine = quarantine_path(source)?;
    ensure_path_token(source, expected, "removal source")?;
    let boundary = rename_noreplace(source, &quarantine);
    let source_after = token_if_present(source)?;
    let quarantine_after = token_if_present(&quarantine)?;
    if quarantine_after.as_ref() == Some(expected) {
        let cleanup = remove_quarantined_exact(&quarantine, expected);
        if let Ok(outcome) = cleanup {
            return Ok(outcome);
        }
        let cleanup_error = cleanup.expect_err("checked cleanup failure");
        let source_after_cleanup = token_if_present(source)?;
        let quarantine_after_cleanup = token_if_present(&quarantine)?;
        if source_after_cleanup.is_none() && quarantine_after_cleanup.is_none() {
            anyhow::bail!(
                "quarantine cleanup failed after both names disappeared unexpectedly ({cleanup_error:#}); source {} and quarantine {} require explicit inspection",
                source.display(),
                quarantine.display()
            );
        }
        if source_after_cleanup.is_none() && quarantine_after_cleanup.as_ref() == Some(expected) {
            if token_if_present(source)? != source_after_cleanup
                || token_if_present(&quarantine)? != quarantine_after_cleanup
            {
                anyhow::bail!(
                    "quarantine cleanup failed ({cleanup_error:#}) and a path changed before restoration; source {} and quarantine {} were preserved",
                    source.display(),
                    quarantine.display()
                );
            }
            let restoration = rename_noreplace(&quarantine, source);
            let restored_source = token_if_present(source)?;
            let restored_quarantine = token_if_present(&quarantine)?;
            if restored_source.as_ref() == Some(expected) && restored_quarantine.is_none() {
                let restoration_note = restoration
                    .err()
                    .map(|error| format!("; restoration durability failed: {error:#}"))
                    .unwrap_or_default();
                anyhow::bail!(
                    "quarantine cleanup failed ({cleanup_error:#}); the exact file was restored without replacement from {} to {}{restoration_note}",
                    quarantine.display(),
                    source.display()
                );
            }
        }
        anyhow::bail!(
            "quarantine cleanup failed ({cleanup_error:#}) and exact restoration was not safe; source {} and quarantine {} were preserved",
            source.display(),
            quarantine.display()
        );
    }
    if source_after.as_ref() == Some(expected) && quarantine_after.is_none() {
        return Err(boundary.err().unwrap_or_else(|| {
            anyhow::anyhow!("no-replace quarantine returned success without moving the owned file")
        }));
    }
    if source_after.is_none() && quarantine_after.is_some() {
        if token_if_present(source)? != source_after
            || token_if_present(&quarantine)? != quarantine_after
        {
            anyhow::bail!(
                "unexpected quarantine occupant changed again; source {} and quarantine {} were preserved",
                source.display(),
                quarantine.display()
            );
        }
        let restoration = rename_noreplace(&quarantine, source);
        let restored_source = token_if_present(source)?;
        let restored_quarantine = token_if_present(&quarantine)?;
        if restored_source == quarantine_after && restored_quarantine.is_none() {
            let restoration_note = restoration
                .err()
                .map(|error| format!("; restoration durability failed: {error:#}"))
                .unwrap_or_default();
            anyhow::bail!(
                "a concurrent writer reached the removal boundary; its actual file was restored to {} from {} without replacement{restoration_note}",
                source.display(),
                quarantine.display()
            );
        }
    }
    anyhow::bail!(
        "token-bound quarantine ended in an unclassified state; source {} and quarantine {} were preserved",
        source.display(),
        quarantine.display()
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CreateExactOutcome, RemoveExactOutcome, create_exclusive_copy, remove_exact, token_for_path,
    };
    use std::fs;

    #[test]
    fn exclusive_copy_and_exact_removal_preserve_the_bound_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let destination = directory.path().join("destination.bin");
        fs::write(&source, b"verified installer bytes").unwrap();
        let source_token = token_for_path(&source).unwrap();

        let outcome = create_exclusive_copy(&source, &destination, &source_token).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"verified installer bytes");
        assert_eq!(token_for_path(&destination).unwrap(), *outcome.token());
        assert!(outcome.token().same_contents(&source_token));

        let removal = remove_exact(&destination, outcome.token()).unwrap();
        assert!(matches!(
            removal,
            RemoveExactOutcome::Removed | RemoveExactOutcome::RemovedNamespaceDurabilityUnconfirmed
        ));
        assert!(!destination.exists());
    }

    #[test]
    fn exclusive_copy_never_overwrites_a_preexisting_destination() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let destination = directory.path().join("destination.bin");
        fs::write(&source, b"candidate").unwrap();
        fs::write(&destination, b"foreign writer").unwrap();
        let source_token = token_for_path(&source).unwrap();

        assert!(create_exclusive_copy(&source, &destination, &source_token).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"foreign writer");
    }

    #[test]
    fn source_token_mismatch_fails_before_creating_a_destination() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let other = directory.path().join("other.bin");
        let destination = directory.path().join("destination.bin");
        fs::write(&source, b"candidate").unwrap();
        fs::write(&other, b"different bytes").unwrap();
        let wrong_token = token_for_path(&other).unwrap();

        assert!(create_exclusive_copy(&source, &destination, &wrong_token).is_err());
        assert!(!destination.exists());
    }

    #[test]
    fn exact_removal_rejects_a_different_token_without_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let other = directory.path().join("other.bin");
        fs::write(&source, b"owned bytes").unwrap();
        fs::write(&other, b"foreign bytes").unwrap();
        let wrong_token = token_for_path(&other).unwrap();

        assert!(remove_exact(&source, &wrong_token).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"owned bytes");
    }

    #[test]
    fn created_outcome_exposes_the_exact_created_token() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.bin");
        let destination = directory.path().join("destination.bin");
        fs::write(&source, b"candidate").unwrap();
        let source_token = token_for_path(&source).unwrap();

        let outcome = create_exclusive_copy(&source, &destination, &source_token).unwrap();
        assert!(matches!(
            outcome,
            CreateExactOutcome::Created(_)
                | CreateExactOutcome::CreatedNamespaceDurabilityUnconfirmed(_)
        ));
    }
}
