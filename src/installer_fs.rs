use crate::fs_ops::{self, FileToken};
use anyhow::{Context, Result};
use std::path::Path;
use std::str::FromStr as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum InstallerFileOperation {
    Token,
    CopyExclusive,
    CreateEmptyExclusive,
    MoveNoreplace,
    Exchange,
    ReplaceWithDisplaced,
    RemoveOwned,
    UserPathSnapshot,
    UserPathAdd,
    UserPathRemove,
    UserPathRestore,
}

pub(crate) fn execute(
    operation: InstallerFileOperation,
    source: Option<&Path>,
    destination: Option<&Path>,
    displaced: Option<&Path>,
    expected_token: Option<&str>,
    expected_destination_token: Option<&str>,
) -> Result<String> {
    match operation {
        InstallerFileOperation::Token => {
            Ok(fs_ops::token_for_path(required_path(source, "source")?)?.to_string())
        }
        InstallerFileOperation::CopyExclusive => {
            format_create_outcome(fs_ops::create_exclusive_copy(
                required_path(source, "source")?,
                required_path(destination, "destination")?,
                &required_token(expected_token, "expected-token")?,
            )?)
        }
        InstallerFileOperation::CreateEmptyExclusive => format_create_outcome(
            fs_ops::create_empty_exclusive(required_path(destination, "destination")?)?,
        ),
        InstallerFileOperation::MoveNoreplace => move_noreplace_exact(
            required_path(source, "source")?,
            required_path(destination, "destination")?,
            &required_token(expected_token, "expected-token")?,
        ),
        InstallerFileOperation::Exchange => exchange_exact(
            required_path(source, "source")?,
            required_path(destination, "destination")?,
            &required_token(expected_token, "expected-token")?,
            &required_token(expected_destination_token, "expected-destination-token")?,
        ),
        InstallerFileOperation::ReplaceWithDisplaced => replace_with_displaced_exact(
            required_path(source, "source")?,
            required_path(destination, "destination")?,
            required_path(displaced, "displaced")?,
            &required_token(expected_token, "expected-token")?,
            &required_token(expected_destination_token, "expected-destination-token")?,
        ),
        InstallerFileOperation::RemoveOwned => {
            let outcome = fs_ops::remove_exact(
                required_path(source, "source")?,
                &required_token(expected_token, "expected-token")?,
            )?;
            Ok(match outcome {
                fs_ops::RemoveExactOutcome::Removed => "removed",
                fs_ops::RemoveExactOutcome::RemovedNamespaceDurabilityUnconfirmed => {
                    "removed-namespace-durability-unconfirmed"
                }
            }
            .to_string())
        }
        InstallerFileOperation::UserPathSnapshot => crate::installer_registry::snapshot(),
        InstallerFileOperation::UserPathAdd => crate::installer_registry::transition(
            crate::installer_registry::PathTransition::Add,
            required_path(source, "source")?,
        ),
        InstallerFileOperation::UserPathRemove => crate::installer_registry::transition(
            crate::installer_registry::PathTransition::Remove,
            required_path(source, "source")?,
        ),
        InstallerFileOperation::UserPathRestore => crate::installer_registry::transition(
            crate::installer_registry::PathTransition::Restore,
            required_path(source, "source")?,
        ),
    }
}

fn format_create_outcome(outcome: fs_ops::CreateExactOutcome) -> Result<String> {
    Ok(match outcome {
        fs_ops::CreateExactOutcome::Created(token) => {
            format!("created|{token}")
        }
        fs_ops::CreateExactOutcome::CreatedNamespaceDurabilityUnconfirmed(token) => {
            format!("created-namespace-durability-unconfirmed|{token}")
        }
    })
}

fn required_path<'a>(value: Option<&'a Path>, name: &str) -> Result<&'a Path> {
    value.with_context(|| format!("internal installer file operation requires --{name}"))
}

fn required_token(value: Option<&str>, name: &str) -> Result<FileToken> {
    let token =
        value.with_context(|| format!("internal installer file operation requires --{name}"))?;
    if token.is_empty() || token.contains('\n') || token.contains('\r') {
        anyhow::bail!("internal installer file token is empty or malformed");
    }
    FileToken::from_str(token).with_context(|| format!("parsing --{name}"))
}

