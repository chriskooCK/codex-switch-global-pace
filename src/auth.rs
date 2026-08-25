use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rand::Rng as _;

use crate::error::CsError;

const MAX_BACKUPS: usize = 3;
const AUTH_PUBLICATION_RECORD_VERSION: u8 = 1;
const MAX_AUTH_PUBLICATION_RECORD_BYTES: usize = 64 * 1024;

pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Upstream Codex version this release is contract-aligned with.
pub(crate) const ALIGNED_CODEX_VERSION: &str = "0.144.1";

/// User-Agent in the upstream shape: `codex_cli_rs/<version> (<os>; <arch>)`.
pub(crate) fn codex_user_agent() -> String {
    format!(
        "codex_cli_rs/{ALIGNED_CODEX_VERSION} ({}; {})",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}
pub(crate) const ISSUER: &str = "https://auth.openai.com";
const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

pub(crate) fn token_url() -> String {
    std::env::var("CS_TOKEN_URL").unwrap_or_else(|_| DEFAULT_TOKEN_URL.to_string())
}

/// Serializes tests that redirect endpoint URLs (`CS_TOKEN_URL`, and the
/// warmup equivalents) at a mock server. Environment variables are
/// process-global, so a per-module lock only serializes that module and lets
/// tests in a sibling module retarget the variable mid-request; both modules
/// must take this one. Mirrors `profile::TEST_ENV_LOCK`, which does the same
/// for the `HOME` / `CODEX_HOME` group.
#[cfg(test)]
pub(crate) static URL_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// ~/.codex/auth.json (or $CODEX_HOME/auth.json)
pub fn codex_auth_path() -> Result<PathBuf> {
    let codex_home = codex_home_from_values(std::env::var_os("CODEX_HOME"), dirs::home_dir())?;
    validate_cli_auth_credentials_store(&codex_home)?;
    Ok(codex_home.join("auth.json"))
}

pub(crate) fn ensure_file_credentials_store() -> Result<()> {
    let codex_home = codex_home_from_values(std::env::var_os("CODEX_HOME"), dirs::home_dir())?;
    validate_cli_auth_credentials_store(&codex_home)
}

fn codex_home_from_values(
    configured_home: Option<OsString>,
    user_home: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(home) = configured_home.filter(|value| !value.is_empty()) {
        return configured_home_path("CODEX_HOME", home);
    }

    let home = user_home.ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    let home = validate_home_path("user home directory", home)?;
    validate_home_path("default CODEX_HOME", home.join(".codex"))
}

fn configured_home_path(name: &str, value: OsString) -> Result<PathBuf> {
    validate_home_path(name, PathBuf::from(value))
}

fn validate_home_path(name: &str, path: PathBuf) -> Result<PathBuf> {
    if !path.is_absolute() {
        anyhow::bail!("{name} must be an absolute path: {}", path.display());
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        anyhow::bail!(
            "{name} contains '..' component which is not allowed: {}",
            path.display()
        );
    }
    if path.parent().is_none() {
        anyhow::bail!("{name} cannot be a filesystem root: {}", path.display());
    }
    validate_existing_directory_components(name, &path)?;
    Ok(path)
}

/// Reject an existing link/reparse point anywhere in a private state path.
///
/// `symlink_metadata(path)` only avoids following the final component. It
/// still follows links in every parent component, so checking the nearest
/// existing descendant alone would let `link/existing-child` hide `link`.
/// Walk from the filesystem root toward the leaf and inspect every component
/// in its own right instead.
fn validate_existing_directory_components(name: &str, path: &Path) -> Result<()> {
    let mut found_existing = false;
    #[cfg(unix)]
    let mut parent: Option<(PathBuf, std::fs::Metadata)> = None;
    #[cfg(unix)]
    let mut encountered_missing = false;
    for component in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match std::fs::symlink_metadata(component) {
            Ok(metadata) => {
                found_existing = true;
                if metadata_is_link_or_reparse(&metadata) {
                    anyhow::bail!(
                        "{name} contains a symlink, junction, or reparse-point component: {}",
                        component.display()
                    );
                }
                if !metadata.file_type().is_dir() {
                    anyhow::bail!(
                        "{name} contains an existing non-directory component: {}",
                        component.display()
                    );
                }
                #[cfg(unix)]
                {
                    validate_unix_directory_component(name, component, &metadata, parent.as_ref())?;
                    parent = Some((component.to_path_buf(), metadata));
                }
            }
            Err(error) if metadata_lookup_requires_parent(&error) => {
                #[cfg(unix)]
                {
                    encountered_missing = true;
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspecting {name} component {}", component.display())
                });
            }
        }
    }
    if !found_existing {
        anyhow::bail!(
            "{name} has no existing directory ancestor: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    if encountered_missing && let Some((existing_parent, metadata)) = parent {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.mode() & 0o022 != 0 && !unix_mode_is_sticky(metadata.mode()) {
            anyhow::bail!(
                "{name} would create a private entry in a group- or other-writable non-sticky directory: {}",
                existing_parent.display()
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_unix_directory_component(
    name: &str,
    component: &Path,
    metadata: &std::fs::Metadata,
    parent: Option<&(PathBuf, std::fs::Metadata)>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let effective_uid = unsafe { libc::geteuid() };
    let trusted_owner = metadata.uid() == 0 || metadata.uid() == effective_uid;

    if let Some((parent_path, parent_metadata)) = parent {
        let parent_mode = parent_metadata.mode();
        if parent_mode & 0o022 != 0 {
            if !unix_mode_is_sticky(parent_mode) {
                anyhow::bail!(
                    "{name} crosses a group- or other-writable non-sticky directory that can replace private path entries: {}",
                    parent_path.display()
                );
            }
            // Sticky directories such as /tmp protect an entry only from users
            // who own neither the directory nor that entry. A child owned by
            // somebody else remains replaceable by its owner.
            if !trusted_owner {
                anyhow::bail!(
                    "{name} contains an entry another user can replace in sticky directory {}: {}",
                    parent_path.display(),
                    component.display()
                );
            }
        }
    }

    // A directory owner can make an apparently read-only directory writable
    // after this check. Only root and this effective user are trusted to keep
    // the path stable for the duration of a private write.
    if !trusted_owner {
        anyhow::bail!(
            "{name} contains a directory owned by another user and cannot be used for private state: {} (uid {})",
            component.display(),
            metadata.uid()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn unix_mode_is_sticky(mode: u32) -> bool {
    u64::from(mode) & u64::from(libc::S_ISVTX) != 0
}

fn metadata_lookup_requires_parent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

fn metadata_is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn app_home_from_values(
    configured_home: Option<OsString>,
    user_home: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(path) = configured_home.filter(|value| !value.is_empty()) {
        return configured_home_path("CODEX_SWITCH_HOME", path);
    }

    let home = user_home.ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    let home = validate_home_path("user home directory", home)?;
    validate_home_path("default CODEX_SWITCH_HOME", home.join(".codex-switch"))
}

fn validate_cli_auth_credentials_store(codex_home: &Path) -> Result<()> {
    let Some((config_path, config)) = load_codex_config(codex_home)? else {
        return Ok(());
    };

    match config.get("cli_auth_credentials_store") {
        None => {}
        Some(toml::Value::String(mode)) if mode == "file" => {}
        Some(_) => anyhow::bail!(
            "codex-switch-global-pace requires file-based Codex credentials; set \
             cli_auth_credentials_store = \"file\" in {}",
            config_path.display()
        ),
    }

    if config.get("forced_login_method").and_then(|v| v.as_str()) == Some("api") {
        anyhow::bail!(
            "Codex managed policy requires API key login, but codex-switch-global-pace requires ChatGPT OAuth"
        );
    }
    Ok(())
}

fn load_codex_config(codex_home: &Path) -> Result<Option<(PathBuf, toml::Value)>> {
    let config_path = codex_home.join("config.toml");
    let raw = match std::fs::read_to_string(&config_path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("reading {}", config_path.display()));
        }
    };
    let config =
        toml::from_str(&raw).with_context(|| format!("parsing {}", config_path.display()))?;
    Ok(Some((config_path, config)))
}

fn validate_managed_auth_config(config: &toml::Value, account_id: Option<&str>) -> Result<()> {
    if config.get("forced_login_method").and_then(|v| v.as_str()) == Some("api") {
        anyhow::bail!(
            "Codex managed policy requires API key login, but codex-switch-global-pace requires ChatGPT OAuth"
        );
    }

    let workspace_ids = forced_chatgpt_workspace_ids(config)?;
    if workspace_ids.is_empty() {
        return Ok(());
    }

    let account_id = account_id.ok_or_else(|| {
        anyhow::anyhow!("login token has no workspace id required by Codex managed policy")
    })?;
    if !workspace_ids.iter().any(|id| id == account_id) {
        anyhow::bail!(
            "workspace {account_id} is not allowed by Codex forced_chatgpt_workspace_id policy"
        );
    }
    Ok(())
}

fn forced_chatgpt_workspace_ids(config: &toml::Value) -> Result<Vec<String>> {
    let workspace_ids: Vec<&str> = match config.get("forced_chatgpt_workspace_id") {
        None => Vec::new(),
        Some(toml::Value::String(id)) => vec![id.trim()],
        Some(toml::Value::Array(ids)) => ids
            .iter()
            .map(|value| {
                value.as_str().ok_or_else(|| {
                    anyhow::anyhow!("forced_chatgpt_workspace_id must contain only strings")
                })
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(str::trim)
            .collect(),
        Some(_) => {
            anyhow::bail!("forced_chatgpt_workspace_id must be a string or a list of strings")
        }
    };
    Ok(workspace_ids
        .into_iter()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect())
}

/// Workspace ids forced by Codex managed config — best-effort, empty when
/// unset or unreadable. Used to pre-restrict the OAuth authorize page the
/// same way Codex does via `allowed_workspace_id`.
pub(crate) fn configured_forced_workspace_ids() -> Vec<String> {
    let Ok(codex_home) = codex_home_from_values(std::env::var_os("CODEX_HOME"), dirs::home_dir())
    else {
        return Vec::new();
    };
    let Ok(Some((_path, config))) = load_codex_config(&codex_home) else {
        return Vec::new();
    };
    forced_chatgpt_workspace_ids(&config).unwrap_or_default()
}

pub(crate) fn validate_managed_chatgpt_account(id_token: &str) -> Result<()> {
    let codex_home = codex_home_from_values(std::env::var_os("CODEX_HOME"), dirs::home_dir())?;
    let Some((_config_path, config)) = load_codex_config(&codex_home)? else {
        return Ok(());
    };
    let auth = serde_json::json!({"tokens": {"id_token": id_token}});
    let account_id = crate::jwt::parse_account_info(&auth).account_id;
    validate_managed_auth_config(&config, account_id.as_deref())
}

/// Enforce the managed ChatGPT workspace policy for a complete auth value.
/// Keep this at credential-write boundaries: JWT claims are only a routing
/// hint until a caller has otherwise authenticated the credentials.
pub(crate) fn validate_managed_auth_value(auth: &serde_json::Value) -> Result<()> {
    let codex_home = codex_home_from_values(std::env::var_os("CODEX_HOME"), dirs::home_dir())?;
    let Some((_config_path, config)) = load_codex_config(&codex_home)? else {
        return Ok(());
    };
    let account_id = crate::jwt::parse_account_info(auth).account_id;
    validate_managed_auth_config(&config, account_id.as_deref())
}

/// ~/.codex-switch/
pub fn app_home() -> Result<PathBuf> {
    // Keep application state relocatable without changing Codex's own home.
    app_home_from_values(std::env::var_os("CODEX_SWITCH_HOME"), dirs::home_dir())
}

/// ~/.codex-switch/profiles/
pub fn profiles_dir() -> Result<PathBuf> {
    Ok(app_home()?.join("profiles"))
}

/// ~/.codex-switch/current
pub fn current_file() -> Result<PathBuf> {
    Ok(app_home()?.join("current"))
}

pub fn read_auth(path: &Path) -> Result<serde_json::Value> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CsError::NoAuthFile(path.display().to_string()).into());
        }
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let val: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(val)
}

/// Result of publishing a same-directory private-file candidate.
///
/// Once [`VisibleDurabilityUnconfirmed`](Self::VisibleDurabilityUnconfirmed)
/// is returned, `path` already names the new bytes. Callers must not retry as
/// though nothing was written: doing so can overwrite a credential that is
/// now the only valid one. On Unix, `DurablyPublished` includes a successful
/// parent-directory sync; on Windows it means the supported atomic namespace
/// primitive completed and the already-synced candidate is visible.
#[derive(Debug)]
#[must_use = "a visible-but-not-durably-confirmed write must be handled explicitly"]
#[cfg_attr(windows, allow(dead_code))] // The partial variant is exercised by Unix parent fsync and tests.
pub enum PrivateWriteOutcome {
    DurablyPublished,
    VisibleDurabilityUnconfirmed { cause: anyhow::Error },
}

#[cfg(test)]
impl PrivateWriteOutcome {
    pub(crate) fn assert_durably_published(self) {
        match self {
            Self::DurablyPublished => {}
            Self::VisibleDurabilityUnconfirmed { cause } => {
                panic!("test fixture was visible but not durably published: {cause:#}")
            }
        }
    }
}

/// Typed error used when a caller requires a durable commit after publication.
/// The wording deliberately records that the destination is already visible.
#[derive(Debug, thiserror::Error)]
#[error(
    "{description} is visible at {path}, but its directory durability could not be confirmed: {source:#}"
)]
pub(crate) struct VisiblePrivateWrite {
    description: &'static str,
    path: PathBuf,
    #[source]
    source: anyhow::Error,
}

pub(crate) fn require_durable_private_write(
    path: &Path,
    description: &'static str,
    outcome: PrivateWriteOutcome,
) -> Result<()> {
    match outcome {
        PrivateWriteOutcome::DurablyPublished => Ok(()),
        PrivateWriteOutcome::VisibleDurabilityUnconfirmed { cause } => Err(VisiblePrivateWrite {
            description,
            path: path.to_path_buf(),
            source: cause,
        }
        .into()),
    }
}

#[cfg(test)]
thread_local! {
    static TEST_PRIVATE_DURABILITY_FAILURE_COUNTDOWN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn fail_next_private_durability_confirmation() {
    fail_private_durability_confirmation_after(0);
}

#[cfg(test)]
pub(crate) fn fail_private_durability_confirmation_after(successful_publications: usize) {
    TEST_PRIVATE_DURABILITY_FAILURE_COUNTDOWN.with(|countdown| {
        countdown.set(
            successful_publications
                .checked_add(1)
                .expect("test durability-failure countdown overflow"),
        );
    });
}

pub(crate) fn atomic_write_private(path: &Path, contents: &[u8]) -> Result<PrivateWriteOutcome> {
    let staged = stage_private_file(path, contents)?;
    let StagedPrivateFile {
        file: tmp,
        _directory_guard,
    } = staged;
    tmp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("atomically replacing {}", path.display()))?;

    // `persist` is the publication boundary. Every error returned above means
    // the destination was not replaced. Work after this point must not turn an
    // already-visible credential into a misleading "not saved" result.
    verify_private_permissions_after_publication(path, "atomically saved");
    private_publication_outcome(path)
}

struct StagedPrivateFile {
    file: tempfile::NamedTempFile,
    _directory_guard: PrivateDirectoryGuard,
}

fn stage_private_file(path: &Path, contents: &[u8]) -> Result<StagedPrivateFile> {
    let parent = private_write_parent(path)?;
    let directory_guard = acquire_private_directory(parent)?;

    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary file in {}", parent.display()))?;
    #[cfg(windows)]
    harden_windows_acl(tmp.path(), false)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", tmp.path().display()))?;
    }
    tmp.write_all(contents)
        .with_context(|| format!("writing temporary file for {}", path.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("syncing temporary file for {}", path.display()))?;
    Ok(StagedPrivateFile {
        file: tmp,
        _directory_guard: directory_guard,
    })
}

/// Capability proving that `path` is a direct private directory for the
/// lifetime of a path-based operation. On Windows the capability owns a handle
/// for every component and denies delete sharing, so neither the directory nor
/// an ancestor can be renamed after validation. Unix instead proves a durable
/// ownership/mode invariant that an unrelated user cannot change.
#[derive(Debug)]
pub(crate) struct PrivateDirectoryGuard {
    #[cfg(windows)]
    _handles: Vec<std::fs::File>,
    #[cfg(not(windows))]
    _private: (),
}

pub(crate) fn ensure_private_directory(path: &Path) -> Result<()> {
    acquire_private_directory(path).map(drop)
}

pub(crate) fn acquire_private_directory(path: &Path) -> Result<PrivateDirectoryGuard> {
    if !path.is_absolute() {
        anyhow::bail!(
            "private directory path must be absolute: {}",
            path.display()
        );
    }
    if path.parent().is_none() {
        anyhow::bail!(
            "private directory path cannot be a filesystem root: {}",
            path.display()
        );
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        anyhow::bail!(
            "private directory path contains '..' component: {}",
            path.display()
        );
    }
    #[cfg(windows)]
    {
        acquire_windows_private_directory(path)
    }
    #[cfg(unix)]
    {
        prepare_unix_private_directory(path)?;
        Ok(PrivateDirectoryGuard { _private: () })
    }
    #[cfg(not(any(unix, windows)))]
    {
        anyhow::bail!(
            "private directory security is unsupported on this platform: {}",
            path.display()
        )
    }
}

#[cfg(unix)]
/// Create a private directory tree without leaving newly-created ancestor
/// entries outside the durability contract used by private-file publication.
///
/// `create_dir_all` followed by syncing only the final directory is
/// insufficient on Unix: after a crash, the final directory can survive while
/// its name in a newly-created parent does not. Create each missing component
/// in order, sync that directory's inode, then sync the parent entry before
/// proceeding to the next component.
fn prepare_unix_private_directory(path: &Path) -> Result<()> {
    validate_existing_directory_components("private directory path", path)?;
    let mut missing = Vec::new();
    let mut cursor = path;
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(metadata) => {
                if metadata_is_link_or_reparse(&metadata) || !metadata.file_type().is_dir() {
                    anyhow::bail!(
                        "private directory path is not an ordinary directory: {}",
                        cursor.display()
                    );
                }
                break;
            }
            Err(error) if metadata_lookup_requires_parent(&error) => {
                missing.push(cursor.to_path_buf());
                cursor = cursor.parent().ok_or_else(|| {
                    anyhow::anyhow!(
                        "private directory path has no existing ancestor: {}",
                        path.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting private directory {}", cursor.display()));
            }
        }
    }

    for directory in missing.iter().rev() {
        match std::fs::create_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(directory).with_context(|| {
                    format!(
                        "checking concurrently-created directory {}",
                        directory.display()
                    )
                })?;
                if metadata_is_link_or_reparse(&metadata) || !metadata.file_type().is_dir() {
                    anyhow::bail!(
                        "private directory path was concurrently occupied by a non-directory or reparse point: {}",
                        directory.display()
                    );
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating directory {}", directory.display()));
            }
        }
        // A successful mkdir in a protected parent creates a current-user
        // entry. If another process won the name, the full chain validation
        // rejects an untrusted owner before chmod follows that path.
        validate_existing_directory_components("private directory path", directory)?;
        secure_private_directory(directory)?;
        validate_existing_directory_components("private directory path", directory)?;
        std::fs::File::open(directory)
            .with_context(|| format!("opening new directory {} for sync", directory.display()))?
            .sync_all()
            .with_context(|| format!("syncing new directory {}", directory.display()))?;
        crate::fs_ops::sync_parent(directory).with_context(|| {
            format!(
                "syncing parent after creating private directory {}",
                directory.display()
            )
        })?;
    }

    if missing.is_empty() {
        secure_private_directory(path)?;
    }
    validate_private_directory_owner(path)?;
    validate_existing_directory_components("private directory path", path)?;
    Ok(())
}

#[cfg(unix)]
fn validate_private_directory_owner(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("identifying private directory {}", path.display()))?;
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        anyhow::bail!(
            "private directory must be owned by the effective user (uid {effective_uid}): {} is owned by uid {}",
            path.display(),
            metadata.uid()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn acquire_windows_private_directory(path: &Path) -> Result<PrivateDirectoryGuard> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

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
                    "private directory path contains a relative component: {}",
                    path.display()
                );
            }
        }
    }
    if prefixes.is_empty() {
        anyhow::bail!(
            "private directory path has no openable component: {}",
            path.display()
        );
    }

    let final_index = prefixes.len() - 1;
    let mut private_tree_started = false;
    let mut handles = Vec::with_capacity(prefixes.len());
    for (index, component_path) in prefixes.into_iter().enumerate() {
        let open = || {
            let mut options = std::fs::OpenOptions::new();
            options
                .access_mode(FILE_READ_ATTRIBUTES)
                // Delete sharing is deliberately absent: every already-opened
                // ancestor remains a stable namespace component while the next
                // child is opened or securely created.
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
            options.open(&component_path)
        };
        let directory = match open() {
            Ok(directory) => directory,
            Err(error)
                if index != 0
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
            {
                private_tree_started = true;
                match create_windows_private_directory(&component_path) {
                    Ok(()) => {}
                    Err(create_error)
                        if create_error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(create_error) => {
                        return Err(create_error).with_context(|| {
                            format!(
                                "securely creating private directory {}",
                                component_path.display()
                            )
                        });
                    }
                }
                open().with_context(|| {
                    format!(
                        "opening securely-created private directory {}",
                        component_path.display()
                    )
                })?
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "opening and pinning private Windows directory component {}",
                        component_path.display()
                    )
                });
            }
        };
        let metadata = directory.metadata().with_context(|| {
            format!(
                "identifying pinned private Windows directory component {}",
                component_path.display()
            )
        })?;
        if !metadata.file_type().is_dir()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            anyhow::bail!(
                "private Windows path component is not a direct directory: {}",
                component_path.display()
            );
        }
        handles.push(directory);

        // Existing ancestors may be shared, but their pinned handles make them
        // immutable for this operation. The final private directory and every
        // component created for it must itself belong to this user before its
        // protected ACL is installed; a hostile pre-creation therefore fails
        // closed instead of being adopted.
        if private_tree_started || index == final_index {
            require_windows_path_owner(&component_path, "private directory")?;
            harden_windows_acl(&component_path, true)?;
        }
    }

    Ok(PrivateDirectoryGuard { _handles: handles })
}

