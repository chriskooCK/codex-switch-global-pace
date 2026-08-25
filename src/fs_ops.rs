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

#[derive(Debug)]
#[must_use = "a visible directory rename with unconfirmed durability must be handled explicitly"]
pub(crate) enum DirectoryRenameOutcome {
    DurablyRenamed,
    #[cfg_attr(windows, allow(dead_code))]
    VisibleDurabilityUnconfirmed {
        cause: anyhow::Error,
    },
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

/// Create an application-owned temporary directory whose returned path has no
/// symlink or reparse-point components. The operating system may expose its
/// temporary root through an alias (macOS commonly does), so resolve that root
/// before creating the random child rather than accepting an aliased child
/// path at a later transaction boundary.
pub(crate) fn create_direct_tempdir() -> Result<tempfile::TempDir> {
    create_direct_tempdir_in(&std::env::temp_dir())
}

fn create_direct_tempdir_in(parent: &Path) -> Result<tempfile::TempDir> {
    let physical_parent = fs::canonicalize(parent)
        .with_context(|| format!("resolving temporary directory root {}", parent.display()))?;
    let directory = tempfile::Builder::new()
        .tempdir_in(&physical_parent)
        .with_context(|| {
            format!(
                "creating application temporary directory in {}",
                physical_parent.display()
            )
        })?;

    #[cfg(unix)]
    open_direct_directory(directory.path()).with_context(|| {
        format!(
            "validating direct application temporary directory {}",
            directory.path().display()
        )
    })?;
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;

        open_pinned_direct_directory(directory.path(), FILE_READ_ATTRIBUTES).with_context(
            || {
                format!(
                    "validating direct application temporary directory {}",
                    directory.path().display()
                )
            },
        )?;
    }
    #[cfg(not(any(unix, windows)))]
    anyhow::bail!(
        "direct application temporary directories are unsupported on this platform: {}",
        directory.path().display()
    );

    Ok(directory)
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
    if !path.is_absolute() {
        anyhow::bail!(
            "transaction directory path is not absolute: {}",
            path.display()
        );
    }

    let mut directory = File::open("/").context("opening filesystem root directory")?;
    let mut walked = PathBuf::from("/");
    for component in path.components() {
        let name = match component {
            std::path::Component::RootDir => continue,
            std::path::Component::CurDir => continue,
            std::path::Component::Normal(name) => name,
            std::path::Component::ParentDir => {
                anyhow::bail!("transaction directory contains '..': {}", path.display())
            }
            std::path::Component::Prefix(_) => {
                anyhow::bail!(
                    "unexpected platform prefix in Unix transaction directory: {}",
                    path.display()
                )
            }
        };
        let name = component_c_string(name, path)?;
        let descriptor = unsafe {
            // SAFETY: `directory` is a live directory descriptor and `name`
            // is NUL-terminated. O_NOFOLLOW and O_DIRECTORY bind this exact
            // path component in one syscall instead of checking then following.
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        walked.push(OsStr::from_bytes(name.as_bytes()));
        if descriptor == -1 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "opening direct transaction directory component {}",
                    walked.display()
                )
            });
        }
        directory = unsafe {
            // SAFETY: a successful openat returned a new uniquely owned fd.
            File::from_raw_fd(descriptor)
        };
    }
    Ok(directory)
}

#[cfg(unix)]
fn component_c_string(name: &OsStr, path: &Path) -> Result<CString> {
    CString::new(name.as_bytes())
        .with_context(|| format!("transaction path contains a NUL byte: {}", path.display()))
}