fn move_noreplace_exact(source: &Path, destination: &Path, expected: &FileToken) -> Result<String> {
    let observed = fs_ops::token_for_path(source)?;
    if &observed != expected {
        anyhow::bail!(
            "move source changed before no-replace publication: {}",
            source.display()
        );
    }
    let boundary = fs_ops::rename_noreplace(source, destination);
    let source_after = fs_ops::token_if_present(source)?;
    let destination_after = fs_ops::token_if_present(destination)?;
    if source_after.is_none() && destination_after.as_ref() == Some(expected) {
        return match boundary {
            Ok(()) => Ok(expected.to_string()),
            Err(error) => Err(error.context(format!(
                "no-replace rename moved the exact file to {}, but its durability boundary failed",
                destination.display()
            ))),
        };
    }
    if source_after.as_ref() == Some(expected) && destination_after.is_none() {
        return Err(boundary.err().unwrap_or_else(|| {
            anyhow::anyhow!("no-replace rename returned success without moving the source")
        }));
    }
    anyhow::bail!(
        "no-replace rename ended in an unclassified state; source and destination were preserved"
    )
}

#[cfg(unix)]
fn exchange_exact(
    source: &Path,
    destination: &Path,
    expected_source: &FileToken,
    expected_destination: &FileToken,
) -> Result<String> {
    if fs_ops::token_for_path(source)? != *expected_source
        || fs_ops::token_for_path(destination)? != *expected_destination
    {
        anyhow::bail!("exchange operands changed before the atomic publication boundary");
    }
    let boundary = fs_ops::exchange(source, destination);
    let boundary_error = boundary.as_ref().err().map(|error| format!("{error:#}"));
    let actual_source = fs_ops::token_if_present(source)?;
    let actual_destination = fs_ops::token_if_present(destination)?;
    let exact_swap = actual_source.as_ref() == Some(expected_destination)
        && actual_destination.as_ref() == Some(expected_source);
    if boundary.is_ok() && exact_swap {
        return Ok(expected_source.to_string());
    }
    if boundary.is_err()
        && actual_source.as_ref() == Some(expected_source)
        && actual_destination.as_ref() == Some(expected_destination)
    {
        anyhow::bail!(
            "atomic exchange failed without changing either operand: {}",
            boundary_error.unwrap_or_else(|| "unknown exchange failure".to_string())
        );
    }

    let source_anchor = actual_source.as_ref() == Some(expected_destination);
    let destination_anchor = actual_destination.as_ref() == Some(expected_source);
    if source_anchor || destination_anchor {
        if fs_ops::token_if_present(source)? != actual_source
            || fs_ops::token_if_present(destination)? != actual_destination
        {
            anyhow::bail!(
                "atomic exchange recovery was not attempted because an operand changed again; every path was preserved"
            );
        }
        let restoration = fs_ops::exchange(source, destination);
        let restored_source = fs_ops::token_if_present(source)?;
        let restored_destination = fs_ops::token_if_present(destination)?;
        if restored_source == actual_destination && restored_destination == actual_source {
            let restoration_note = restoration
                .err()
                .map(|error| format!("; restoration durability failed: {error:#}"))
                .unwrap_or_default();
            anyhow::bail!(
                "atomic exchange did not commit its expected state; the two actual operands were restored{restoration_note}"
            );
        }
        anyhow::bail!(
            "atomic exchange recovery ended in an unclassified state; every path was preserved"
        );
    }
    anyhow::bail!(
        "atomic exchange encountered unclassifiable concurrent changes; every path was preserved"
    )
}

#[cfg(not(unix))]
fn exchange_exact(
    _source: &Path,
    _destination: &Path,
    _expected_source: &FileToken,
    _expected_destination: &FileToken,
) -> Result<String> {
    anyhow::bail!("atomic exchange is supported only on Linux and macOS")
}