#[cfg(windows)]
fn create_windows_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

    struct Descriptor(*mut core::ffi::c_void);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    let current_user_sid = windows_current_user_sid_string(path)
        .map_err(|error| std::io::Error::other(format!("{error:#}")))?;
    let sddl = windows_private_acl_sddl(&current_user_sid, true);
    let sddl_wide: Vec<u16> = std::ffi::OsStr::new(&sddl)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut security_descriptor: PSECURITY_DESCRIPTOR = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            SDDL_REVISION_1,
            &mut security_descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let _descriptor = Descriptor(security_descriptor);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security_descriptor,
        bInheritHandle: 0,
    };
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn secure_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("setting permissions on {}", path.display()))?;
    Ok(())
}

fn private_write_parent(path: &Path) -> Result<&Path> {
    path.parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))
}

fn verify_private_permissions_after_publication(_path: &Path, _description: &str) {
    #[cfg(windows)]
    if let Err(error) = harden_windows_acl(_path, false) {
        // The temporary file was hardened before its same-directory rename, so
        // this is a post-publish verification/repair failure, not a failed save.
        tracing::error!(
            "{} was {_description}, but post-publish ACL verification failed: {error:#}",
            _path.display(),
        );
    }
}

fn private_publication_outcome(_path: &Path) -> Result<PrivateWriteOutcome> {
    #[cfg(test)]
    if TEST_PRIVATE_DURABILITY_FAILURE_COUNTDOWN.with(|countdown| {
        let remaining = countdown.get();
        if remaining == 0 {
            return false;
        }
        countdown.set(remaining - 1);
        remaining == 1
    }) {
        return Ok(PrivateWriteOutcome::VisibleDurabilityUnconfirmed {
            cause: anyhow::anyhow!("injected parent-directory durability failure"),
        });
    }

    #[cfg(unix)]
    if let Err(cause) = confirm_namespace_durability(_path) {
        return Ok(PrivateWriteOutcome::VisibleDurabilityUnconfirmed { cause });
    }

    Ok(PrivateWriteOutcome::DurablyPublished)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConditionalWrite {
    Written,
    Changed,
    /// The candidate is definitely public, but a durable recovery artifact
    /// could not be finalized. The caller must not claim that live auth stayed
    /// unchanged or write the derived activation marker.
    PublishedRecoveryRequired(String),
    /// The foreign live writer was restored, but private transaction cleanup
    /// is incomplete. No further credential publication is permitted until the
    /// recorded state is reconciled.
    RestoredRecoveryRequired(String),
    /// Namespace state no longer proves either a completed publication or a
    /// completed restoration. Every observed file is preserved for recovery.
    AmbiguousRecoveryRequired(String),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthPublicationRecord {
    version: u8,
    nonce: String,
    backup_stamp: String,
    expected_token: String,
    candidate_token: String,
    backup_token: String,
}

struct ParsedAuthPublication {
    candidate: PathBuf,
    displaced: PathBuf,
    backup_stage: PathBuf,
    backup: PathBuf,
    record: PathBuf,
    expected_token: crate::fs_ops::FileToken,
    candidate_token: crate::fs_ops::FileToken,
    backup_token: crate::fs_ops::FileToken,
    record_token: crate::fs_ops::FileToken,
}

/// A private same-directory file whose deletion remains bound to the exact
/// inode/file-id and digest created by this process. A later writer that takes
/// over the path is always preserved.
struct ExactPrivateFile {
    path: PathBuf,
    token: crate::fs_ops::FileToken,
    cleanup_on_drop: bool,
    _directory_guard: PrivateDirectoryGuard,
}

impl ExactPrivateFile {
    fn path(&self) -> &Path {
        &self.path
    }

    fn token(&self) -> &crate::fs_ops::FileToken {
        &self.token
    }

    fn disarm(&mut self) {
        self.cleanup_on_drop = false;
    }
}

impl Drop for ExactPrivateFile {
    fn drop(&mut self) {
        if !self.cleanup_on_drop {
            return;
        }
        if let Err(error) = remove_bound_path(&self.path, &self.token) {
            tracing::warn!(
                "private transaction file {} was preserved because exact cleanup could not be proven: {error:#}",
                self.path.display()
            );
        }
    }
}

#[derive(Debug)]
enum PublicationSettlement {
    Published,
    LiveUnchanged,
    PublishedRecoveryRequired(String),
    RestoredRecoveryRequired(String),
    AmbiguousRecoveryRequired(String),
}

enum ExpectedInodeObservation {
    Unavailable,
    Unchanged,
    Changed(crate::fs_ops::FileToken),
    Unreadable(String),
}

fn stage_exact_private_file(path: &Path, contents: &[u8]) -> Result<ExactPrivateFile> {
    let staged = stage_private_file(path, contents)?;
    let StagedPrivateFile {
        mut file,
        _directory_guard,
    } = staged;
    let token = crate::fs_ops::token_for_file(file.as_file_mut())?;
    let (_file, staged_path) = file
        .keep()
        .map_err(|error| error.error)
        .with_context(|| format!("retaining private transaction file for {}", path.display()))?;
    Ok(ExactPrivateFile {
        path: staged_path,
        token,
        cleanup_on_drop: true,
        _directory_guard,
    })
}

pub(crate) fn remove_bound_path(path: &Path, expected: &crate::fs_ops::FileToken) -> Result<()> {
    let boundary = crate::fs_ops::remove_exact(path, expected);
    match crate::fs_ops::token_if_present(path)? {
        None => match boundary {
            Ok(crate::fs_ops::RemoveExactOutcome::Removed) => Ok(()),
            Ok(crate::fs_ops::RemoveExactOutcome::RemovedNamespaceDurabilityUnconfirmed) => {
                #[cfg(unix)]
                {
                    confirm_namespace_durability(path).with_context(|| {
                        format!(
                            "the exact file disappeared from {}, but its removal is not durably confirmed",
                            path.display()
                        )
                    })
                }
                #[cfg(windows)]
                {
                    anyhow::bail!(
                        "Windows exact removal at {} returned an unsupported Unix-only durability outcome",
                        path.display()
                    )
                }
            }
            Err(error) => Err(error).with_context(|| {
                format!(
                    "the exact path disappeared during failed token-bound cleanup at {}; recovery artifacts may remain",
                    path.display()
                )
            }),
        },
        Some(observed) if &observed == expected => Err(boundary.err().unwrap_or_else(|| {
            anyhow::anyhow!("exact file remained after a successful removal boundary")
        })),
        Some(observed) => anyhow::bail!(
            "refusing to remove replacement at {}; expected token {}, observed {}",
            path.display(),
            expected,
            observed
        ),
    }
}

fn auth_publication_record_path(path: &Path) -> Result<PathBuf> {
    let parent = private_write_parent(path)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("live auth path has no UTF-8 file name: {}", path.display()))?;
    Ok(parent.join(format!(".{name}.codex-switch-publication")))
}

fn transaction_nonce() -> String {
    let mut nonce = [0_u8; 16];
    rand::rng().fill_bytes(&mut nonce);
    hex::encode(nonce)
}

fn validate_transaction_nonce(nonce: &str) -> Result<()> {
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("auth publication record contains an invalid transaction nonce");
    }
    Ok(())
}