#[cfg(target_os = "linux")]
unsafe fn linux_renameat2(
    source_directory: libc::c_int,
    source_name: *const libc::c_char,
    destination_directory: libc::c_int,
    destination_name: *const libc::c_char,
    flags: libc::c_uint,
) -> libc::c_long {
    unsafe {
        // SAFETY: callers keep both directory descriptors and NUL-terminated
        // component names live for this Linux kernel syscall.
        libc::syscall(
            libc::SYS_renameat2,
            source_directory,
            source_name,
            destination_directory,
            destination_name,
            flags,
        )
    }
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
        linux_renameat2(
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

/// Atomically move one direct directory without replacing the destination and
/// durably publish both sides of a cross-parent rename.
#[cfg(unix)]
fn rename_directory_entry_noreplace(
    source_parent: &File,
    source_name: &CString,
    destination_parent: &File,
    destination_name: &CString,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    let result = unsafe {
        // SAFETY: both descriptors and NUL-terminated component names remain
        // live for the syscall. RENAME_NOREPLACE is one atomic boundary.
        linux_renameat2(
            source_parent.as_raw_fd(),
            source_name.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        // SAFETY: as above; RENAME_EXCL supplies the no-replace contract.
        libc::renameatx_np(
            source_parent.as_raw_fd(),
            source_name.as_ptr(),
            destination_parent.as_raw_fd(),
            destination_name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let result: libc::c_int = return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "durable directory rename is supported only on Linux and macOS",
    ));

    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
pub(crate) fn rename_directory_noreplace_durable(
    source: &Path,
    destination: &Path,
) -> Result<DirectoryRenameOutcome> {
    rename_directory_noreplace_durable_impl(source, destination, || {})
}

#[cfg(all(unix, test))]
fn rename_directory_noreplace_durable_with_hook(
    source: &Path,
    destination: &Path,
    before_rename: impl FnOnce(),
) -> Result<DirectoryRenameOutcome> {
    rename_directory_noreplace_durable_impl(source, destination, before_rename)
}

#[cfg(unix)]
fn rename_directory_noreplace_durable_impl(
    source: &Path,
    destination: &Path,
    before_rename: impl FnOnce(),
) -> Result<DirectoryRenameOutcome> {
    let (source_parent, source_name) = direct_parent(source)?;
    let (destination_parent, destination_name) = direct_parent(destination)?;
    let source_directory = open_direct_directory(&source_parent)?;
    let destination_parent_directory = if source_parent == destination_parent {
        None
    } else {
        Some(open_direct_directory(&destination_parent)?)
    };
    // Open the exact source before the namespace mutation so a final-component
    // symlink can never be renamed as a profile directory.
    let moved_directory = open_direct_directory(source)?;
    let moved_metadata = moved_directory
        .metadata()
        .with_context(|| format!("binding source directory {}", source.display()))?;
    let moved_identity = (moved_metadata.dev(), moved_metadata.ino());
    let source_name = component_c_string(source_name, source)?;
    let destination_name = component_c_string(destination_name, destination)?;
    let destination_directory = destination_parent_directory
        .as_ref()
        .unwrap_or(&source_directory);

    before_rename();

    if let Err(error) = rename_directory_entry_noreplace(
        &source_directory,
        &source_name,
        destination_directory,
        &destination_name,
    ) {
        return Err(error).with_context(|| {
            format!(
                "atomically renaming directory without replacement {} -> {}",
                source.display(),
                destination.display()
            )
        });
    }

    let published_directory = match open_direct_directory(destination).with_context(|| {
        format!(
            "opening the directory published by rename at {}",
            destination.display()
        )
    }) {
        Ok(directory) => directory,
        Err(publication_error) => {
            let rollback = rename_directory_entry_noreplace(
                destination_directory,
                &destination_name,
                &source_directory,
                &source_name,
            );
            match rollback {
                Ok(()) => {
                    let source_sync = source_directory.sync_all();
                    let destination_sync = if std::ptr::eq(destination_directory, &source_directory)
                    {
                        Ok(())
                    } else {
                        destination_directory.sync_all()
                    };
                    anyhow::bail!(
                        "source entry changed into a non-direct directory during exact rename ({publication_error:#}); the changed entry was restored without replacement to {} and profile metadata was not updated{}",
                        source.display(),
                        if source_sync.is_ok() && destination_sync.is_ok() {
                            String::new()
                        } else {
                            "; restoration durability was not confirmed".to_string()
                        }
                    );
                }
                Err(rollback_error) => anyhow::bail!(
                    "source entry changed into a non-direct directory during exact rename ({publication_error:#}) and exact restoration was not safe; profile metadata was not updated (source {}, destination {}, rollback: {rollback_error})",
                    source.display(),
                    destination.display()
                ),
            }
        }
    };
    let published_metadata = published_directory
        .metadata()
        .with_context(|| format!("binding renamed directory {}", destination.display()))?;
    let published_identity = (published_metadata.dev(), published_metadata.ino());
    if published_identity != moved_identity {
        let rollback = rename_directory_entry_noreplace(
            destination_directory,
            &destination_name,
            &source_directory,
            &source_name,
        );
        let restored_identity = open_direct_directory(source)
            .and_then(|directory| {
                let metadata = directory.metadata().with_context(|| {
                    format!("identifying restored directory {}", source.display())
                })?;
                Ok((metadata.dev(), metadata.ino()))
            })
            .ok();
        if rollback.is_ok() && restored_identity == Some(published_identity) {
            let source_sync = source_directory.sync_all();
            let destination_sync = if std::ptr::eq(destination_directory, &source_directory) {
                Ok(())
            } else {
                destination_directory.sync_all()
            };
            let durability_note = match (source_sync, destination_sync) {
                (Ok(()), Ok(())) => String::new(),
                (source_result, destination_result) => format!(
                    "; restoration durability was not confirmed (source parent: {}; destination parent: {})",
                    source_result
                        .err()
                        .map_or_else(|| "ok".to_string(), |error| error.to_string()),
                    destination_result
                        .err()
                        .map_or_else(|| "ok".to_string(), |error| error.to_string())
                ),
            };
            anyhow::bail!(
                "source directory changed during exact rename; the different directory was restored without replacement from {} to {}, profile metadata was not updated{durability_note}",
                destination.display(),
                source.display()
            );
        }
        anyhow::bail!(
            "source directory changed during exact rename and exact restoration was not safe; profile metadata was not updated (source {}, destination {}, rollback: {})",
            source.display(),
            destination.display(),
            rollback.err().map_or_else(
                || "postcondition mismatch".to_string(),
                |error| error.to_string()
            )
        );
    }

    let mut sync_errors = Vec::new();
    if let Err(error) = source_directory.sync_all() {
        sync_errors.push(format!(
            "source parent {}: {error}",
            source_parent.display()
        ));
    }
    if let Some(directory) = destination_parent_directory
        && let Err(error) = directory.sync_all()
    {
        sync_errors.push(format!(
            "destination parent {}: {error}",
            destination_parent.display()
        ));
    }
    if sync_errors.is_empty() {
        Ok(DirectoryRenameOutcome::DurablyRenamed)
    } else {
        Ok(DirectoryRenameOutcome::VisibleDurabilityUnconfirmed {
            cause: anyhow::anyhow!(sync_errors.join("; ")),
        })
    }
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
        linux_renameat2(
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
    use std::os::windows::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting transaction directory {}", path.display()))?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        anyhow::bail!(
            "transaction parent is not a direct directory: {}",
            path.display()
        );
    }
    Ok(())
}

/// Handles that pin one direct Windows directory and all of its ancestors.
#[cfg(windows)]
struct PinnedWindowsDirectory {
    /// Every component remains open so an intermediate directory cannot be
    /// exchanged after its child has been validated.
    handles: Vec<File>,
}

#[cfg(windows)]
impl PinnedWindowsDirectory {
    fn file(&self) -> &File {
        self.handles
            .last()
            .expect("an absolute directory path has at least its root handle")
    }
}

/// Open one exact directory while denying namespace replacement for the
/// lifetime of the returned handles. `access` is explicit because a
/// handle-bound rename needs `DELETE` on its source but only attribute access
/// on its destination parent.
#[cfg(windows)]
fn open_pinned_direct_directory(path: &Path, access: u32) -> Result<PinnedWindowsDirectory> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    if !path.is_absolute() {
        anyhow::bail!(
            "Windows transaction directory path is not absolute: {}",
            path.display()
        );
    }

    let mut walked = PathBuf::new();
    let mut prefixes = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(_) => walked.push(component.as_os_str()),
            std::path::Component::RootDir => {
                walked.push(component.as_os_str());
                prefixes.push(walked.clone());
            }
            std::path::Component::Normal(_) => {
                walked.push(component.as_os_str());
                prefixes.push(walked.clone());
            }
            std::path::Component::CurDir | std::path::Component::ParentDir => {
                anyhow::bail!(
                    "Windows transaction directory contains a relative component: {}",
                    path.display()
                )
            }
        }
    }
    if prefixes.is_empty() {
        anyhow::bail!(
            "Windows transaction directory has no openable component: {}",
            path.display()
        );
    }

    let mut handles = Vec::with_capacity(prefixes.len());
    let last = prefixes.len() - 1;
    for (index, component_path) in prefixes.into_iter().enumerate() {
        let component_access = if index == last {
            access
        } else {
            FILE_READ_ATTRIBUTES
        };
        let directory = OpenOptions::new()
            .access_mode(component_access)
            // Denying delete sharing pins this namespace component. Write
            // sharing is unrelated to replacing the directory itself and
            // permits normal mutation of entries below an app-owned parent.
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&component_path)
            .with_context(|| {
                format!(
                    "opening and pinning Windows directory component {}",
                    component_path.display()
                )
            })?;
        let metadata = directory.metadata().with_context(|| {
            format!(
                "identifying pinned Windows directory component {}",
                component_path.display()
            )
        })?;
        if !metadata.file_type().is_dir()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            anyhow::bail!(
                "pinned Windows path component is not a direct directory: {}",
                component_path.display()
            );
        }
        handles.push(directory);
    }
    Ok(PinnedWindowsDirectory { handles })
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