#[cfg(windows)]
fn replace_with_displaced_exact(
    replacement: &Path,
    destination: &Path,
    displaced: &Path,
    expected_replacement: &FileToken,
    expected_destination: &FileToken,
) -> Result<String> {
    if fs_ops::token_for_path(replacement)? != *expected_replacement
        || fs_ops::token_for_path(destination)? != *expected_destination
    {
        anyhow::bail!("ReplaceFileW operands changed before the atomic publication boundary");
    }
    // The script supplies a CSPRNG sibling name. This absence check rejects a
    // normal collision; ReplaceFileW is not advertised as strict namespace CAS
    // against a non-cooperating writer in the same Windows user session.
    if fs_ops::token_if_present(displaced)?.is_some() {
        anyhow::bail!(
            "displaced recovery path already exists: {}",
            displaced.display()
        );
    }
    let boundary = fs_ops::replace_with_displaced(replacement, destination, displaced);
    let boundary_error = boundary.as_ref().err().map(|error| format!("{error:#}"));
    let actual_replacement = fs_ops::token_if_present(replacement)?;
    let actual_destination = fs_ops::token_if_present(destination)?;
    let actual_displaced = fs_ops::token_if_present(displaced)?;
    let exact_publication = actual_replacement.is_none()
        && actual_destination.as_ref() == Some(expected_replacement)
        && actual_displaced.as_ref() == Some(expected_destination);
    if boundary.is_ok() && exact_publication {
        return Ok("replaced".to_string());
    }
    if boundary.is_err()
        && actual_replacement.as_ref() == Some(expected_replacement)
        && actual_destination.as_ref() == Some(expected_destination)
        && actual_displaced.is_none()
    {
        anyhow::bail!(
            "ReplaceFileW failed without changing any operand: {}",
            boundary_error.unwrap_or_else(|| "unknown ReplaceFileW failure".to_string())
        );
    }

    if actual_destination.is_none()
        && actual_displaced.is_some()
        && actual_replacement.as_ref() == Some(expected_replacement)
    {
        if fs_ops::token_if_present(replacement)? != actual_replacement
            || fs_ops::token_if_present(destination)? != actual_destination
            || fs_ops::token_if_present(displaced)? != actual_displaced
        {
            anyhow::bail!(
                "ReplaceFileW partial-failure recovery was not attempted because a path changed again; every path was preserved"
            );
        }
        let restoration = fs_ops::rename_noreplace(displaced, destination);
        let restored_replacement = fs_ops::token_if_present(replacement)?;
        let restored_destination = fs_ops::token_if_present(destination)?;
        let restored_displaced = fs_ops::token_if_present(displaced)?;
        if restored_replacement == actual_replacement
            && restored_destination == actual_displaced
            && restored_displaced.is_none()
        {
            let restoration_note = restoration
                .err()
                .map(|error| format!("; restoration durability failed: {error:#}"))
                .unwrap_or_default();
            anyhow::bail!(
                "ReplaceFileW failed after displacing the actual destination; that destination was restored without replacement{restoration_note}"
            );
        }
        anyhow::bail!(
            "ReplaceFileW partial-failure recovery ended in an unclassified state; every path was preserved"
        );
    }

    let destination_anchor = actual_destination.as_ref() == Some(expected_replacement)
        || actual_destination.as_ref() == Some(expected_destination);
    let displaced_anchor = actual_displaced.as_ref() == Some(expected_destination);
    if actual_replacement.is_none()
        && actual_destination.is_some()
        && actual_displaced.is_some()
        && (destination_anchor || displaced_anchor)
    {
        if fs_ops::token_if_present(replacement)? != actual_replacement
            || fs_ops::token_if_present(destination)? != actual_destination
            || fs_ops::token_if_present(displaced)? != actual_displaced
        {
            anyhow::bail!(
                "ReplaceFileW recovery was not attempted because an operand changed again; every path was preserved"
            );
        }
        let restoration = fs_ops::replace_with_displaced(displaced, destination, replacement);
        let restored_replacement = fs_ops::token_if_present(replacement)?;
        let restored_destination = fs_ops::token_if_present(destination)?;
        let restored_displaced = fs_ops::token_if_present(displaced)?;
        if restored_replacement == actual_destination
            && restored_destination == actual_displaced
            && restored_displaced.is_none()
        {
            let restoration_note = restoration
                .err()
                .map(|error| format!("; restoration boundary failed: {error:#}"))
                .unwrap_or_default();
            anyhow::bail!(
                "ReplaceFileW did not commit its expected state; the actual destination and replacement were restored{restoration_note}"
            );
        }
        anyhow::bail!(
            "ReplaceFileW recovery ended in an unclassified state; every path was preserved"
        );
    }

    anyhow::bail!(
        "ReplaceFileW ended in an unclassified state; replacement, public, and displaced paths were preserved"
    )
}

#[cfg(not(windows))]
fn replace_with_displaced_exact(
    _replacement: &Path,
    _destination: &Path,
    _displaced: &Path,
    _expected_replacement: &FileToken,
    _expected_destination: &FileToken,
) -> Result<String> {
    anyhow::bail!("ReplaceFileW publication is supported only on Windows")
}