fn transaction_role_path(path: &Path, role: &str, nonce: &str) -> Result<PathBuf> {
    validate_transaction_nonce(nonce)?;
    if !matches!(role, "candidate" | "displaced" | "backup") {
        anyhow::bail!("invalid auth publication transaction role");
    }
    let parent = private_write_parent(path)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| {
            format!(
                "transaction path has no UTF-8 file name: {}",
                path.display()
            )
        })?;
    Ok(parent.join(format!(".{name}.codex-switch-{role}-{nonce}")))
}

fn backup_stamp() -> Result<String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos()
        .to_string())
}

fn backup_path_for_transaction(path: &Path, stamp: &str, nonce: &str) -> Result<PathBuf> {
    validate_transaction_nonce(nonce)?;
    validate_backup_stamp(stamp)?;
    let parent = private_write_parent(path)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("auth path has no UTF-8 file name: {}", path.display()))?;
    Ok(parent.join(format!("{name}.bak.{stamp}-{nonce}")))
}

fn validate_backup_stamp(stamp: &str) -> Result<u128> {
    if stamp.is_empty() || !stamp.bytes().all(|byte| byte.is_ascii_digit()) {
        anyhow::bail!("auth publication record contains an invalid backup timestamp");
    }
    let parsed = stamp
        .parse::<u128>()
        .context("auth publication record contains an invalid backup timestamp")?;
    if parsed.to_string() != stamp {
        anyhow::bail!("auth publication record contains a non-canonical backup timestamp");
    }
    Ok(parsed)
}

fn managed_backup_sort_key(stem: &str, name: &str) -> Option<(u128, String)> {
    let suffix = name.strip_prefix(stem)?.strip_prefix(".bak.")?;
    let (stamp, nonce) = suffix.split_once('-')?;
    let stamp = validate_backup_stamp(stamp).ok()?;
    validate_transaction_nonce(nonce).ok()?;
    Some((stamp, nonce.to_string()))
}

enum RecordPublication {
    Durable(crate::fs_ops::FileToken),
    VisibleNotDurable {
        token: crate::fs_ops::FileToken,
        error: anyhow::Error,
    },
}

#[cfg(unix)]
pub(crate) fn confirm_namespace_durability(path: &Path) -> Result<()> {
    crate::fs_ops::sync_parent(path)
        .with_context(|| format!("confirming transaction durability for {}", path.display()))
}

pub(crate) fn confirm_namespace_boundary(path: &Path, boundary: &Result<()>) -> Result<()> {
    #[cfg(unix)]
    {
        let _ = boundary;
        confirm_namespace_durability(path)
    }
    #[cfg(windows)]
    {
        match boundary {
            Ok(()) => Ok(()),
            Err(error) => anyhow::bail!(
                "Windows namespace primitive for {} reported failure despite a visible post-state: {error:#}",
                path.display()
            ),
        }
    }
}

fn move_owned_noreplace(file: &mut ExactPrivateFile, destination: &Path) -> Result<()> {
    let source = file.path.clone();
    let boundary = crate::fs_ops::rename_noreplace(&source, destination);
    let source_after = crate::fs_ops::token_if_present(&source)?;
    let destination_after = crate::fs_ops::token_if_present(destination)?;
    if destination_after.as_ref() == Some(file.token()) && source_after.is_none() {
        file.path = destination.to_path_buf();
        return confirm_namespace_boundary(destination, &boundary).or_else(|durability| {
            Err(boundary.err().unwrap_or(durability)).with_context(|| {
                format!(
                    "owned no-replace move became visible but was not durably confirmed at {}",
                    destination.display()
                )
            })
        });
    }
    if source_after.as_ref() == Some(file.token()) {
        return Err(boundary.err().unwrap_or_else(|| {
            anyhow::anyhow!(
                "no-replace destination {} was occupied concurrently",
                destination.display()
            )
        }));
    }
    file.disarm();
    Err(boundary.err().unwrap_or_else(|| {
        anyhow::anyhow!(
            "owned no-replace move left unclassified paths {} and {}",
            source.display(),
            destination.display()
        )
    }))
}

fn publish_record_exclusive(
    record_path: &Path,
    record: &AuthPublicationRecord,
) -> Result<RecordPublication> {
    let encoded = serde_json::to_vec(record)?;
    let mut staged = stage_exact_private_file(record_path, &encoded)?;
    let boundary = crate::fs_ops::rename_noreplace(staged.path(), record_path);
    let source_after = crate::fs_ops::token_if_present(staged.path())?;
    let destination_after = crate::fs_ops::token_if_present(record_path)?;
    if destination_after.as_ref() == Some(staged.token()) && source_after.is_none() {
        let token = staged.token().clone();
        staged.disarm();
        return match confirm_namespace_boundary(record_path, &boundary) {
            Ok(()) => Ok(RecordPublication::Durable(token)),
            Err(durability) => Ok(RecordPublication::VisibleNotDurable {
                token,
                error: boundary.err().unwrap_or(durability),
            }),
        };
    }
    if source_after.as_ref() == Some(staged.token()) {
        return Err(boundary.err().unwrap_or_else(|| {
            anyhow::anyhow!(
                "fixed auth publication record {} was claimed concurrently",
                record_path.display()
            )
        }));
    }
    staged.disarm();
    Err(boundary.err().unwrap_or_else(|| {
        anyhow::anyhow!(
            "auth publication record boundary left unclassified state at {}",
            record_path.display()
        )
    }))
}

fn read_publication_record(
    live_path: &Path,
) -> Result<Option<(AuthPublicationRecord, crate::fs_ops::FileToken)>> {
    let record_path = auth_publication_record_path(live_path)?;
    let mut file = match crate::fs_ops::open_direct_regular(&record_path) {
        Ok(file) => file,
        Err(error) if io_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => {
            return Ok(None);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("opening auth publication record {}", record_path.display())
            });
        }
    };
    let before = crate::fs_ops::token_for_file(&mut file)?;
    let mut raw = Vec::new();
    {
        let mut bounded = (&mut file).take((MAX_AUTH_PUBLICATION_RECORD_BYTES + 1) as u64);
        bounded.read_to_end(&mut raw).with_context(|| {
            format!("reading auth publication record {}", record_path.display())
        })?;
    }
    if raw.len() > MAX_AUTH_PUBLICATION_RECORD_BYTES {
        anyhow::bail!(
            "auth publication record exceeds the {}-byte schema limit; preserving {}",
            MAX_AUTH_PUBLICATION_RECORD_BYTES,
            record_path.display()
        );
    }
    let after = crate::fs_ops::token_for_file(&mut file)?;
    let path_after = crate::fs_ops::token_if_present(&record_path)?.with_context(|| {
        format!(
            "auth publication record disappeared while being read: {}",
            record_path.display()
        )
    })?;
    if before != after || after != path_after || !before.matches_bytes(&raw) {
        anyhow::bail!(
            "auth publication record changed while being read; preserving {}",
            record_path.display()
        );
    }
    let record: AuthPublicationRecord = serde_json::from_slice(&raw)
        .with_context(|| format!("parsing auth publication record {}", record_path.display()))?;
    Ok(Some((record, before)))
}

fn parse_publication_record(
    live_path: &Path,
    record: AuthPublicationRecord,
    record_token: crate::fs_ops::FileToken,
) -> Result<ParsedAuthPublication> {
    if record.version != AUTH_PUBLICATION_RECORD_VERSION {
        anyhow::bail!(
            "unsupported auth publication record version {}; preserving recovery state",
            record.version
        );
    }
    validate_transaction_nonce(&record.nonce)?;
    let candidate = transaction_role_path(live_path, "candidate", &record.nonce)?;
    #[cfg(unix)]
    let displaced = candidate.clone();
    #[cfg(windows)]
    let displaced = transaction_role_path(live_path, "displaced", &record.nonce)?;
    let backup_stage = transaction_role_path(live_path, "backup", &record.nonce)?;
    let backup = backup_path_for_transaction(live_path, &record.backup_stamp, &record.nonce)?;
    let record_path = auth_publication_record_path(live_path)?;
    if candidate == live_path
        || displaced == live_path
        || backup_stage == live_path
        || backup == live_path
        || candidate == record_path
        || displaced == record_path
        || backup_stage == record_path
        || backup == record_path
        || backup_stage == candidate
        || backup_stage == displaced
        || backup == candidate
        || backup == displaced
        || backup == backup_stage
    {
        anyhow::bail!("auth publication record contains overlapping transaction paths");
    }
    #[cfg(windows)]
    if candidate == displaced {
        anyhow::bail!("Windows auth publication record must use a separate displaced path");
    }
    #[cfg(unix)]
    if candidate != displaced {
        anyhow::bail!("Unix auth publication record must bind the exchange path as displaced");
    }
    Ok(ParsedAuthPublication {
        candidate,
        displaced,
        backup_stage,
        backup,
        record: record_path,
        expected_token: record
            .expected_token
            .parse()
            .context("parsing expected auth publication token")?,
        candidate_token: record
            .candidate_token
            .parse()
            .context("parsing candidate auth publication token")?,
        backup_token: record
            .backup_token
            .parse()
            .context("parsing backup auth publication token")?,
        record_token,
    })
}

fn harden_private_file(path: &Path) -> Result<()> {
    #[cfg(windows)]
    harden_windows_acl(path, false)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting private permissions on {}", path.display()))?;
        crate::fs_ops::sync_parent(path)?;
    }
    Ok(())
}

fn remove_publication_record(parsed: &ParsedAuthPublication) -> Result<()> {
    remove_bound_path(&parsed.record, &parsed.record_token)
}

#[cfg(unix)]
fn exchange_candidate_with_live(
    candidate: &Path,
    live_path: &Path,
    _displaced: &Path,
) -> Result<()> {
    crate::fs_ops::exchange(candidate, live_path)
}

#[cfg(windows)]
fn exchange_candidate_with_live(
    candidate: &Path,
    live_path: &Path,
    displaced: &Path,
) -> Result<()> {
    crate::fs_ops::replace_with_displaced(candidate, live_path, displaced)
}

#[cfg(unix)]
fn restore_displaced_to_live(_candidate: &Path, live_path: &Path, displaced: &Path) -> Result<()> {
    crate::fs_ops::exchange(displaced, live_path)
}

#[cfg(windows)]
fn restore_displaced_to_live(candidate: &Path, live_path: &Path, displaced: &Path) -> Result<()> {
    crate::fs_ops::replace_with_displaced(displaced, live_path, candidate)
}

fn publication_state_detail(live_path: &Path, parsed: &ParsedAuthPublication) -> String {
    format!(
        "live={}, candidate={}, displaced={}, backup-stage={}, backup={}, record={}",
        live_path.display(),
        parsed.candidate.display(),
        parsed.displaced.display(),
        parsed.backup_stage.display(),
        parsed.backup.display(),
        parsed.record.display()
    )
}

fn finalize_independent_backup(live_path: &Path, parsed: &ParsedAuthPublication) -> Result<bool> {
    if !parsed.backup_token.same_contents(&parsed.expected_token) {
        anyhow::bail!(
            "independent auth backup does not match the expected live credential; {}",
            publication_state_detail(live_path, parsed)
        );
    }

    #[cfg(unix)]
    confirm_namespace_durability(live_path)?;

    let displaced = crate::fs_ops::token_if_present(&parsed.displaced)?;
    match displaced.as_ref() {
        Some(token) if token == &parsed.expected_token => {
            remove_bound_path(&parsed.displaced, &parsed.expected_token).with_context(|| {
                format!(
                    "removing the exact displaced live credential at {}",
                    parsed.displaced.display()
                )
            })?;
        }
        None => {}
        Some(observed) => anyhow::bail!(
            "the actual displaced live credential changed after publication; expected {}, observed {}; preserving both it and the independent backup",
            parsed.expected_token,
            observed
        ),
    }

    let stage = crate::fs_ops::token_if_present(&parsed.backup_stage)?;
    let backup = crate::fs_ops::token_if_present(&parsed.backup)?;
    if stage.as_ref() == Some(&parsed.backup_token) && backup.is_none() {
        let boundary = crate::fs_ops::rename_noreplace(&parsed.backup_stage, &parsed.backup);
        let stage_after = crate::fs_ops::token_if_present(&parsed.backup_stage)?;
        let backup_after = crate::fs_ops::token_if_present(&parsed.backup)?;
        if backup_after.as_ref() != Some(&parsed.backup_token) || stage_after.is_some() {
            return Err(boundary.err().unwrap_or_else(|| {
                anyhow::anyhow!(
                    "independent backup promotion left unclassified state; {}",
                    publication_state_detail(live_path, parsed)
                )
            }));
        }
        confirm_namespace_boundary(&parsed.backup, &boundary).with_context(|| {
            format!(
                "the independent backup is visible at {}, but its namespace commit is not durable",
                parsed.backup.display()
            )
        })?;
    } else if stage.is_none() && backup.as_ref() == Some(&parsed.backup_token) {
        #[cfg(unix)]
        confirm_namespace_durability(&parsed.backup)?;
    } else {
        anyhow::bail!(
            "independent auth backup cannot be finalized without adopting or replacing another file; {}",
            publication_state_detail(live_path, parsed)
        );
    }

    harden_private_file(&parsed.backup)?;
    let candidate_is_still_live =
        crate::fs_ops::token_if_present(live_path)?.as_ref() == Some(&parsed.candidate_token);
    remove_publication_record(parsed)?;
    cleanup_old_backups(live_path);
    Ok(candidate_is_still_live)
}

