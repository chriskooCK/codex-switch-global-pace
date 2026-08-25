use std::ops::Deref;
use std::path::Path;

/// A test directory created beneath the physical identity of the OS temp root.
///
/// macOS commonly exposes `/var` as a symlink to `/private/var`. Tests that
/// exercise direct, no-symlink filesystem transactions must receive the
/// canonical root up front rather than discover the alias after creating
/// files beneath it.
pub struct TempDir(tempfile::TempDir);

impl TempDir {
    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.path()
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

pub fn tempdir() -> TempDir {
    let physical_root = std::fs::canonicalize(std::env::temp_dir())
        .expect("resolve the physical operating-system temp root");
    let directory = tempfile::Builder::new()
        .prefix("codex-switch-global-pace-test-")
        .tempdir_in(physical_root)
        .expect("create a test directory beneath the physical temp root");
    TempDir(directory)
}
