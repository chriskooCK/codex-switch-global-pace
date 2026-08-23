# ADR 0001: Platform-native user-level direct installation

## Status

Accepted on 2026-07-13.

## Context

The Unix installer placed `codex-switch-global-pace` in `/usr/local/bin`. On systems where that directory is owned by `root`, installation and every later self-update required `sudo`. The documented plain `codex-switch-global-pace self-update` workflow therefore did not match the default direct-install experience. Windows already used a user-owned directory and user PATH.

The installation must remain checksum-verified, avoid changing ownership of shared system directories, and keep daemon-service privileges separate from ordinary CLI installation.

## Options considered

### System-wide direct installation by default

- Keeps the executable in a traditional global PATH location.
- Requires elevated installation and updates on common Unix systems.
- Runs more of the updater with elevated privileges than necessary.

### Platform-native user-level direct installation by default

- macOS and Linux install to `$HOME/.local/bin`.
- Windows installs to `%LOCALAPPDATA%\Programs\codex-switch-global-pace` and updates user PATH.
- An explicit Unix `--system` option retains `/usr/local/bin` for administrators.
- This project currently publishes direct GitHub Release installations only.

### Package-manager-only installation

- Provides native stable-package ownership.
- Does not cover rolling development releases or every supported Linux environment.

## Decision

Use platform-native user-level direct installation by default. Keep an explicit Unix system-wide mode. Detect legacy `/usr/local/bin/codex-switch-global-pace` installations and require a one-time migration that prevents the old executable from continuing to shadow the user installation.

The self-updater must check whether it can replace the current executable before downloading an archive. Windows Task Scheduler daemon installation may still require administrator privileges; that is independent of CLI installation ownership.

## Consequences

- New direct installations can self-update without elevation.
- Existing Unix system installations need one final privileged cleanup or must continue using system mode.
- Installer tests must cover destination selection, PATH guidance, legacy conflicts, and checksum ordering.
- Documentation must distinguish direct, system-wide, and package-manager update commands.