fn cleanup_unpublished_auth(live_path: &Path, parsed: &ParsedAuthPublication) -> Result<()> {
    let backup_stage = crate::fs_ops::token_if_present(&parsed.backup_stage)?;
    let backup = crate::fs_ops::token_if_present(&parsed.backup)?;
    if backup.is_some() {
        anyhow::bail!(
            "refusing to abort an auth publication after its retained backup appeared; {}",
            publication_state_detail(live_path, parsed)
        );
    }
    match backup_stage.as_ref() {
        Some(token) if token == &parsed.backup_token => {
            remove_bound_path(&parsed.backup_stage, &parsed.backup_token)?;
        }
        None => {}
        Some(observed) => anyhow::bail!(
            "refusing to remove a replacement at {}; expected token {}, observed {}",
            parsed.backup_stage.display(),
            parsed.backup_token,
            observed
        ),
    }

    #[cfg(windows)]
    if crate::fs_ops::token_if_present(&parsed.displaced)?.is_some() {
        anyhow::bail!(
            "refusing to classify an unpublished Windows transaction while a displaced file exists; {}",
            publication_state_detail(live_path, parsed)
        );
    }

    match crate::fs_ops::token_if_present(&parsed.candidate)?.as_ref() {
        Some(token) if token == &parsed.candidate_token => {
            remove_bound_path(&parsed.candidate, &parsed.candidate_token)?;
        }
        None => {}
        Some(observed) => anyhow::bail!(
            "refusing to remove a replacement at {}; expected token {}, observed {}",
            parsed.candidate.display(),
            parsed.candidate_token,
            observed
        ),
    }
    remove_publication_record(parsed)
}

fn restore_foreign_live(
    live_path: &Path,
    parsed: &ParsedAuthPublication,
    foreign: &crate::fs_ops::FileToken,
) -> Result<()> {
    let live_before = crate::fs_ops::token_if_present(live_path)?;
    let displaced_before = crate::fs_ops::token_if_present(&parsed.displaced)?;
    if live_before.as_ref() != Some(&parsed.candidate_token)
        || displaced_before.as_ref() != Some(foreign)
    {
        anyhow::bail!(
            "live or displaced auth changed before exact restoration; {}",
            publication_state_detail(live_path, parsed)
        );
    }
    #[cfg(windows)]
    if crate::fs_ops::token_if_present(&parsed.candidate)?.is_some() {
        anyhow::bail!(
            "refusing Windows auth restoration because its random displaced destination is no longer empty; {}",
            publication_state_detail(live_path, parsed)
        );
    }
    let boundary = restore_displaced_to_live(&parsed.candidate, live_path, &parsed.displaced);
    let live_after = crate::fs_ops::token_if_present(live_path)?;
    let candidate_after = crate::fs_ops::token_if_present(&parsed.candidate)?;
    let displaced_after = if parsed.displaced == parsed.candidate {
        candidate_after.clone()
    } else {
        crate::fs_ops::token_if_present(&parsed.displaced)?
    };
    let restored = live_after.as_ref() == Some(foreign)
        && candidate_after.as_ref() == Some(&parsed.candidate_token)
        && (parsed.displaced == parsed.candidate || displaced_after.is_none());
    if !restored {
        return Err(boundary.err().unwrap_or_else(|| {
            anyhow::anyhow!("exchange-back post-state changed before exact restoration")
        }));
    }
    // Windows namespace primitives can report failure after mutating names.
    // Never upgrade such an error from post-state alone or clean the recovery
    // artifacts; Unix uses this same boundary helper for the parent fsync.
    confirm_namespace_boundary(live_path, &boundary).with_context(|| {
        format!(
            "the foreign live credential was visibly restored at {}, but durability is unconfirmed",
            live_path.display()
        )
    })?;
    cleanup_unpublished_auth(live_path, parsed)
}

fn settle_auth_publication(
    live_path: &Path,
    parsed: &ParsedAuthPublication,
    expected_inode: &ExpectedInodeObservation,
) -> Result<PublicationSettlement> {
    let live = crate::fs_ops::token_if_present(live_path)?;
    let candidate = crate::fs_ops::token_if_present(&parsed.candidate)?;
    let displaced = if parsed.displaced == parsed.candidate {
        candidate.clone()
    } else {
        crate::fs_ops::token_if_present(&parsed.displaced)?
    };
    let backup_stage = crate::fs_ops::token_if_present(&parsed.backup_stage)?;
    let backup = crate::fs_ops::token_if_present(&parsed.backup)?;
    let backup_waiting = backup_stage.as_ref() == Some(&parsed.backup_token) && backup.is_none();
    let backup_retained = backup_stage.is_none() && backup.as_ref() == Some(&parsed.backup_token);
    let backup_ready = backup_waiting || backup_retained;

    if live.as_ref() == Some(&parsed.candidate_token) {
        if backup_ready
            && (displaced.as_ref() == Some(&parsed.expected_token) || displaced.is_none())
        {
            return match finalize_independent_backup(live_path, parsed) {
                Ok(true) => Ok(PublicationSettlement::Published),
                Ok(false) => Ok(PublicationSettlement::LiveUnchanged),
                Err(error) => Ok(PublicationSettlement::PublishedRecoveryRequired(format!(
                    "new live auth is published, but its independent recovery backup is incomplete: {error:#}"
                ))),
            };
        }

        if backup_waiting
            && let Some(foreign) = displaced.as_ref()
            && foreign != &parsed.expected_token
            && foreign != &parsed.candidate_token
        {
            return match expected_inode {
                ExpectedInodeObservation::Unchanged => {
                    match restore_foreign_live(live_path, parsed, foreign) {
                        Ok(()) => Ok(PublicationSettlement::LiveUnchanged),
                        Err(error) => Ok(PublicationSettlement::RestoredRecoveryRequired(format!(
                            "conditional auth publication encountered a foreign writer, but exact restoration or cleanup is incomplete: {error:#}; {}",
                            publication_state_detail(live_path, parsed)
                        ))),
                    }
                }
                ExpectedInodeObservation::Changed(observed) => {
                    Ok(PublicationSettlement::PublishedRecoveryRequired(format!(
                        "new live auth is published, but the previously-open live credential was modified after the exchange (observed token {observed}); its actual displaced file and the independent backup were preserved"
                    )))
                }
                ExpectedInodeObservation::Unreadable(error) => {
                    Ok(PublicationSettlement::PublishedRecoveryRequired(format!(
                        "new live auth is published, but the previously-open live credential could not be revalidated after the exchange ({error}); its actual displaced file and the independent backup were preserved"
                    )))
                }
                ExpectedInodeObservation::Unavailable => {
                    Ok(PublicationSettlement::PublishedRecoveryRequired(format!(
                        "new live auth is published, but recovery cannot distinguish a pre-exchange foreign writer from a post-exchange mutation of the displaced credential; every artifact was preserved; {}",
                        publication_state_detail(live_path, parsed)
                    )))
                }
            };
        }

        return Ok(PublicationSettlement::PublishedRecoveryRequired(format!(
            "new live auth is visible, but transaction artifacts no longer match the expected displaced credential; {}",
            publication_state_detail(live_path, parsed)
        )));
    }

    // The candidate was published and then a later writer replaced the public
    // name. Finish only the independently-copied recovery backup; never touch
    // that later writer.
    if displaced.as_ref() == Some(&parsed.expected_token) && backup_ready {
        return match finalize_independent_backup(live_path, parsed) {
            Ok(_) => Ok(PublicationSettlement::LiveUnchanged),
            Err(error) => Ok(PublicationSettlement::AmbiguousRecoveryRequired(format!(
                "the conditional candidate was published but is no longer live, and recovery finalization is incomplete: {error:#}; {}",
                publication_state_detail(live_path, parsed)
            ))),
        };
    }

    #[cfg(unix)]
    let candidate_waiting = candidate.as_ref() == Some(&parsed.candidate_token);
    #[cfg(windows)]
    let candidate_waiting =
        candidate.as_ref() == Some(&parsed.candidate_token) && displaced.is_none();
    if candidate_waiting
        && !backup_retained
        && backup_stage
            .as_ref()
            .is_none_or(|token| token == &parsed.backup_token)
    {
        return match cleanup_unpublished_auth(live_path, parsed) {
            Ok(()) => Ok(PublicationSettlement::LiveUnchanged),
            Err(error) => Ok(PublicationSettlement::RestoredRecoveryRequired(format!(
                "live auth was not replaced, but exact private transaction cleanup is incomplete: {error:#}"
            ))),
        };
    }

    let no_transaction_artifacts =
        candidate.is_none() && displaced.is_none() && backup_stage.is_none() && backup.is_none();
    if no_transaction_artifacts {
        return match remove_publication_record(parsed) {
            Ok(()) => Ok(PublicationSettlement::LiveUnchanged),
            Err(error) => Ok(PublicationSettlement::RestoredRecoveryRequired(format!(
                "auth transaction artifacts were already cleaned, but the exact recovery record remains: {error:#}"
            ))),
        };
    }

    if live.as_ref() == Some(&parsed.expected_token)
        && candidate.is_none()
        && displaced.is_none()
        && backup_waiting
    {
        return match cleanup_unpublished_auth(live_path, parsed) {
            Ok(()) => Ok(PublicationSettlement::LiveUnchanged),
            Err(error) => Ok(PublicationSettlement::RestoredRecoveryRequired(format!(
                "the original live auth is intact, but exact private transaction cleanup is incomplete: {error:#}"
            ))),
        };
    }

    Ok(PublicationSettlement::AmbiguousRecoveryRequired(format!(
        "auth publication state cannot be classified without risking credential loss; every artifact was preserved: {}",
        publication_state_detail(live_path, parsed)
    )))
}

/// Resolve a fixed durable auth-publication record while the caller holds the
/// live-auth transaction. Exact known states are completed or rolled back;
/// unknown namespace occupants are never overwritten or deleted.
pub(crate) fn recover_interrupted_auth_publication(live_path: &Path) -> Result<()> {
    let _directory_guard = acquire_private_directory(private_write_parent(live_path)?)?;
    let Some((record, record_token)) = read_publication_record(live_path)? else {
        return Ok(());
    };
    let parsed = parse_publication_record(live_path, record, record_token)?;
    match settle_auth_publication(live_path, &parsed, &ExpectedInodeObservation::Unavailable)? {
        PublicationSettlement::Published | PublicationSettlement::LiveUnchanged => Ok(()),
        PublicationSettlement::PublishedRecoveryRequired(message)
        | PublicationSettlement::RestoredRecoveryRequired(message)
        | PublicationSettlement::AmbiguousRecoveryRequired(message) => {
            Err(anyhow::anyhow!(message))
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_BEFORE_AUTH_EXCHANGE:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static TEST_AFTER_AUTH_EXCHANGE:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static TEST_BEFORE_BACKUP_RETENTION_DELETE:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn before_next_auth_exchange(action: impl FnOnce() + 'static) {
    TEST_BEFORE_AUTH_EXCHANGE.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

#[cfg(test)]
fn run_before_auth_exchange_test_hook() {
    TEST_BEFORE_AUTH_EXCHANGE.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action();
        }
    });
}

#[cfg(test)]
fn after_next_auth_exchange(action: impl FnOnce() + 'static) {
    TEST_AFTER_AUTH_EXCHANGE.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

#[cfg(test)]
fn run_after_auth_exchange_test_hook() {
    TEST_AFTER_AUTH_EXCHANGE.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action();
        }
    });
}

#[cfg(test)]
fn before_next_backup_retention_delete(action: impl FnOnce() + 'static) {
    TEST_BEFORE_BACKUP_RETENTION_DELETE.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

#[cfg(test)]
fn run_before_backup_retention_delete_test_hook() {
    TEST_BEFORE_BACKUP_RETENTION_DELETE.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action();
        }
    });
}

fn create_independent_backup_stage(
    source: &Path,
    destination: &Path,
    expected: &crate::fs_ops::FileToken,
) -> Result<ExactPrivateFile> {
    let directory_guard = acquire_private_directory(private_write_parent(destination)?)?;
    let creation = crate::fs_ops::create_exclusive_copy(source, destination, expected)
        .with_context(|| {
            format!(
                "creating independent auth recovery copy {} -> {}",
                source.display(),
                destination.display()
            )
        })?;
    let token = creation.token().clone();
    let mut owned = ExactPrivateFile {
        path: destination.to_path_buf(),
        token,
        cleanup_on_drop: true,
        _directory_guard: directory_guard,
    };
    if matches!(
        creation,
        crate::fs_ops::CreateExactOutcome::CreatedNamespaceDurabilityUnconfirmed(_)
    ) {
        #[cfg(unix)]
        if let Err(durability) = confirm_namespace_durability(destination) {
            return match remove_bound_path(destination, owned.token()) {
                Ok(()) => {
                    owned.disarm();
                    Err(durability).with_context(|| {
                        format!(
                            "independent auth recovery copy {} was visible but not durably confirmed and was removed exactly",
                            destination.display()
                        )
                    })
                }
                Err(cleanup) => {
                    owned.disarm();
                    anyhow::bail!(
                        "independent auth recovery copy {} was not durably confirmed ({durability:#}) and could not be removed exactly ({cleanup:#}); it was preserved",
                        destination.display()
                    )
                }
            };
        }
        #[cfg(windows)]
        anyhow::bail!(
            "Windows independent auth recovery copy {} returned an unsupported Unix-only durability outcome",
            destination.display()
        );
    }
    if let Err(hardening) = harden_private_file(destination) {
        return match remove_bound_path(destination, owned.token()) {
            Ok(()) => {
                owned.disarm();
                Err(hardening).with_context(|| {
                    format!(
                        "securing independent auth recovery copy {}",
                        destination.display()
                    )
                })
            }
            Err(cleanup) => {
                owned.disarm();
                anyhow::bail!(
                    "independent auth recovery copy {} could not be secured ({hardening:#}) or exactly removed ({cleanup:#}); it was preserved",
                    destination.display()
                )
            }
        };
    }
    Ok(owned)
}