/// Rename the exact opened Windows directory handle without replacing the
/// destination. The source handle denies delete sharing until publication, so
/// another writer cannot exchange the path between validation and the rename.
#[cfg(windows)]
pub(crate) fn rename_directory_noreplace_durable(
    source: &Path,
    destination: &Path,
) -> Result<DirectoryRenameOutcome> {
    rename_directory_noreplace_durable_impl(source, destination, || {})
}

#[cfg(all(windows, test))]
fn rename_directory_noreplace_durable_with_hook(
    source: &Path,
    destination: &Path,
    before_rename: impl FnOnce(),
) -> Result<DirectoryRenameOutcome> {
    rename_directory_noreplace_durable_impl(source, destination, before_rename)
}

#[cfg(windows)]
fn rename_directory_noreplace_durable_impl(
    source: &Path,
    destination: &Path,
    before_rename: impl FnOnce(),
) -> Result<DirectoryRenameOutcome> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_GENERIC_READ, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FileRenameInfo,
        SetFileInformationByHandle,
    };

    let (_source_parent, _) = direct_parent(source)?;
    let (destination_parent, _) = direct_parent(destination)?;
    let source_directory = open_pinned_direct_directory(source, FILE_READ_ATTRIBUTES | DELETE)?;
    let _destination_directory =
        open_pinned_direct_directory(&destination_parent, FILE_GENERIC_READ)?;

    before_rename();

    // Keep every destination ancestor pinned, then use the documented common
    // absolute-name form. RootDirectory-relative renames are not accepted by
    // every local filesystem driver even though NTFS supports the structure.
    let destination_wide: Vec<u16> = destination.as_os_str().encode_wide().collect();
    if destination_wide.is_empty() || destination_wide.contains(&0) {
        anyhow::bail!(
            "Windows rename destination is empty or contains a NUL character: {}",
            destination.display()
        );
    }
    let name_bytes = destination_wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .context("Windows rename destination length overflow")?;
    let name_bytes_u32 = u32::try_from(name_bytes)
        .context("Windows rename destination exceeds the platform length limit")?;
    let buffer_bytes = std::mem::size_of::<FILE_RENAME_INFO>()
        .checked_add(name_bytes)
        .context("Windows rename information length overflow")?;
    let buffer_bytes_u32 = u32::try_from(buffer_bytes)
        .context("Windows rename information exceeds the platform length limit")?;
    let word_bytes = std::mem::size_of::<usize>();
    let mut storage = vec![0_usize; buffer_bytes.div_ceil(word_bytes)];
    let rename_info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        // SAFETY: `storage` is word-aligned and large enough for the fixed
        // header plus every UTF-16 code unit copied into the flexible tail.
        (*rename_info).Anonymous.ReplaceIfExists = false;
        (*rename_info).RootDirectory = std::ptr::null_mut();
        (*rename_info).FileNameLength = name_bytes_u32;
        std::ptr::copy_nonoverlapping(
            destination_wide.as_ptr(),
            std::ptr::addr_of_mut!((*rename_info).FileName).cast::<u16>(),
            destination_wide.len(),
        );
    }
    let result = unsafe {
        // SAFETY: `source_directory` owns the DELETE-capable pinned directory
        // handle, and `rename_info` points into the live, correctly sized
        // aligned buffer initialized above. ReplaceIfExists is false.
        SetFileInformationByHandle(
            source_directory.file().as_raw_handle(),
            FileRenameInfo,
            rename_info.cast(),
            buffer_bytes_u32,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .context("renaming the exact Windows directory handle without replacement");
    }
    Ok(DirectoryRenameOutcome::DurablyRenamed)
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
        CreateExactOutcome, DirectoryRenameOutcome, RemoveExactOutcome, create_direct_tempdir,
        create_exclusive_copy, remove_exact, rename_directory_noreplace_durable,
        rename_directory_noreplace_durable_with_hook, token_for_path,
    };
    use std::fs;

    #[cfg(unix)]
    #[test]
    fn direct_tempdir_is_created_below_the_physical_parent() {
        use std::os::unix::fs::symlink;

        let fixture = create_direct_tempdir().expect("create temporary-root fixture");
        let physical_parent = fixture.path().join("physical");
        let aliased_parent = fixture.path().join("alias");
        fs::create_dir(&physical_parent).expect("create physical temporary parent");
        symlink(&physical_parent, &aliased_parent).expect("create temporary parent alias");

        let aliased_directory = tempfile::Builder::new()
            .tempdir_in(&aliased_parent)
            .expect("create control directory through alias");
        assert!(
            super::open_direct_directory(aliased_directory.path()).is_err(),
            "the transaction path policy must continue rejecting the aliased control path"
        );

        let physical_parent =
            fs::canonicalize(&physical_parent).expect("resolve expected physical temporary parent");
        let directory = super::create_direct_tempdir_in(&aliased_parent)
            .expect("create direct temporary directory through an aliased root");

        assert_eq!(directory.path().parent(), Some(physical_parent.as_path()));
        super::open_direct_directory(directory.path())
            .expect("created temporary directory must be directly openable");
    }

    #[test]
    fn durable_directory_rename_supports_cross_parent_no_replace_moves() {
        let root = create_direct_tempdir().unwrap();
        let source_parent = root.path().join("source-parent");
        let destination_parent = root.path().join("destination-parent");
        fs::create_dir_all(source_parent.join("profile")).unwrap();
        fs::create_dir(&destination_parent).unwrap();
        fs::write(source_parent.join("profile/auth.json"), b"credential").unwrap();
        let source = source_parent.join("profile");
        let destination = destination_parent.join("archived");

        let outcome = rename_directory_noreplace_durable(&source, &destination).unwrap();
        assert!(matches!(
            outcome,
            DirectoryRenameOutcome::DurablyRenamed
                | DirectoryRenameOutcome::VisibleDurabilityUnconfirmed { .. }
        ));
        assert!(!source.exists());
        assert_eq!(
            fs::read(destination.join("auth.json")).unwrap(),
            b"credential"
        );

        fs::create_dir_all(source.join("nested")).unwrap();
        let error = rename_directory_noreplace_durable(&source, &destination)
            .expect_err("a durable rename must never replace an existing directory");
        assert!(!error.to_string().is_empty());
        assert!(source.join("nested").exists());
        assert_eq!(
            fs::read(destination.join("auth.json")).unwrap(),
            b"credential"
        );
    }

    #[cfg(unix)]
    #[test]
    fn durable_directory_rename_never_follows_a_source_symlink() {
        use std::os::unix::fs::symlink;

        let root = create_direct_tempdir().unwrap();
        let original = root.path().join("original");
        let source = root.path().join("source-link");
        let destination = root.path().join("destination");
        fs::create_dir(&original).unwrap();
        fs::write(original.join("auth.json"), b"original").unwrap();
        symlink(&original, &source).unwrap();

        let error = rename_directory_noreplace_durable(&source, &destination)
            .expect_err("the source bind must reject a final symlink atomically");

        assert!(!destination.exists(), "the symlink itself was moved");
        assert_eq!(fs::read(original.join("auth.json")).unwrap(), b"original");
        assert!(
            format!("{error:#}").contains("opening direct transaction directory component"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn durable_directory_rename_rejects_a_source_swapped_after_binding() {
        let root = create_direct_tempdir().unwrap();
        let source_parent = root.path().join("source-parent");
        let destination_parent = root.path().join("destination-parent");
        let source = source_parent.join("profile");
        let parked_source = source_parent.join("parked-profile");
        let replacement = source_parent.join("replacement");
        let destination = destination_parent.join("archived");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&replacement).unwrap();
        fs::create_dir(&destination_parent).unwrap();
        fs::write(source.join("auth.json"), b"original").unwrap();
        fs::write(replacement.join("auth.json"), b"replacement").unwrap();

        let error = rename_directory_noreplace_durable_with_hook(&source, &destination, || {
            fs::rename(&source, &parked_source).unwrap();
            fs::rename(&replacement, &source).unwrap();
        })
        .expect_err("a path-swapped source must never be reported as the bound directory");

        assert!(
            format!("{error:#}").contains("source directory changed during exact rename"),
            "{error:#}"
        );
        assert_eq!(
            fs::read(parked_source.join("auth.json")).unwrap(),
            b"original"
        );
        assert_eq!(fs::read(source.join("auth.json")).unwrap(), b"replacement");
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn durable_directory_rename_restores_a_symlink_swapped_after_binding() {
        use std::os::unix::fs::symlink;

        let root = create_direct_tempdir().unwrap();
        let source_parent = root.path().join("source-parent");
        let destination_parent = root.path().join("destination-parent");
        let source = source_parent.join("profile");
        let parked_source = source_parent.join("parked-profile");
        let destination = destination_parent.join("archived");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir(&destination_parent).unwrap();
        fs::write(source.join("auth.json"), b"original").unwrap();

        let error = rename_directory_noreplace_durable_with_hook(&source, &destination, || {
            fs::rename(&source, &parked_source).unwrap();
            symlink(&parked_source, &source).unwrap();
        })
        .expect_err("a symlink swapped after binding must never be committed as the profile");

        assert!(
            format!("{error:#}").contains("changed into a non-direct directory"),
            "{error:#}"
        );
        assert!(
            fs::symlink_metadata(&source)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!destination.exists());
        assert_eq!(
            fs::read(parked_source.join("auth.json")).unwrap(),
            b"original"
        );
    }

    #[cfg(windows)]
    #[test]
    fn durable_directory_rename_pins_the_source_namespace_until_handle_rename() {
        let root = create_direct_tempdir().unwrap();
        let source_parent = root.path().join("source-parent");
        let destination_parent = root.path().join("destination-parent");
        let source = source_parent.join("profile");
        let parked_source = source_parent.join("parked-profile");
        let destination = destination_parent.join("archived");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir(&destination_parent).unwrap();
        fs::write(source.join("auth.json"), b"original").unwrap();

        let outcome = rename_directory_noreplace_durable_with_hook(&source, &destination, || {
            fs::rename(&source, &parked_source)
                .expect_err("the pinned source must reject a namespace swap");
        })
        .unwrap();

        assert!(matches!(outcome, DirectoryRenameOutcome::DurablyRenamed));
        assert!(!source.exists());
        assert!(!parked_source.exists());
        assert_eq!(
            fs::read(destination.join("auth.json")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn exclusive_copy_and_exact_removal_preserve_the_bound_bytes() {
        let directory = create_direct_tempdir().unwrap();
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
        let directory = create_direct_tempdir().unwrap();
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
        let directory = create_direct_tempdir().unwrap();
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
        let directory = create_direct_tempdir().unwrap();
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
        let directory = create_direct_tempdir().unwrap();
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