fn prepare_existing_auth_publication(
    path: &Path,
    expected_token: &crate::fs_ops::FileToken,
    contents: &[u8],
) -> Result<(String, String, ExactPrivateFile, ExactPrivateFile)> {
    const UNIQUE_NAME_ATTEMPTS: usize = 16;

    for _ in 0..UNIQUE_NAME_ATTEMPTS {
        let nonce = transaction_nonce();
        let stamp = backup_stamp()?;
        let candidate_path = transaction_role_path(path, "candidate", &nonce)?;
        let backup_stage_path = transaction_role_path(path, "backup", &nonce)?;
        let backup_path = backup_path_for_transaction(path, &stamp, &nonce)?;
        #[cfg(windows)]
        let displaced_path = transaction_role_path(path, "displaced", &nonce)?;

        if crate::fs_ops::token_if_present(&backup_path)?.is_some()
            || cfg!(windows) && {
                #[cfg(windows)]
                {
                    crate::fs_ops::token_if_present(&displaced_path)?.is_some()
                }
                #[cfg(not(windows))]
                {
                    false
                }
            }
        {
            continue;
        }

        let mut candidate = stage_exact_private_file(path, contents)?;
        if let Err(error) = move_owned_noreplace(&mut candidate, &candidate_path) {
            if io_error_kind(&error) == Some(std::io::ErrorKind::AlreadyExists) {
                continue;
            }
            return Err(error).with_context(|| {
                format!(
                    "preparing strict auth candidate {}",
                    candidate_path.display()
                )
            });
        }

        match create_independent_backup_stage(path, &backup_stage_path, expected_token) {
            Ok(backup_stage) => return Ok((nonce, stamp, candidate, backup_stage)),
            Err(error) if io_error_kind(&error) == Some(std::io::ErrorKind::AlreadyExists) => {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    anyhow::bail!(
        "could not reserve collision-free auth publication paths for {}",
        path.display()
    )
}

fn conditional_write_from_settlement(
    path: &Path,
    settlement: PublicationSettlement,
    boundary: Option<Result<()>>,
) -> Result<ConditionalWrite> {
    match settlement {
        PublicationSettlement::Published => {
            verify_private_permissions_after_publication(path, "conditionally exchanged");
            Ok(ConditionalWrite::Written)
        }
        PublicationSettlement::LiveUnchanged => match boundary {
            Some(Err(error)) => Err(error).with_context(|| {
                format!(
                    "auth publication boundary failed and exact recovery restored {}",
                    path.display()
                )
            }),
            Some(Ok(())) | None => Ok(ConditionalWrite::Changed),
        },
        PublicationSettlement::PublishedRecoveryRequired(message) => {
            Ok(ConditionalWrite::PublishedRecoveryRequired(message))
        }
        PublicationSettlement::RestoredRecoveryRequired(message) => {
            Ok(ConditionalWrite::RestoredRecoveryRequired(message))
        }
        PublicationSettlement::AmbiguousRecoveryRequired(message) => {
            Ok(ConditionalWrite::AmbiguousRecoveryRequired(message))
        }
    }
}

/// Publish a private candidate without ever making an existing public auth
/// name disappear. A missing destination uses a no-replace rename. An existing
/// destination is atomically exchanged with the candidate (or ReplaceFileW on
/// Windows), and the actual displaced file is classified by identity+digest.
pub(crate) fn atomic_write_private_if_unchanged(
    path: &Path,
    expected: Option<&[u8]>,
    contents: &[u8],
) -> Result<ConditionalWrite> {
    recover_interrupted_auth_publication(path).with_context(|| {
        format!(
            "an earlier auth publication at {} requires recovery before another write",
            path.display()
        )
    })?;
    let Some(expected_bytes) = expected else {
        let mut candidate = stage_exact_private_file(path, contents)?;
        let boundary = crate::fs_ops::rename_noreplace(candidate.path(), path);
        let source_after = crate::fs_ops::token_if_present(candidate.path())?;
        let destination_after = crate::fs_ops::token_if_present(path)?;
        if destination_after.as_ref() == Some(candidate.token()) && source_after.is_none() {
            candidate.disarm();
            return match confirm_namespace_boundary(path, &boundary) {
                Ok(()) => {
                    verify_private_permissions_after_publication(path, "conditionally created");
                    Ok(ConditionalWrite::Written)
                }
                Err(error) => Ok(ConditionalWrite::PublishedRecoveryRequired(format!(
                    "new auth is visible at {}, but its namespace commit could not be durably confirmed: {:#}",
                    path.display(),
                    boundary.err().unwrap_or(error)
                ))),
            };
        }
        if source_after.as_ref() == Some(candidate.token()) {
            return if destination_after.is_some() {
                Ok(ConditionalWrite::Changed)
            } else {
                Err(boundary.err().unwrap_or_else(|| {
                    anyhow::anyhow!("no-replace auth publication made no namespace change")
                }))
            };
        }
        candidate.disarm();
        return Ok(ConditionalWrite::AmbiguousRecoveryRequired(format!(
            "no-replace auth publication left unclassified state ({:#}); candidate and public paths were preserved",
            boundary
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("post-state changed after successful rename"))
        )));
    };

    let Some(expected_token) = crate::fs_ops::token_if_present(path)? else {
        return Ok(ConditionalWrite::Changed);
    };
    if !expected_token.matches_bytes(expected_bytes) {
        return Ok(ConditionalWrite::Changed);
    }

    let (nonce, stamp, mut candidate, mut backup_stage) =
        prepare_existing_auth_publication(path, &expected_token, contents)?;
    let record = AuthPublicationRecord {
        version: AUTH_PUBLICATION_RECORD_VERSION,
        nonce,
        backup_stamp: stamp,
        expected_token: expected_token.to_string(),
        candidate_token: candidate.token().to_string(),
        backup_token: backup_stage.token().to_string(),
    };
    let record_path = auth_publication_record_path(path)?;
    let record_token = match publish_record_exclusive(&record_path, &record)? {
        RecordPublication::Durable(token) => token,
        RecordPublication::VisibleNotDurable { token, error } => {
            candidate.disarm();
            backup_stage.disarm();
            return Ok(ConditionalWrite::RestoredRecoveryRequired(format!(
                "live auth was not replaced, but the fixed recovery record {} became visible without durable confirmation ({error:#}); record token {token} and all exact private artifacts were preserved",
                record_path.display()
            )));
        }
    };
    let parsed = parse_publication_record(path, record, record_token)?;
    candidate.disarm();
    backup_stage.disarm();

    let mut expected_handle = match crate::fs_ops::open_direct_regular(path) {
        Ok(file) => file,
        Err(error) => {
            let settlement =
                settle_auth_publication(path, &parsed, &ExpectedInodeObservation::Unavailable)?;
            let cleanup = conditional_write_from_settlement(path, settlement, None)?;
            return match cleanup {
                ConditionalWrite::Changed => Err(error).with_context(|| {
                    format!(
                        "opening the exact live auth witness before publication at {}",
                        path.display()
                    )
                }),
                other => Ok(other),
            };
        }
    };
    let handle_before = crate::fs_ops::token_for_file(&mut expected_handle)?;
    if handle_before != expected_token {
        let settlement =
            settle_auth_publication(path, &parsed, &ExpectedInodeObservation::Unavailable)?;
        return conditional_write_from_settlement(path, settlement, None);
    }

    #[cfg(test)]
    run_before_auth_exchange_test_hook();
    let live_immediately_before = crate::fs_ops::token_if_present(path)?;
    let handle_immediately_before = crate::fs_ops::token_for_file(&mut expected_handle)?;
    if live_immediately_before.as_ref() != Some(&expected_token)
        || handle_immediately_before != expected_token
    {
        let settlement =
            settle_auth_publication(path, &parsed, &ExpectedInodeObservation::Unavailable)?;
        return conditional_write_from_settlement(path, settlement, None);
    }

    let boundary = exchange_candidate_with_live(&parsed.candidate, path, &parsed.displaced);
    #[cfg(test)]
    run_after_auth_exchange_test_hook();
    #[cfg(windows)]
    if let Err(error) = boundary.as_ref() {
        return Ok(ConditionalWrite::AmbiguousRecoveryRequired(format!(
            "the Windows auth namespace primitive reported failure ({error:#}); public and recovery post-state were preserved for the next exact recovery pass; {}",
            publication_state_detail(path, &parsed)
        )));
    }
    let expected_inode = match crate::fs_ops::token_for_file(&mut expected_handle) {
        Ok(observed) if observed == expected_token => ExpectedInodeObservation::Unchanged,
        Ok(observed) => ExpectedInodeObservation::Changed(observed),
        Err(error) => ExpectedInodeObservation::Unreadable(format!("{error:#}")),
    };
    let settlement = match settle_auth_publication(path, &parsed, &expected_inode) {
        Ok(settlement) => settlement,
        Err(error) => {
            return Ok(ConditionalWrite::AmbiguousRecoveryRequired(format!(
                "auth publication boundary could not classify every preserved path: {error:#}; {}",
                publication_state_detail(path, &parsed)
            )));
        }
    };
    conditional_write_from_settlement(path, settlement, Some(boundary))
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsTokenOwnership {
    user_sid: String,
    default_owner_sid: String,
}

#[cfg(windows)]
fn windows_current_token_ownership(path: &Path) -> Result<WindowsTokenOwnership> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSID, TOKEN_INFORMATION_CLASS, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
        TokenOwner, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
    struct LocalAllocation(*mut core::ffi::c_void);
    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0) };
        }
    }
    let last_error = |api: &str| {
        anyhow::anyhow!(
            "{api} failed for {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )
    };

    fn token_information(
        token: HANDLE,
        class: TOKEN_INFORMATION_CLASS,
        description: &str,
        path: &Path,
    ) -> Result<Vec<usize>> {
        let mut bytes = 0;
        let probe_ok = unsafe { GetTokenInformation(token, class, null_mut(), 0, &mut bytes) };
        let probe_error = std::io::Error::last_os_error();
        if probe_ok != 0
            || bytes == 0
            || probe_error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
        {
            anyhow::bail!(
                "GetTokenInformation({description}) size failed for {}: {probe_error}",
                path.display()
            );
        }
        let words = (bytes as usize).div_ceil(std::mem::size_of::<usize>());
        let mut information = vec![0usize; words];
        if unsafe {
            GetTokenInformation(
                token,
                class,
                information.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            )
        } == 0
        {
            anyhow::bail!(
                "GetTokenInformation({description}) failed for {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        Ok(information)
    }

    fn sid_string(sid: PSID, description: &str, path: &Path) -> Result<String> {
        if sid.is_null() {
            anyhow::bail!(
                "GetTokenInformation({description}) returned a null SID for {}",
                path.display()
            );
        }
        let mut string_sid = null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut string_sid) } == 0 {
            anyhow::bail!(
                "ConvertSidToStringSidW({description}) failed for {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        let _string_sid = LocalAllocation(string_sid.cast());
        let mut sid_len = 0;
        while unsafe { *string_sid.add(sid_len) } != 0 {
            sid_len += 1;
        }
        String::from_utf16(unsafe { std::slice::from_raw_parts(string_sid, sid_len) }).with_context(
            || {
                format!(
                    "decoding ConvertSidToStringSidW({description}) output for {}",
                    path.display()
                )
            },
        )
    }

    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    let _token = OwnedHandle(token);
    let token_user = token_information(token, TokenUser, "TokenUser", path)?;
    let token_owner = token_information(token, TokenOwner, "TokenOwner", path)?;
    let user_sid = unsafe { (*(token_user.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    let default_owner_sid = unsafe { (*(token_owner.as_ptr().cast::<TOKEN_OWNER>())).Owner };
    Ok(WindowsTokenOwnership {
        user_sid: sid_string(user_sid, "TokenUser", path)?,
        default_owner_sid: sid_string(default_owner_sid, "TokenOwner", path)?,
    })
}

#[cfg(windows)]
fn windows_current_user_sid_string(path: &Path) -> Result<String> {
    Ok(windows_current_token_ownership(path)?.user_sid)
}

#[cfg(windows)]
fn require_windows_path_owner(path: &Path, description: &str) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    struct LocalAllocation(*mut core::ffi::c_void);
    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0) };
        }
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut owner = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        anyhow::bail!(
            "reading owner for {description} {} failed: {}",
            path.display(),
            std::io::Error::from_raw_os_error(status as i32)
        );
    }
    let _descriptor = LocalAllocation(descriptor);
    let mut owner_string = null_mut();
    if unsafe { ConvertSidToStringSidW(owner, &mut owner_string) } == 0 {
        anyhow::bail!(
            "converting owner SID for {description} {} failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    let _owner_string = LocalAllocation(owner_string.cast());
    let mut owner_len = 0;
    while unsafe { *owner_string.add(owner_len) } != 0 {
        owner_len += 1;
    }
    let owner_sid =
        String::from_utf16(unsafe { std::slice::from_raw_parts(owner_string, owner_len) })?;
    let token_ownership = windows_current_token_ownership(path)?;
    if !windows_owner_sid_matches_token(
        &owner_sid,
        &token_ownership.user_sid,
        &token_ownership.default_owner_sid,
    ) {
        anyhow::bail!(
            "{description} must be owned by the current Windows token user or its default owner: {} is owned by SID {}",
            path.display(),
            owner_sid
        );
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn windows_owner_sid_matches_token(
    owner_sid: &str,
    user_sid: &str,
    default_owner_sid: &str,
) -> bool {
    owner_sid == user_sid || owner_sid == default_owner_sid
}

#[cfg(any(windows, test))]
fn windows_private_acl_sddl(current_user_sid: &str, directory: bool) -> String {
    let inheritance = if directory { "OICI" } else { "" };
    format!(
        "D:P(A;{inheritance};FA;;;{current_user_sid})\
         (A;{inheritance};FA;;;S-1-5-18)\
         (A;{inheritance};FA;;;S-1-5-32-544)"
    )
}

#[cfg(windows)]
fn harden_windows_acl(path: &Path, directory: bool) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    struct LocalAllocation(*mut core::ffi::c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            // SAFETY: both wrapped pointers come from Win32 APIs documented to
            // allocate with LocalAlloc and are released exactly once here.
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    fn last_error(path: &Path, api: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "{api} failed for {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )
    }

    let current_user_sid = windows_current_user_sid_string(path)?;

    let sddl = windows_private_acl_sddl(&current_user_sid, directory);
    let sddl_wide: Vec<u16> = std::ffi::OsStr::new(&sddl)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut security_descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `sddl_wide` is NUL-terminated and the output pointer is writable;
    // the returned descriptor is owned by LocalFree.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_wide.as_ptr(),
            SDDL_REVISION_1,
            &mut security_descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(last_error(
            path,
            "ConvertStringSecurityDescriptorToSecurityDescriptorW",
        ));
    }
    let _security_descriptor = LocalAllocation(security_descriptor);

    let mut dacl_present = 0;
    let mut dacl: *mut ACL = null_mut();
    let mut dacl_defaulted = 0;
    // SAFETY: `security_descriptor` is live and valid; all output pointers
    // refer to initialized local variables.
    if unsafe {
        GetSecurityDescriptorDacl(
            security_descriptor,
            &mut dacl_present,
            &mut dacl,
            &mut dacl_defaulted,
        )
    } == 0
    {
        return Err(last_error(path, "GetSecurityDescriptorDacl"));
    }
    if dacl_present == 0 || dacl.is_null() {
        anyhow::bail!(
            "GetSecurityDescriptorDacl returned no DACL for {}",
            path.display()
        );
    }

    let path_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: the path is NUL-terminated, `dacl` points inside the live
    // security descriptor, and null owner/group/SACL pointers are required
    // because only the exact protected DACL is being replaced.
    let status = unsafe {
        SetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(anyhow::anyhow!(
            "SetNamedSecurityInfoW failed for {}: {}",
            path.display(),
            std::io::Error::from_raw_os_error(status as i32)
        ));
    }

    Ok(())
}

#[cfg(windows)]
pub(crate) fn harden_windows_private_file(path: &Path) -> Result<()> {
    require_windows_path_owner(path, "private file")?;
    harden_windows_acl(path, false)
}

pub fn write_auth(path: &Path, val: &serde_json::Value) -> Result<PrivateWriteOutcome> {
    let raw = serde_json::to_string_pretty(val)?;
    atomic_write_private(path, raw.as_bytes())
}

/// Mask sensitive token/credential fields in a JSON body before logging.
/// Used by debug-level logs that may otherwise leak access/refresh/id tokens
/// when users share `--debug` output (e.g. in a bug report).
pub(crate) fn redact_sensitive_log_body(body: &serde_json::Value) -> String {
    const SENSITIVE_KEYS: &[&str] = &[
        "authorization_code",
        "code_verifier",
        "access_token",
        "refresh_token",
        "id_token",
        "client_secret",
    ];

    fn redact(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(obj) => {
                for key in SENSITIVE_KEYS {
                    if obj.contains_key(*key) {
                        obj.insert((*key).to_string(), serde_json::json!("***"));
                    }
                }
                for (key, v) in obj.iter_mut() {
                    if !SENSITIVE_KEYS.contains(&key.as_str()) {
                        redact(v);
                    }
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr.iter_mut() {
                    redact(v);
                }
            }
            _ => {}
        }
    }

    let mut value = body.clone();
    redact(&mut value);
    serde_json::to_string(&value).expect("serde_json::Value always serializes to JSON")
}

fn io_error_kind(error: &anyhow::Error) -> Option<std::io::ErrorKind> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .map(std::io::Error::kind)
}

pub fn apply_tokens(
    val: &mut serde_json::Value,
    id_token: &str,
    access_token: &str,
    refresh_token: &str,
) -> Result<()> {
    validate_complete_oauth_tokens(id_token, access_token, refresh_token)?;
    let tokens = val
        .get_mut("tokens")
        .and_then(|t| t.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!("auth.json missing tokens object"))?;

    tokens.insert("id_token".into(), serde_json::json!(id_token));
    tokens.insert("access_token".into(), serde_json::json!(access_token));
    tokens.insert("refresh_token".into(), serde_json::json!(refresh_token));
    // Codex refreshes proactively when last_refresh is older than 8 days;
    // stamping it here keeps our refreshes recognized (matches upstream).
    if let Some(obj) = val.as_object_mut() {
        obj.insert(
            "last_refresh".into(),
            serde_json::json!(crate::output::format_iso8601(now_unix_secs())),
        );
    }
    Ok(())
}

/// Validate a complete OAuth token set before it can cross a credential-write
/// boundary. OAuth responses distinguish an omitted optional field from an
/// explicitly present but empty value; the latter must never erase a usable
/// credential.
pub(crate) fn validate_complete_oauth_tokens(
    id_token: &str,
    access_token: &str,
    refresh_token: &str,
) -> Result<()> {
    for (field, value) in [
        ("id_token", id_token),
        ("access_token", access_token),
        ("refresh_token", refresh_token),
    ] {
        if value.trim().is_empty() {
            anyhow::bail!("OAuth {field} must not be empty");
        }
    }
    Ok(())
}

/// Extract (access_token, refresh_token) from an auth.json Value.
pub fn extract_tokens(val: &serde_json::Value) -> (Option<String>, Option<String>) {
    let at = val
        .pointer("/tokens/access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let rt = val
        .pointer("/tokens/refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    (at, rt)
}

pub fn extract_id_token(val: &serde_json::Value) -> Option<String> {
    val.pointer("/tokens/id_token")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Current unix timestamp in seconds.
pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read auth.json and parse AccountInfo without collapsing path, read, or JSON
/// errors into an empty account model.
pub(crate) fn read_account_info_checked(path: &Path) -> Result<crate::jwt::AccountInfo> {
    let value = read_auth(path)?;
    let mut info = account_info_from_auth_value(&value);
    crate::cache::apply_workspace_name(&mut info)?;
    Ok(info)
}

/// Derive request-routing and display metadata from the exact auth snapshot
/// that supplied the request's bearer token. Callers that already loaded an
/// auth value must not reopen the path and accidentally mix two generations.
pub(crate) fn account_info_from_auth_value(value: &serde_json::Value) -> crate::jwt::AccountInfo {
    crate::jwt::parse_account_info(value)
}

pub fn validate_auth_value(val: &serde_json::Value) -> Result<crate::jwt::AccountInfo> {
    let tokens = val
        .get("tokens")
        .and_then(|t| t.as_object())
        .ok_or_else(|| anyhow::anyhow!("auth.json missing tokens object"))?;

    let id_token = tokens
        .get("id_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("tokens.id_token is required"))?;

    let has_access = tokens
        .get("access_token")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    let has_refresh = tokens
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());

    if !has_access && !has_refresh {
        return Err(anyhow::anyhow!(
            "tokens.access_token or tokens.refresh_token is required"
        ));
    }

    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("tokens.id_token is not a valid JWT"))?;
    let decoded = {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| anyhow::anyhow!("tokens.id_token payload is not valid base64url"))?
    };
    let _: serde_json::Value = serde_json::from_slice(&decoded)
        .map_err(|_| anyhow::anyhow!("tokens.id_token payload is not valid JSON"))?;

    let info = crate::jwt::parse_account_info(val);
    if info.account_id.as_deref().is_none_or(str::is_empty) {
        return Err(anyhow::anyhow!(
            "id_token does not contain a usable account_id"
        ));
    }

    Ok(info)
}

/// Build a shared reqwest client with standard user-agent and proxy support.
pub fn build_http_client() -> Result<reqwest::Client> {
    let proxy_url = crate::config::resolve_proxy()?;
    let no_proxy = crate::config::resolve_no_proxy()?;
    build_http_client_with_proxy(proxy_url.as_deref(), no_proxy.as_deref())
}

fn build_http_client_with_proxy(
    proxy_url: Option<&str>,
    no_proxy: Option<&str>,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(codex_user_agent())
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(60));

    if let Some(url) = proxy_url {
        let mut proxy = parse_http_proxy_url(url)?;
        let sanitized_url = sanitize_proxy_url(url);
        tracing::debug!("Using proxy: {sanitized_url}");
        if let Some(no_proxy) = no_proxy {
            tracing::debug!("No-proxy list: {no_proxy}");
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string(no_proxy));
        }
        builder = builder.proxy(proxy);
    }

    if let Some(path) = custom_ca_path_from_values(
        std::env::var_os("CODEX_CA_CERTIFICATE"),
        std::env::var_os("SSL_CERT_FILE"),
    ) {
        let pem = std::fs::read(&path)
            .with_context(|| format!("reading custom CA bundle {}", path.display()))?;
        let certificates = reqwest::Certificate::from_pem_bundle(&pem)
            .with_context(|| format!("parsing custom CA bundle {}", path.display()))?;
        if certificates.is_empty() {
            anyhow::bail!(
                "custom CA bundle {} contains no certificates",
                path.display()
            );
        }
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }

    Ok(builder.build()?)
}

/// Parse proxy configuration through the same reqwest boundary used by every
/// HTTP client. Parse failures deliberately omit both the original URL and the
/// parser's source text: either may contain userinfo from a malformed URL.
pub(crate) fn parse_http_proxy_url(url: &str) -> Result<reqwest::Proxy> {
    reqwest::Proxy::all(url)
        .map_err(|_| anyhow::anyhow!("invalid proxy URL (credentials, if present, were redacted)"))
}

fn custom_ca_path_from_values(
    codex_ca: Option<OsString>,
    ssl_cert_file: Option<OsString>,
) -> Option<PathBuf> {
    codex_ca
        .filter(|value| !value.is_empty())
        .or_else(|| ssl_cert_file.filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

fn sanitize_proxy_url(url: &str) -> String {
    let Some(scheme_sep) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_sep + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map(|idx| authority_start + idx)
        .unwrap_or(url.len());
    let authority = &url[authority_start..authority_end];
    let Some(userinfo_end) = authority.rfind('@') else {
        return url.to_string();
    };
    let at_pos = authority_start + userinfo_end;

    let mut sanitized = String::with_capacity(url.len());
    sanitized.push_str(&url[..authority_start]);
    sanitized.push_str("***:***");
    sanitized.push_str(&url[at_pos..]);
    sanitized
}

/// An intercepting proxy re-signs traffic with its own CA, and rustls reports
/// that as a bare "UnknownIssuer" with no indication of what to do. The OS trust
/// store is consulted first, so reaching here means the CA is not installed
/// there either and has to be supplied explicitly.
fn tls_trust_hint(message: &str) -> Option<&'static str> {
    if message.contains("UnknownIssuer") || message.contains("invalid peer certificate") {
        return Some(
            "\n  hint: the server's certificate was not signed by a CA this machine trusts. \
             An intercepting proxy (Proxyman, Charles, a corporate MITM) re-signs traffic with \
             its own CA — add that CA to the system trust store, or export it as PEM and point \
             CODEX_CA_CERTIFICATE at the file.",
        );
    }
    None
}

/// Format a reqwest error with the full source chain for diagnostics.
pub fn format_reqwest_error(context: &str, err: &reqwest::Error) -> anyhow::Error {
    let mut msg = format!("{context}: {err}");
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        msg.push_str(&format!("\n  caused by: {cause}"));
        source = std::error::Error::source(cause);
    }
    if let Some(hint) = tls_trust_hint(&msg) {
        msg.push_str(hint);
    }
    anyhow::anyhow!("{msg}")
}

fn cleanup_old_backups(path: &Path) {
    let parent = match path.parent() {
        Some(p) => p,
        None => return,
    };
    let stem = match path.file_name().and_then(|f| f.to_str()) {
        Some(s) => s,
        None => return,
    };
    let mut backups: Vec<((u128, String), PathBuf, crate::fs_ops::FileToken)> =
        std::fs::read_dir(parent)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let key = managed_backup_sort_key(stem, name.to_str()?)?;
            let path = entry.path();
            match crate::fs_ops::token_if_present(&path) {
                Ok(Some(token)) => Some((key, path, token)),
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!(
                        "preserving canonical auth backup {} because ownership could not be bound: {error:#}",
                        path.display()
                    );
                    None
                }
            }
        })
        .collect();

    if backups.len() <= MAX_BACKUPS {
        return;
    }

    backups.sort_by(|left, right| left.0.cmp(&right.0));
    let to_remove = backups.len() - MAX_BACKUPS;
    #[cfg(test)]
    run_before_backup_retention_delete_test_hook();
    for (_, old, token) in &backups[..to_remove] {
        if let Err(error) = remove_bound_path(old, token) {
            tracing::warn!(
                "preserving old auth backup {} because exact cleanup failed: {error:#}",
                old.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_recent_rfc3339(value: &serde_json::Value) {
        let text = value.as_str().expect("last_refresh should be a string");
        let parsed = chrono::DateTime::parse_from_rfc3339(text).expect("RFC3339 last_refresh");
        let age = chrono::Utc::now().signed_duration_since(parsed);
        assert!(
            age.num_seconds().abs() < 60,
            "last_refresh not recent: {text}"
        );
    }

    #[test]
    fn test_apply_tokens_updates_last_refresh() {
        let mut val = json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "old-id",
                "access_token": "old-access",
                "refresh_token": "old-refresh",
                "account_id": "acct"
            },
            "last_refresh": "2020-01-01T00:00:00Z"
        });

        apply_tokens(&mut val, "new-id", "new-access", "new-refresh").unwrap();

        assert_eq!(val["tokens"]["access_token"], "new-access");
        assert_recent_rfc3339(&val["last_refresh"]);
    }

    #[test]
    fn test_user_agent_matches_upstream_shape() {
        let ua = codex_user_agent();
        assert!(
            ua.starts_with("codex_cli_rs/0.144.1 ("),
            "unexpected UA: {ua}"
        );
        assert!(ua.ends_with(')'));
    }

    #[test]
    fn test_sanitize_proxy_url_masks_userinfo() {
        let url = "http://user:pass@example.com:8080/path?q=1";

        assert_eq!(
            sanitize_proxy_url(url),
            "http://***:***@example.com:8080/path?q=1"
        );
    }

    #[test]
    fn test_sanitize_proxy_url_keeps_url_without_userinfo() {
        let url = "socks5://example.com:1080";

        assert_eq!(sanitize_proxy_url(url), url);
    }

    #[cfg(unix)]
    #[test]
    fn test_write_auth_sets_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("auth.json");

        write_auth(&path, &json!({ "tokens": {} }))
            .unwrap()
            .assert_durably_published();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    fn backup_names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .filter(|name| name.starts_with("auth.json.bak."))
            .collect();
        names.sort();
        names
    }

    fn canonical_backup_name(stamp: u128, nonce_digit: char) -> String {
        format!(
            "auth.json.bak.{stamp}-{}",
            nonce_digit.to_string().repeat(32)
        )
    }

    /// Two scripted switches inside one second are ordinary. A
    /// second-resolution backup name made the later one overwrite
    /// the earlier, so the pre-switch credentials the user expected to be able
    /// to recover were gone and `MAX_BACKUPS` retained fewer real recovery
    /// points than it claims.
    #[test]
    fn two_conditional_publications_within_the_same_second_retain_both_backups() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("auth.json");

        write_auth(&path, &json!({ "tokens": { "refresh_token": "first" } }))
            .unwrap()
            .assert_durably_published();
        let first = std::fs::read(&path).unwrap();
        let second =
            serde_json::to_vec_pretty(&json!({ "tokens": { "refresh_token": "second" } })).unwrap();
        assert_eq!(
            atomic_write_private_if_unchanged(&path, Some(&first), &second).unwrap(),
            ConditionalWrite::Written
        );
        let second = std::fs::read(&path).unwrap();
        let third =
            serde_json::to_vec_pretty(&json!({ "tokens": { "refresh_token": "third" } })).unwrap();
        assert_eq!(
            atomic_write_private_if_unchanged(&path, Some(&second), &third).unwrap(),
            ConditionalWrite::Written
        );

        let names = backup_names(dir.path());
        assert_eq!(
            names.len(),
            2,
            "the first backup must survive a second one taken in the same second: {names:?}"
        );
    }

    /// Canonical transaction backups use an integer timestamp and fixed-width
    /// nonce. Parse the timestamp numerically so different widths cannot change
    /// retention order.
    #[test]
    fn cleanup_keeps_the_newest_backups_across_both_timestamp_widths() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth(&path, &json!({ "tokens": {} }))
            .unwrap()
            .assert_durably_published();

        for name in [
            canonical_backup_name(1_785_000_000, '0'),
            canonical_backup_name(1_785_000_001_000_000_000, '1'),
            canonical_backup_name(1_785_000_002_000_000_000, '2'),
            canonical_backup_name(1_785_000_003_000_000_000, '3'),
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }

        cleanup_old_backups(&path);

        assert_eq!(
            backup_names(dir.path()),
            vec![
                canonical_backup_name(1_785_000_001_000_000_000, '1'),
                canonical_backup_name(1_785_000_002_000_000_000, '2'),
                canonical_backup_name(1_785_000_003_000_000_000, '3'),
            ],
            "the numerically oldest canonical backup must be the one dropped"
        );
    }

    #[test]
    fn cleanup_preserves_manual_legacy_and_malformed_backup_names() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth(&path, &json!({ "tokens": {} }))
            .unwrap()
            .assert_durably_published();

        let managed = [
            canonical_backup_name(10, '0'),
            canonical_backup_name(20, '1'),
            canonical_backup_name(30, '2'),
            canonical_backup_name(40, '3'),
        ];
        for name in &managed {
            std::fs::write(dir.path().join(name), b"managed").unwrap();
        }
        let foreign = [
            "auth.json.bak.manual".to_string(),
            "auth.json.bak.1785000000".to_string(),
            format!("auth.json.bak.01-{}", "a".repeat(32)),
            format!("auth.json.bak.50-{}", "A".repeat(32)),
            format!("auth.json.bak.60-{}", "a".repeat(31)),
            format!("auth.json.bak.70-{}-extra", "a".repeat(32)),
            format!("auth.json.bak.-{}", "a".repeat(32)),
            format!(
                "auth.json.bak.340282366920938463463374607431768211456-{}",
                "a".repeat(32)
            ),
        ];
        for name in &foreign {
            std::fs::write(dir.path().join(name), b"foreign").unwrap();
        }

        cleanup_old_backups(&path);

        assert!(!dir.path().join(&managed[0]).exists());
        for name in &managed[1..] {
            assert!(
                dir.path().join(name).exists(),
                "missing managed backup {name}"
            );
        }
        for name in &foreign {
            assert_eq!(
                std::fs::read(dir.path().join(name)).unwrap(),
                b"foreign",
                "foreign backup-like entry must be preserved: {name}"
            );
        }
    }

    #[test]
    fn cleanup_does_not_delete_a_backup_replaced_after_eligibility_scan() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth(&path, &json!({ "tokens": {} }))
            .unwrap()
            .assert_durably_published();

        let managed = [
            canonical_backup_name(10, '0'),
            canonical_backup_name(20, '1'),
            canonical_backup_name(30, '2'),
            canonical_backup_name(40, '3'),
        ];
        for name in &managed {
            std::fs::write(dir.path().join(name), b"managed").unwrap();
        }
        let replaced = dir.path().join(&managed[0]);
        let replacement_path = replaced.clone();
        before_next_backup_retention_delete(move || {
            std::fs::write(replacement_path, b"foreign replacement").unwrap();
        });

        cleanup_old_backups(&path);

        assert_eq!(std::fs::read(replaced).unwrap(), b"foreign replacement");
        for name in &managed[1..] {
            assert!(dir.path().join(name).exists());
        }
    }

    #[test]
    fn independent_backup_stage_sets_private_permissions() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let source = dir.path().join("auth.json");
        let destination = dir.path().join(".auth.json.codex-switch-backup-test");

        write_auth(&source, &json!({ "tokens": {} }))
            .unwrap()
            .assert_durably_published();
        let expected = crate::fs_ops::token_for_path(&source).unwrap();
        let backup = create_independent_backup_stage(&source, &destination, &expected).unwrap();

        assert!(backup.token().same_contents(&expected));
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(backup.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn metadata_errors_are_not_treated_as_missing_auth_or_managed_config() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let blocked_parent = dir.path().join("not-a-directory");
        std::fs::write(&blocked_parent, b"x").unwrap();

        let auth_path = blocked_parent.join("auth.json");
        let auth_error = read_auth(&auth_path).unwrap_err();
        assert!(auth_error.to_string().contains("reading"), "{auth_error:#}");

        let config_error = load_codex_config(&blocked_parent).unwrap_err();
        assert!(
            config_error.to_string().contains("reading"),
            "{config_error:#}"
        );

        let valid_source = dir.path().join("valid-auth.json");
        write_auth(&valid_source, &json!({ "tokens": {} }))
            .unwrap()
            .assert_durably_published();
        let expected = crate::fs_ops::token_for_path(&valid_source).unwrap();
        let backup_destination = dir.path().join("recovery-copy");
        let backup_error =
            match create_independent_backup_stage(&auth_path, &backup_destination, &expected) {
                Ok(_) => panic!("an unreadable source path must not create a recovery copy"),
                Err(error) => error,
            };
        assert!(
            format!("{backup_error:#}").contains("creating independent auth recovery copy"),
            "{backup_error:#}"
        );
        assert!(!backup_destination.exists());
    }

    #[test]
    fn backup_stage_parent_errors_are_not_treated_as_available_slots() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let source = dir.path().join("auth.json");
        write_auth(&source, &json!({ "tokens": {} }))
            .unwrap()
            .assert_durably_published();
        let expected = crate::fs_ops::token_for_path(&source).unwrap();
        let blocked_parent = dir.path().join("not-a-directory");
        std::fs::write(&blocked_parent, b"x").unwrap();
        let destination = blocked_parent.join(".auth.json.codex-switch-backup-test");

        let error = match create_independent_backup_stage(&source, &destination, &expected) {
            Ok(_) => panic!("an invalid destination parent must not be treated as an empty slot"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("private")
                && format!("{error:#}").contains(&blocked_parent.display().to_string()),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&blocked_parent).unwrap(), b"x");
    }

    #[test]
    fn test_explicit_non_file_credentials_stores_are_rejected() {
        for mode in ["keyring", "auto", "ephemeral"] {
            let dir = crate::fs_ops::create_direct_tempdir().unwrap();
            std::fs::write(
                dir.path().join("config.toml"),
                format!("cli_auth_credentials_store = \"{mode}\"\n"),
            )
            .unwrap();

            let err = validate_cli_auth_credentials_store(dir.path()).unwrap_err();

            assert!(
                err.to_string()
                    .contains("cli_auth_credentials_store = \"file\"")
            );
        }
    }

    #[test]
    fn test_missing_credentials_store_defaults_to_file() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "model = \"gpt-5\"\n").unwrap();

        validate_cli_auth_credentials_store(dir.path()).unwrap();
    }

    #[test]
    fn test_explicit_file_credentials_store_is_allowed() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .unwrap();

        validate_cli_auth_credentials_store(dir.path()).unwrap();
    }

    #[test]
    fn test_empty_codex_home_falls_back_to_default_home() {
        let user_home = std::env::current_dir().unwrap().join("test-user-home");

        let codex_home =
            codex_home_from_values(Some(std::ffi::OsString::from("")), Some(user_home.clone()))
                .unwrap();

        assert_eq!(codex_home, user_home.join(".codex"));
    }

    #[test]
    fn derived_state_homes_reject_relative_user_home() {
        let relative = PathBuf::from("relative-user-home");

        for result in [
            codex_home_from_values(None, Some(relative.clone())),
            app_home_from_values(None, Some(relative.clone())),
        ] {
            let error = result.expect_err("a relative HOME would split locks by working directory");
            assert!(
                error.to_string().contains("must be an absolute path"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn derived_state_homes_reject_parent_components() {
        let ambiguous = std::env::current_dir()
            .unwrap()
            .join("home")
            .join("..")
            .join("other");

        for result in [
            codex_home_from_values(None, Some(ambiguous.clone())),
            app_home_from_values(None, Some(ambiguous.clone())),
        ] {
            let error = result.expect_err("HOME parent traversal makes lock roots ambiguous");
            assert!(error.to_string().contains("contains '..'"), "{error:#}");
        }
    }

    #[test]
    fn configured_state_homes_must_be_absolute_and_parent_free() {
        for name in ["CODEX_HOME", "CODEX_SWITCH_HOME"] {
            let relative = configured_home_path(name, OsString::from("relative/state"))
                .expect_err("relative state roots split process locks by working directory");
            assert!(
                relative.to_string().contains("must be an absolute path"),
                "{relative:#}"
            );

            let parent = std::env::current_dir()
                .unwrap()
                .join("state")
                .join("..")
                .join("other")
                .into_os_string();
            let parent = configured_home_path(name, parent)
                .expect_err("parent traversal makes the configured lock root ambiguous");
            assert!(parent.to_string().contains("contains '..'"), "{parent:#}");
        }
    }

    #[test]
    fn state_homes_reject_the_filesystem_root() {
        let root = std::env::current_dir()
            .expect("current directory")
            .ancestors()
            .last()
            .expect("current directory has a filesystem root")
            .to_path_buf();

        for (name, result) in [
            (
                "CODEX_HOME",
                configured_home_path("CODEX_HOME", root.clone().into_os_string()),
            ),
            (
                "CODEX_SWITCH_HOME",
                configured_home_path("CODEX_SWITCH_HOME", root.clone().into_os_string()),
            ),
            (
                "derived CODEX_HOME",
                codex_home_from_values(None, Some(root.clone())),
            ),
            (
                "derived CODEX_SWITCH_HOME",
                app_home_from_values(None, Some(root.clone())),
            ),
        ] {
            let error = result.expect_err("a filesystem root must never be a private state home");
            assert!(
                error.to_string().contains("cannot be a filesystem root"),
                "unexpected {name} error: {error:#}"
            );
        }
    }

    #[test]
    fn configured_state_homes_accept_clean_absolute_paths() {
        let path = std::env::current_dir()
            .unwrap()
            .join("state")
            .join("codex-switch")
            .into_os_string();

        let resolved = configured_home_path("CODEX_SWITCH_HOME", path).unwrap();
        assert!(resolved.is_absolute());
    }

    #[test]
    fn configured_state_homes_reject_existing_file_components() {
        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let file = root.path().join("not-a-directory");
        std::fs::write(&file, b"occupied").unwrap();

        let error = configured_home_path("CODEX_SWITCH_HOME", file.join("state").into_os_string())
            .expect_err("state paths must not traverse an existing file");

        assert!(
            error.to_string().contains("non-directory component"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_homes_reject_symlinked_nearest_ancestors_and_default_directories() {
        use std::os::unix::fs::symlink;

        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let linked = root.path().join("linked");
        symlink(&real, &linked).unwrap();

        let configured =
            configured_home_path("CODEX_SWITCH_HOME", linked.join("state").into_os_string())
                .expect_err("configured state paths must not traverse symlinks");
        assert!(configured.to_string().contains("symlink"), "{configured:#}");

        let user_home = root.path().join("user");
        std::fs::create_dir(&user_home).unwrap();
        symlink(&real, user_home.join(".codex")).unwrap();
        let derived = codex_home_from_values(None, Some(user_home))
            .expect_err("the derived credential directory must not be a symlink");
        assert!(derived.to_string().contains("symlink"), "{derived:#}");
    }

    #[cfg(unix)]
    #[test]
    fn state_homes_reject_existing_descendants_below_an_intermediate_symlink() {
        use std::os::unix::fs::symlink;

        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir_all(real.join("existing-child")).unwrap();
        let linked = root.path().join("linked");
        symlink(&real, &linked).unwrap();

        let error = configured_home_path(
            "CODEX_SWITCH_HOME",
            linked.join("existing-child").into_os_string(),
        )
        .expect_err("every existing path component must be inspected without following links");

        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert!(error.to_string().contains("linked"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_rejects_a_nonsticky_world_writable_ancestor() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let shared = root.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();

        let error = ensure_private_directory(&shared.join("private"))
            .expect_err("another user could replace a child in a non-sticky writable directory");

        assert!(error.to_string().contains("non-sticky"), "{error:#}");
        assert!(!shared.join("private").exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_allows_a_current_user_entry_in_a_sticky_parent() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let shared = root.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let private = shared.join("private");

        let _guard = acquire_private_directory(&private)
            .expect("sticky ownership protects the newly-created private entry");
        let metadata = std::fs::metadata(&private).unwrap();

        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }

    #[cfg(windows)]
    #[test]
    fn state_homes_reject_existing_descendants_below_an_intermediate_reparse_point() {
        use std::os::windows::fs::symlink_dir;

        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir_all(real.join("existing-child")).unwrap();
        let linked = root.path().join("linked");
        if let Err(error) = symlink_dir(&real, &linked) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("creating directory symlink for path validation test: {error}");
        }

        let error = configured_home_path(
            "CODEX_SWITCH_HOME",
            linked.join("existing-child").into_os_string(),
        )
        .expect_err("every existing path component must reject reparse points");

        assert!(error.to_string().contains("reparse-point"), "{error:#}");
        assert!(error.to_string().contains("linked"), "{error:#}");
    }

    #[cfg(windows)]
    #[test]
    fn private_directory_guard_pins_the_full_windows_path_until_drop() {
        let root = crate::fs_ops::create_direct_tempdir().unwrap();
        let parent = root.path().join("parent");
        let private = parent.join("private");
        let moved = root.path().join("moved-parent");
        let guard = acquire_private_directory(&private).expect("acquire private path capability");

        let error = std::fs::rename(&parent, &moved)
            .expect_err("an ancestor must not be renameable while the capability is live");
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Other
            ),
            "unexpected Windows sharing error: {error}"
        );
        assert!(private.is_dir());

        drop(guard);
        std::fs::rename(&parent, &moved)
            .expect("dropping the capability must release its namespace pins");
        assert!(moved.join("private").is_dir());
    }

    #[test]
    fn test_managed_auth_rejects_api_only_policy() {
        let config: toml::Value = toml::from_str("forced_login_method = \"api\"\n").unwrap();

        let err = validate_managed_auth_config(&config, Some("workspace-a")).unwrap_err();

        assert!(err.to_string().contains("requires API key login"));
    }

    #[test]
    fn test_managed_auth_enforces_workspace_list() {
        let config: toml::Value = toml::from_str(
            "forced_login_method = \"chatgpt\"\nforced_chatgpt_workspace_id = [\"workspace-a\", \"workspace-b\"]\n",
        )
        .unwrap();

        validate_managed_auth_config(&config, Some("workspace-b")).unwrap();
        let err = validate_managed_auth_config(&config, Some("workspace-c")).unwrap_err();

        assert!(err.to_string().contains("workspace-c"));
    }

    #[test]
    fn windows_acl_sddl_replaces_the_dacl_instead_of_only_removing_inheritance() {
        let sddl = windows_private_acl_sddl("S-1-5-21-1-2-3-1001", true);
        assert!(sddl.starts_with("D:P"));
        assert_eq!(sddl.matches("(A;").count(), 3);
        assert!(
            !sddl.contains("S-1-1-0"),
            "the exact DACL path must not preserve unknown explicit ACEs"
        );
    }

    #[test]
    fn windows_private_owner_accepts_only_the_token_user_or_default_owner() {
        let user = "S-1-5-21-1-2-3-1001";
        let default_owner = "S-1-5-32-544";

        assert!(super::windows_owner_sid_matches_token(
            user,
            user,
            default_owner
        ));
        assert!(super::windows_owner_sid_matches_token(
            default_owner,
            user,
            default_owner
        ));
        assert!(!super::windows_owner_sid_matches_token(
            "S-1-5-21-9-9-9-1002",
            user,
            default_owner
        ));
    }

    #[test]
    fn test_custom_ca_prefers_codex_ca_and_ignores_empty_values() {
        let selected = custom_ca_path_from_values(
            Some(OsString::from("/certs/codex.pem")),
            Some(OsString::from("/certs/ssl.pem")),
        );
        assert_eq!(selected, Some(PathBuf::from("/certs/codex.pem")));

        let fallback = custom_ca_path_from_values(
            Some(OsString::from("")),
            Some(OsString::from("/certs/ssl.pem")),
        );
        assert_eq!(fallback, Some(PathBuf::from("/certs/ssl.pem")));
    }

    #[test]
    fn test_redact_sensitive_log_body_masks_nested_keys() {
        let body = json!({
            "data": {
                "access_token": "secret",
                "items": [
                    { "refresh_token": "r" },
                    { "keep": "value" }
                ]
            },
            "access_token": "top",
            "keep_top": "value"
        });

        let redacted: serde_json::Value =
            serde_json::from_str(&redact_sensitive_log_body(&body)).unwrap();

        assert_eq!(redacted["access_token"], "***");
        assert_eq!(redacted["data"]["access_token"], "***");
        assert_eq!(redacted["data"]["items"][0]["refresh_token"], "***");
        assert_eq!(redacted["data"]["items"][1]["keep"], "value");
        assert_eq!(redacted["keep_top"], "value");
    }

    #[test]
    fn unknown_issuer_error_explains_how_to_trust_an_intercepting_proxy() {
        let msg = "Usage API request failed: error sending request\n  caused by: invalid peer certificate: UnknownIssuer";
        let hint = super::tls_trust_hint(msg).expect("UnknownIssuer must carry a hint");
        assert!(
            hint.contains("CODEX_CA_CERTIFICATE"),
            "the hint must name the variable that fixes it: {hint}"
        );
    }

    #[test]
    fn an_ordinary_connection_failure_gets_no_certificate_hint() {
        let msg = "Usage API request failed: error sending request\n  caused by: tcp connect error: Connection refused (os error 61)";
        assert!(
            super::tls_trust_hint(msg).is_none(),
            "a hint about certificates would misdirect a plain connection failure"
        );
    }

    #[test]
    fn windows_private_acl_sddl_is_exact_and_language_neutral() {
        let current_user = "S-1-5-21-1-2-3-1001";
        assert_eq!(
            super::windows_private_acl_sddl(current_user, false),
            "D:P(A;;FA;;;S-1-5-21-1-2-3-1001)\
             (A;;FA;;;S-1-5-18)\
             (A;;FA;;;S-1-5-32-544)"
        );
        assert_eq!(
            super::windows_private_acl_sddl(current_user, true),
            "D:P(A;OICI;FA;;;S-1-5-21-1-2-3-1001)\
             (A;OICI;FA;;;S-1-5-18)\
             (A;OICI;FA;;;S-1-5-32-544)"
        );
    }

    #[test]
    fn conditional_private_write_never_publishes_after_a_detected_change() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let existing = dir.path().join("auth.json");
        std::fs::write(&existing, b"original").unwrap();

        assert_eq!(
            super::atomic_write_private_if_unchanged(&existing, Some(b"different"), b"replacement")
                .unwrap(),
            super::ConditionalWrite::Changed
        );
        assert_eq!(std::fs::read(&existing).unwrap(), b"original");

        assert_eq!(
            super::atomic_write_private_if_unchanged(&existing, Some(b"original"), b"replacement")
                .unwrap(),
            super::ConditionalWrite::Written
        );
        assert_eq!(std::fs::read(&existing).unwrap(), b"replacement");

        let missing = dir.path().join("new-auth.json");
        assert_eq!(
            super::atomic_write_private_if_unchanged(&missing, None, b"created").unwrap(),
            super::ConditionalWrite::Written
        );
        assert_eq!(
            super::atomic_write_private_if_unchanged(&missing, None, b"clobber").unwrap(),
            super::ConditionalWrite::Changed
        );
        assert_eq!(std::fs::read(&missing).unwrap(), b"created");
    }

    #[test]
    fn conditional_publication_preserves_a_writer_that_wins_before_exchange() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let live = dir.path().join("auth.json");
        std::fs::write(&live, b"original").unwrap();
        std::fs::write(dir.path().join("auth.json.bak.1"), b"retained").unwrap();
        let backups_before = backup_names(dir.path());

        let writer_path = live.clone();
        super::before_next_auth_exchange(move || {
            std::fs::write(writer_path, b"foreign").unwrap();
        });

        assert_eq!(
            super::atomic_write_private_if_unchanged(&live, Some(b"original"), b"replacement")
                .unwrap(),
            super::ConditionalWrite::Changed
        );
        assert_eq!(std::fs::read(&live).unwrap(), b"foreign");
        assert_eq!(
            backup_names(dir.path()),
            backups_before,
            "an aborted conditional publication must not change backup retention"
        );
        assert!(super::read_publication_record(&live).unwrap().is_none());
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
                .all(|name| !name.contains(".codex-switch-candidate-")
                    && !name.contains(".codex-switch-backup-")
                    && !name.contains(".codex-switch-displaced-"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_foreign_restore_never_overwrites_a_late_candidate_path_occupant() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let live = dir.path().join("auth.json");
        let candidate = dir.path().join(".auth.json.codex-switch-candidate-test");
        let displaced = dir.path().join(".auth.json.codex-switch-displaced-test");
        let backup_stage = dir.path().join(".auth.json.codex-switch-backup-test");
        let backup = dir.path().join("auth.json.bak.test");
        let record = dir.path().join(".auth.json.codex-switch-publication");

        std::fs::write(&live, b"candidate").unwrap();
        std::fs::write(&candidate, b"late-foreign-occupant").unwrap();
        std::fs::write(&displaced, b"foreign-live").unwrap();
        std::fs::write(&backup_stage, b"original").unwrap();
        std::fs::write(&record, b"record").unwrap();

        let parsed = super::ParsedAuthPublication {
            candidate: candidate.clone(),
            displaced: displaced.clone(),
            backup_stage,
            backup,
            record: record.clone(),
            expected_token: crate::fs_ops::token_for_path(&live).unwrap(),
            candidate_token: crate::fs_ops::token_for_path(&live).unwrap(),
            backup_token: crate::fs_ops::token_for_path(&displaced).unwrap(),
            record_token: crate::fs_ops::token_for_path(&record).unwrap(),
        };
        let foreign = crate::fs_ops::token_for_path(&displaced).unwrap();

        let error = super::restore_foreign_live(&live, &parsed, &foreign).unwrap_err();
        assert!(format!("{error:#}").contains("no longer empty"));
        assert_eq!(std::fs::read(&live).unwrap(), b"candidate");
        assert_eq!(std::fs::read(&displaced).unwrap(), b"foreign-live");
        assert_eq!(
            std::fs::read(&candidate).unwrap(),
            b"late-foreign-occupant",
            "the random restore destination must remain no-clobber"
        );
    }

    #[test]
    fn an_old_live_handle_cannot_mutate_the_independent_backup() {
        use std::io::{Seek as _, SeekFrom};

        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let live = dir.path().join("auth.json");
        std::fs::write(&live, b"original").unwrap();
        let mut old_live = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&live)
            .unwrap();
        super::after_next_auth_exchange(move || {
            old_live.seek(SeekFrom::Start(0)).unwrap();
            old_live.write_all(b"mutated").unwrap();
            old_live.set_len(b"mutated".len() as u64).unwrap();
            old_live.sync_all().unwrap();
        });

        let result =
            super::atomic_write_private_if_unchanged(&live, Some(b"original"), b"replacement")
                .unwrap();
        assert!(matches!(
            result,
            super::ConditionalWrite::PublishedRecoveryRequired(_)
        ));
        assert_eq!(std::fs::read(&live).unwrap(), b"replacement");

        let (record, record_token) = super::read_publication_record(&live)
            .unwrap()
            .expect("a side-effect-aware result must retain the recovery record");
        let parsed = super::parse_publication_record(&live, record, record_token).unwrap();
        assert_eq!(std::fs::read(&parsed.backup_stage).unwrap(), b"original");
        assert_eq!(std::fs::read(&parsed.displaced).unwrap(), b"mutated");
        assert!(
            super::recover_interrupted_auth_publication(&live).is_err(),
            "recovery without the original open-handle witness must preserve both artifacts"
        );
        assert_eq!(std::fs::read(&live).unwrap(), b"replacement");
        assert_eq!(std::fs::read(&parsed.backup_stage).unwrap(), b"original");
        assert_eq!(std::fs::read(&parsed.displaced).unwrap(), b"mutated");
    }

    #[test]
    fn publication_record_cannot_redirect_transaction_paths() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let live = dir.path().join("auth.json");
        let unrelated = dir.path().join("unrelated.json");
        std::fs::write(&live, b"original").unwrap();
        std::fs::write(&unrelated, b"do-not-touch").unwrap();
        let token = crate::fs_ops::token_for_path(&live).unwrap().to_string();
        let record_path = super::auth_publication_record_path(&live).unwrap();
        std::fs::write(
            &record_path,
            serde_json::to_vec(&json!({
                "version": super::AUTH_PUBLICATION_RECORD_VERSION,
                "nonce": "../unrelated.json",
                "backup_stamp": "1",
                "expected_token": token,
                "candidate_token": token,
                "backup_token": token,
                "candidate_name": "unrelated.json"
            }))
            .unwrap(),
        )
        .unwrap();

        let error = super::recover_interrupted_auth_publication(&live).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("unknown field") || message.contains("invalid transaction nonce"),
            "unexpected validation error: {message}"
        );
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"do-not-touch");
        assert!(record_path.exists(), "a damaged record must be preserved");
    }

    #[test]
    fn oversized_publication_record_is_bounded_and_preserved() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let live = dir.path().join("auth.json");
        std::fs::write(&live, b"original").unwrap();
        let record_path = super::auth_publication_record_path(&live).unwrap();
        let oversized = vec![b'x'; super::MAX_AUTH_PUBLICATION_RECORD_BYTES + 1];
        std::fs::write(&record_path, &oversized).unwrap();

        let error = super::recover_interrupted_auth_publication(&live).unwrap_err();

        assert!(format!("{error:#}").contains("schema limit"));
        assert_eq!(std::fs::read(&live).unwrap(), b"original");
        assert_eq!(std::fs::read(&record_path).unwrap(), oversized);
    }

    #[test]
    fn successful_exchange_retains_an_independent_original_backup() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let live = dir.path().join("auth.json");
        std::fs::write(&live, b"original").unwrap();

        assert_eq!(
            super::atomic_write_private_if_unchanged(&live, Some(b"original"), b"replacement")
                .unwrap(),
            super::ConditionalWrite::Written
        );
        let backups = backup_names(dir.path());
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read(dir.path().join(&backups[0])).unwrap(),
            b"original"
        );
        assert!(super::read_publication_record(&live).unwrap().is_none());
    }

    #[test]
    fn exclusive_backup_stage_never_adopts_or_overwrites_a_collision() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let live = dir.path().join("auth.json");
        let collision = dir.path().join(".auth.json.codex-switch-backup-collision");
        std::fs::write(&live, b"original").unwrap();
        std::fs::write(&collision, b"foreign").unwrap();
        let expected = crate::fs_ops::token_for_path(&live).unwrap();

        assert!(super::create_independent_backup_stage(&live, &collision, &expected).is_err());
        assert_eq!(std::fs::read(&collision).unwrap(), b"foreign");
    }

    #[test]
    fn private_write_durability_contract_includes_new_directory_ancestors() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let path = dir.path().join("new/app/profiles/alice/auth.json");

        super::atomic_write_private(&path, b"credential")
            .unwrap()
            .assert_durably_published();

        assert_eq!(std::fs::read(path).unwrap(), b"credential");
    }

    #[cfg(windows)]
    #[test]
    fn atomic_private_write_removes_unknown_explicit_windows_aces() {
        let dir = crate::fs_ops::create_direct_tempdir().unwrap();
        let status = std::process::Command::new("icacls")
            .arg(dir.path())
            .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
            .status()
            .unwrap();
        assert!(status.success(), "failed to seed an Everyone ACE");

        let path = dir.path().join("auth.json");
        super::atomic_write_private(&path, br#"{"refresh_token":"secret"}"#)
            .unwrap()
            .assert_durably_published();

        let inspect = r#"
$ErrorActionPreference = 'Stop'
foreach ($item in @($env:CS_ACL_DIR, $env:CS_ACL_FILE)) {
    $acl = if (Test-Path -LiteralPath $item -PathType Container) {
        [IO.Directory]::GetAccessControl($item)
    } else {
        [IO.File]::GetAccessControl($item)
    }
    Write-Output ('protected=' + $acl.AreAccessRulesProtected)
    foreach ($rule in $acl.Access) {
        Write-Output $rule.IdentityReference.Translate(
            [Security.Principal.SecurityIdentifier]
        ).Value
    }
}
"#;
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                inspect,
            ])
            .env("CS_ACL_DIR", dir.path())
            .env("CS_ACL_FILE", &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ACL inspection failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let acl = String::from_utf8(output.stdout).unwrap();
        assert_eq!(acl.matches("protected=True").count(), 2);
        assert!(
            !acl.lines().any(|line| line.trim() == "S-1-1-0"),
            "Everyone ACE survived exact DACL replacement:\n{acl}"
        );
    }
}
