#!/usr/bin/env bash
set -euo pipefail

# codex-switch-global-pace installer / uninstaller for macOS and Linux
# Usage:
#   curl -fsSL https://github.com/chriskooCK/codex-switch-global-pace/releases/latest/download/install.sh | bash
#   curl -fsSL https://github.com/chriskooCK/codex-switch-global-pace/releases/download/dev/install.sh | bash -s -- --dev
#   curl -fsSL .../install.sh | bash -s -- --system       # install system-wide (may require sudo)
#   curl -fsSL .../install.sh | bash -s -- --uninstall    # uninstall this program
#   curl -fsSL .../install.sh | CS_VERSION=20260712.1.0 bash  # install specific version

REPO="chriskooCK/codex-switch-global-pace"
# Release workflow replaces this value in the installer asset. Keeping the
# source value empty makes a raw checkout fail closed instead of guessing which
# release version a downloaded archive ought to contain.
PACKAGED_RELEASE_VERSION=""
USER_INSTALL_DIR="${HOME}/.local/bin"
SYSTEM_INSTALL_DIR="/usr/local/bin"
BINARY_NAME="codex-switch-global-pace"
DATA_DIR="${HOME}/.codex-switch"
LEGACY_BIN="${SYSTEM_INSTALL_DIR}/${BINARY_NAME}"
SYSTEM_INSTALL_MARKER="${SYSTEM_INSTALL_DIR}/.codex-switch-global-pace-system-install-v1"
PATH_BLOCK_BEGIN="# >>> codex-switch-global-pace PATH >>>"
PATH_BLOCK_END="# <<< codex-switch-global-pace PATH <<<"

info()  { printf '\033[0;34m[info]\033[0m  %s\n' "$*"; }
warn()  { printf '\033[0;33m[warn]\033[0m  %s\n' "$*" >&2; }
error() { printf '\033[0;31m[error]\033[0m %s\n' "$*" >&2; exit 1; }

SEMVER_PATTERN='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?(\+([0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*))?$'

validate_version() {
  local version="$1"
  [[ "$version" =~ $SEMVER_PATTERN ]] || error "Invalid CS_VERSION '${version}'; expected a SemVer version such as 20260824.6.0."
}

is_homebrew_cellar_path() {
  case "$1" in
    */Cellar/codex-switch-global-pace/*) return 0 ;;
    *) return 1 ;;
  esac
}

classify_legacy_binary() {
  LEGACY_RESOLVED="$(resolve_path_target "$LEGACY_BIN")"
  if is_homebrew_cellar_path "$LEGACY_RESOLVED"; then
    LEGACY_KIND="homebrew"
  else
    LEGACY_KIND="direct"
  fi
}

legacy_service_references_binary() {
  LEGACY_SERVICE_PATH=""
  case "$OS" in
    darwin)
      local plist_path="${HOME}/Library/LaunchAgents/com.codex-switch-global-pace.daemon.plist"
      if [ -f "$plist_path" ] && grep -Fq "<string>${LEGACY_BIN}</string>" "$plist_path"; then
        LEGACY_SERVICE_PATH="$plist_path"
        return 0
      fi
      ;;
    linux)
      local unit_path="${HOME}/.config/systemd/user/codex-switch-global-pace-daemon.service"
      if [ -f "$unit_path" ] && grep -Fqx "ExecStart=\"${LEGACY_BIN}\" daemon start --foreground" "$unit_path"; then
        LEGACY_SERVICE_PATH="$unit_path"
        return 0
      fi
      ;;
  esac
  return 1
}

legacy_service_is_running() {
  case "$OS" in
    darwin)
      launchctl list com.codex-switch-global-pace.daemon >/dev/null 2>&1
      ;;
    linux)
      systemctl --user is-active --quiet codex-switch-global-pace-daemon
      ;;
    *)
      return 1
      ;;
  esac
}

verify_candidate_version() {
  local candidate="$1" expected="$2" output first_line
  if ! output="$("$candidate" --version 2>&1)"; then
    CANDIDATE_ERROR="candidate version check failed: ${output}"
    return 1
  fi
  first_line="${output%%$'\n'*}"
  first_line="${first_line%$'\r'}"
  if [ "$first_line" != "${BINARY_NAME} ${expected}" ]; then
    CANDIDATE_ERROR="candidate reported '${first_line}', expected '${BINARY_NAME} ${expected}'"
    return 1
  fi
  return 0
}

run_install_fs() {
  if [ "${INSTALL_WITH_SUDO:-false}" = true ]; then
    sudo "$@"
  else
    "$@"
  fi
}

cleanup_install_artifacts() {
  if [ -n "${INSTALL_STAGE:-}" ]; then
    run_install_fs rm -f "$INSTALL_STAGE" >/dev/null 2>&1 || true
  fi
  if [ -n "${INSTALL_BACKUP:-}" ]; then
    run_install_fs rm -f "$INSTALL_BACKUP" >/dev/null 2>&1 || true
  fi
}

rollback_installed_binary() {
  if [ "${INSTALL_DEST_EXISTED:-false}" = true ] && [ -n "${INSTALL_BACKUP:-}" ]; then
    run_install_fs mv -f "$INSTALL_BACKUP" "$INSTALL_DEST" || return 1
    INSTALL_BACKUP=""
  else
    run_install_fs rm -f "$INSTALL_DEST" || return 1
  fi
  return 0
}

stage_and_replace_binary() {
  local candidate="$1"
  INSTALL_DEST="${INSTALL_DIR}/${BINARY_NAME}"
  INSTALL_STAGE=""
  INSTALL_BACKUP=""
  INSTALL_DEST_EXISTED=false

  if [ -L "$INSTALL_DEST" ]; then
    CANDIDATE_ERROR="refusing to replace symbolic-link install target ${INSTALL_DEST}"
    return 1
  fi
  INSTALL_STAGE="$(run_install_fs mktemp "${INSTALL_DIR}/.${BINARY_NAME}.install.XXXXXX")" || return 1
  run_install_fs install -m 0755 "$candidate" "$INSTALL_STAGE" || return 1

  if [ -e "$INSTALL_DEST" ]; then
    INSTALL_DEST_EXISTED=true
    INSTALL_BACKUP="$(run_install_fs mktemp "${INSTALL_DIR}/.${BINARY_NAME}.backup.XXXXXX")" || return 1
    run_install_fs cp -p "$INSTALL_DEST" "$INSTALL_BACKUP" || return 1
  fi

  run_install_fs mv -f "$INSTALL_STAGE" "$INSTALL_DEST" || return 1
  INSTALL_STAGE=""
  return 0
}

commit_installed_binary() {
  if [ -n "${INSTALL_BACKUP:-}" ]; then
    run_install_fs rm -f "$INSTALL_BACKUP"
    INSTALL_BACKUP=""
  fi
}

resolve_path_target() (
  local profile_target="$1"
  local link_target link_hops=0 physical_dir
  while [ -L "$profile_target" ]; do
    link_hops=$((link_hops + 1))
    [ "$link_hops" -le 40 ] || error "Too many symbolic links while resolving $1."
    link_target="$(readlink "$profile_target")" || error "Failed to resolve symbolic link $1."
    case "$link_target" in
      /*) ;;
      *) link_target="$(dirname "$profile_target")/${link_target}" ;;
    esac
    profile_target="$link_target"
  done
  physical_dir="$(CDPATH= cd -P "$(dirname "$profile_target")" && pwd -P)" || error "Failed to resolve profile directory for $1."
  printf '%s/%s\n' "$physical_dir" "$(basename "$profile_target")"
)

file_identity() (
  local path="$1" identity
  if identity="$(stat -f '%d:%i' "$path" 2>/dev/null)"; then
    printf '%s\n' "$identity"
  elif identity="$(stat -c '%d:%i' "$path" 2>/dev/null)"; then
    printf '%s\n' "$identity"
  else
    error "Failed to identify ${path}."
  fi
)

remove_path_block() (
  local profile_file="$1"
  local profile_target current_profile_target profile_identity current_profile_identity
  local profile_dir tmp_file=""
  [ -f "$profile_file" ] || return 0
  grep -F "$PATH_BLOCK_BEGIN" "$profile_file" >/dev/null 2>&1 || return 0
  profile_target="$(resolve_path_target "$profile_file")"
  profile_identity="$(file_identity "$profile_target")"
  profile_dir="$(dirname "$profile_target")"
  tmp_file="$(mktemp "${profile_dir}/.${BINARY_NAME}.XXXXXX")" || error "Failed to create temporary profile file for ${profile_file}."
  trap '[ -z "$tmp_file" ] || rm -f "$tmp_file"' EXIT
  if ! cp -p "$profile_target" "$tmp_file"; then
    error "Failed to prepare temporary profile file for ${profile_file}."
  fi
  if ! awk -v begin="$PATH_BLOCK_BEGIN" -v end="$PATH_BLOCK_END" '
    $0 == begin {
      if (inside || seen_begin) invalid = 1
      inside = 1
      seen_begin = 1
      next
    }
    $0 == end {
      if (!inside || seen_end) invalid = 1
      inside = 0
      seen_end = 1
      next
    }
    !inside { print }
    END {
      if (invalid || !seen_begin || !seen_end || inside) exit 1
    }
  ' "$profile_target" > "$tmp_file"; then
    error "Failed to remove codex-switch-global-pace PATH block from ${profile_file}."
  fi
  current_profile_target="$(resolve_path_target "$profile_file")"
  if [ "$current_profile_target" != "$profile_target" ]; then
    error "Profile link changed while updating ${profile_file}; original file was left unchanged."
  fi
  current_profile_identity="$(file_identity "$current_profile_target")"
  if [ "$current_profile_identity" != "$profile_identity" ]; then
    error "Profile file changed while updating ${profile_file}; newer contents were left unchanged."
  fi
  if ! mv -f "$tmp_file" "$profile_target"; then
    error "Failed to replace ${profile_file} with the updated PATH configuration."
  fi
  tmp_file=""
  info "Removed codex-switch-global-pace PATH entry from ${profile_file}."
)

remove_managed_path_blocks() {
  remove_path_block "${HOME}/.zprofile"
  remove_path_block "${HOME}/.bash_profile"
  remove_path_block "${HOME}/.profile"
  remove_path_block "${HOME}/.config/fish/config.fish"
}

# Parse arguments
USE_DEV=false
UNINSTALL=false
SYSTEM_INSTALL=false
for arg in "$@"; do
  case "$arg" in
    --dev)       USE_DEV=true ;;
    --uninstall) UNINSTALL=true ;;
    --system)    SYSTEM_INSTALL=true ;;
    *)           error "Unknown argument: $arg" ;;
  esac
done

if [ "$SYSTEM_INSTALL" = true ]; then
  INSTALL_DIR="$SYSTEM_INSTALL_DIR"
else
  INSTALL_DIR="$USER_INSTALL_DIR"
fi

# ── Uninstall ────────────────────────────────────────────
if [ "$UNINSTALL" = true ]; then
  info "Uninstalling codex-switch-global-pace..."

  LEGACY_KIND="missing"
  LEGACY_RESOLVED="$LEGACY_BIN"
  if [ -e "$LEGACY_BIN" ]; then
    classify_legacy_binary
  fi
  if [ "$LEGACY_KIND" = "homebrew" ] && { [ "$SYSTEM_INSTALL" = true ] || [ ! -x "${INSTALL_DIR}/${BINARY_NAME}" ]; }; then
    error "Homebrew-managed install detected at ${LEGACY_RESOLVED}. Run 'brew uninstall codex-switch-global-pace'; the direct uninstaller did not change Homebrew files."
  fi

  SERVICE_UNINSTALL_FAILED=false
  if [ -x "${INSTALL_DIR}/${BINARY_NAME}" ]; then
    DAEMON_BIN="${INSTALL_DIR}/${BINARY_NAME}"
  else
    DAEMON_BIN="$(command -v codex-switch-global-pace 2>/dev/null || true)"
  fi
  if [ -z "$DAEMON_BIN" ] && [ "$SYSTEM_INSTALL" = false ] && [ "$LEGACY_KIND" = "direct" ] && [ -x "$LEGACY_BIN" ]; then
    DAEMON_BIN="$LEGACY_BIN"
  fi
  if [ -n "$DAEMON_BIN" ]; then
    if "$DAEMON_BIN" daemon uninstall; then
      info "Removed daemon service."
    else
      warn "Failed to remove daemon service with '${DAEMON_BIN} daemon uninstall'."
      SERVICE_UNINSTALL_FAILED=true
    fi
  else
    case "$(uname -s)" in
      Darwin)
        PLIST_PATH="${HOME}/Library/LaunchAgents/com.codex-switch-global-pace.daemon.plist"
        if [ -f "$PLIST_PATH" ]; then
          if ! launchctl unload "$PLIST_PATH"; then
            warn "Failed to unload LaunchAgent ${PLIST_PATH}."
            SERVICE_UNINSTALL_FAILED=true
          else
            rm -f "$PLIST_PATH"
            info "Removed LaunchAgent ${PLIST_PATH}."
          fi
        fi
        ;;
      Linux)
        UNIT_PATH="${HOME}/.config/systemd/user/codex-switch-global-pace-daemon.service"
        if [ -f "$UNIT_PATH" ]; then
          if ! systemctl --user disable --now codex-switch-global-pace-daemon; then
            warn "Failed to disable systemd user service codex-switch-global-pace-daemon."
            SERVICE_UNINSTALL_FAILED=true
          else
            rm -f "$UNIT_PATH"
            systemctl --user daemon-reload || warn "Failed to reload systemd user units."
            info "Removed systemd user service ${UNIT_PATH}."
          fi
        fi
        ;;
    esac
  fi

  if [ "$SERVICE_UNINSTALL_FAILED" = true ]; then
    error "Daemon service cleanup failed; binary and data were kept. Resolve the service error and retry uninstall."
  fi

  BIN_PATH="${INSTALL_DIR}/${BINARY_NAME}"
  if [ "$SYSTEM_INSTALL" = false ] && [ ! -f "$BIN_PATH" ] && [ "$LEGACY_KIND" = "direct" ] && [ -f "$LEGACY_BIN" ]; then
    BIN_PATH="$LEGACY_BIN"
  fi
  if [ -f "$BIN_PATH" ]; then
    BIN_DIR="${BIN_PATH%/*}"
    if [ "$BIN_PATH" = "$LEGACY_BIN" ] && [ -w "$BIN_DIR" ]; then
      rm -f "$BIN_PATH" "$SYSTEM_INSTALL_MARKER"
    elif [ "$BIN_PATH" = "$LEGACY_BIN" ]; then
      info "Removing ${BIN_PATH} (requires sudo)"
      sudo rm -f "$BIN_PATH" "$SYSTEM_INSTALL_MARKER"
    elif [ -w "$BIN_DIR" ]; then
      rm -f "$BIN_PATH"
    else
      info "Removing ${BIN_PATH} (requires sudo)"
      sudo rm -f "$BIN_PATH"
    fi
    info "Removed ${BIN_PATH}"
  fi

  if [ "$SYSTEM_INSTALL" = false ]; then
    remove_managed_path_blocks
  fi

  # This directory is deliberately shared with codex-switch. Removing it here
  # would delete profiles and credentials still used by the other program.
  if [ -d "$DATA_DIR" ]; then
    info "Kept shared profile data: ${DATA_DIR}"
  fi

  info "codex-switch-global-pace has been uninstalled."
  exit 0
fi

# ── Install ──────────────────────────────────────────────

if [ "$USE_DEV" = true ]; then
  VERSION="dev"
else
  VERSION="${CS_VERSION:-latest}"
  if [ "$VERSION" != "latest" ]; then
    validate_version "$VERSION"
  fi
fi

if [ "$VERSION" = "latest" ] || [ "$VERSION" = "dev" ]; then
  [ -n "$PACKAGED_RELEASE_VERSION" ] || error "This installer is not bound to a GitHub Release. Download install.sh from the stable or dev Release assets instead of running the repository copy directly."
  EXPECTED_RELEASE_VERSION="$PACKAGED_RELEASE_VERSION"
else
  EXPECTED_RELEASE_VERSION="$VERSION"
fi
validate_version "$EXPECTED_RELEASE_VERSION"
if [ "$USE_DEV" = true ]; then
  case "$EXPECTED_RELEASE_VERSION" in
    *-dev|*-dev.*) ;;
    *) error "Development installer expected a -dev release, got '${EXPECTED_RELEASE_VERSION}'." ;;
  esac
fi

# Detect OS and architecture
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$OS" in
  linux)  PLATFORM="linux" ;;
  darwin) PLATFORM="darwin" ;;
  *)      error "Unsupported OS: $OS" ;;
esac

case "$ARCH" in
  x86_64|amd64)   ARCH_NAME="amd64" ;;
  aarch64|arm64)   ARCH_NAME="arm64" ;;
  *)               error "Unsupported architecture: $ARCH" ;;
esac

# A pre-user-install direct binary in /usr/local/bin would otherwise shadow the
# new user-owned binary. Classify its ownership before downloading, then remove
# it only after the new binary and any running daemon service are committed.
MIGRATE_LEGACY=false
MIGRATE_LEGACY_SERVICE=false
LEGACY_NEEDS_SUDO=false
if [ -e "$LEGACY_BIN" ] || [ -L "$LEGACY_BIN" ]; then
  classify_legacy_binary
  if [ "$LEGACY_KIND" = "homebrew" ]; then
    error "Homebrew-managed install detected at ${LEGACY_RESOLVED}. Run 'brew uninstall codex-switch-global-pace' before using the direct installer; no Homebrew files were changed."
  fi
  if [ "$SYSTEM_INSTALL" = false ]; then
    if legacy_service_references_binary; then
      if ! legacy_service_is_running; then
        error "The installed daemon service at ${LEGACY_SERVICE_PATH} still references ${LEGACY_BIN}, but it is not running. To preserve that state safely, run '${LEGACY_BIN} daemon uninstall', rerun this installer, then reinstall the daemon only when you want it running. No binary or service was changed."
      fi
      MIGRATE_LEGACY_SERVICE=true
      info "Running legacy daemon service detected; it will be moved to the user-owned binary transactionally."
    fi
    if [ ! -w "$SYSTEM_INSTALL_DIR" ]; then
      info "Legacy system install detected at ${LEGACY_RESOLVED}; migration requires sudo once."
      LEGACY_NEEDS_SUDO=true
    else
      info "Legacy system install detected at ${LEGACY_RESOLVED}; it will be migrated."
    fi
    MIGRATE_LEGACY=true
  fi
fi

ASSET_NAME="codex-switch-global-pace-${PLATFORM}-${ARCH_NAME}.tar.gz"

# Get release URL
if [ "$USE_DEV" = true ]; then
  DOWNLOAD_URL="https://github.com/${REPO}/releases/download/dev/${ASSET_NAME}"
else
  if [ "$VERSION" = "latest" ]; then
    DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/${ASSET_NAME}"
  else
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/v${VERSION}/${ASSET_NAME}"
  fi
fi

info "Detected: ${PLATFORM}/${ARCH_NAME}"
info "Downloading: ${DOWNLOAD_URL}"

# Download, verify, and extract
TMP_DIR="$(mktemp -d)"
INSTALL_STAGE=""
INSTALL_BACKUP=""
INSTALL_WITH_SUDO=false
trap 'cleanup_install_artifacts; rm -rf "$TMP_DIR"' EXIT

curl -fsSL "$DOWNLOAD_URL" -o "${TMP_DIR}/${ASSET_NAME}" || error "Download failed. Check the URL or your network."
CHECKSUM_URL="${DOWNLOAD_URL}.sha256"
CHECKSUM_FILE="${TMP_DIR}/${ASSET_NAME}.sha256"
curl -fsSL "$CHECKSUM_URL" -o "$CHECKSUM_FILE" || error "Checksum download failed. The release is incomplete or your network is unavailable."

EXPECTED_SHA256="$(awk -v filename="$ASSET_NAME" '
  NF != 2 { exit 1 }
  length($1) != 64 || $1 !~ /^[[:xdigit:]]+$/ { exit 1 }
  $2 != filename && $2 != "*" filename { exit 1 }
  NR > 1 { exit 1 }
  { print tolower($1) }
  END { if (NR != 1) exit 1 }
' "$CHECKSUM_FILE")" || error "Invalid checksum file for ${ASSET_NAME}."
[ -n "$EXPECTED_SHA256" ] || error "Checksum file for ${ASSET_NAME} is empty."

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL_SHA256="$(sha256sum "${TMP_DIR}/${ASSET_NAME}" | awk '{print tolower($1)}')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL_SHA256="$(shasum -a 256 "${TMP_DIR}/${ASSET_NAME}" | awk '{print tolower($1)}')"
else
  error "Neither sha256sum nor shasum is available to verify the download."
fi

[ "$ACTUAL_SHA256" = "$EXPECTED_SHA256" ] || error "Checksum mismatch for ${ASSET_NAME}; refusing to extract it."
info "Checksum verified: ${ASSET_NAME}"
tar xzf "${TMP_DIR}/${ASSET_NAME}" -C "$TMP_DIR"

CANDIDATE_ERROR=""
if ! verify_candidate_version "${TMP_DIR}/${BINARY_NAME}" "$EXPECTED_RELEASE_VERSION"; then
  error "Downloaded binary failed its pre-install check; the existing installation was not changed: ${CANDIDATE_ERROR}"
fi

if [ "$MIGRATE_LEGACY" = true ] && [ "$LEGACY_NEEDS_SUDO" = true ]; then
  sudo -v || error "Cannot migrate ${LEGACY_BIN} without sudo. Re-run with access to remove the legacy binary, or use --system."
fi

# Install
if [ "$SYSTEM_INSTALL" = true ]; then
  if [ ! -w "$INSTALL_DIR" ]; then
    info "Installing system-wide to ${INSTALL_DIR} (requires sudo)"
    sudo -v || error "Cannot install to ${INSTALL_DIR} without sudo."
    INSTALL_WITH_SUDO=true
  fi
else
  mkdir -p "$INSTALL_DIR"
fi

MARKER_WAS_PRESENT=false
[ -e "$SYSTEM_INSTALL_MARKER" ] && MARKER_WAS_PRESENT=true
CANDIDATE_ERROR=""
if ! stage_and_replace_binary "${TMP_DIR}/${BINARY_NAME}"; then
  cleanup_install_artifacts
  error "Failed to stage an atomic binary replacement; the existing installation was not changed. ${CANDIDATE_ERROR}"
fi

if [ "$SYSTEM_INSTALL" = true ] && ! run_install_fs install -m 0644 /dev/null "$SYSTEM_INSTALL_MARKER"; then
  rollback_installed_binary || error "System install marker creation failed and the prior executable could not be restored from ${INSTALL_BACKUP}."
  [ "$MARKER_WAS_PRESENT" = true ] || run_install_fs rm -f "$SYSTEM_INSTALL_MARKER" || true
  error "System install marker creation failed; the prior executable was restored."
fi

if ! verify_candidate_version "${INSTALL_DIR}/${BINARY_NAME}" "$EXPECTED_RELEASE_VERSION"; then
  rollback_installed_binary || error "Installed binary verification failed and the prior executable could not be restored from ${INSTALL_BACKUP}: ${CANDIDATE_ERROR}"
  if [ "$SYSTEM_INSTALL" = true ] && [ "$MARKER_WAS_PRESENT" = false ]; then
    run_install_fs rm -f "$SYSTEM_INSTALL_MARKER" || true
  fi
  error "Installed binary verification failed; the prior executable was restored: ${CANDIDATE_ERROR}"
fi

if [ "$MIGRATE_LEGACY_SERVICE" = true ]; then
  info "Reinstalling the running daemon service with ${INSTALL_DEST}..."
  if ! "$INSTALL_DEST" daemon install; then
    warn "The new service installation failed; restoring the legacy service definition."
    if "$LEGACY_BIN" daemon install; then
      rollback_installed_binary || error "Legacy daemon service was restored, but the prior user binary could not be restored from ${INSTALL_BACKUP}."
      error "Daemon service migration failed; the legacy service and prior user binary were restored, and ${LEGACY_BIN} was kept."
    fi
    commit_installed_binary
    error "Daemon service migration and legacy-service restoration both failed. Both verified binaries were kept at ${INSTALL_DEST} and ${LEGACY_BIN}; resolve the service error before removing either path."
  fi
fi
commit_installed_binary

if [ "$MIGRATE_LEGACY" = true ]; then
  if [ "$LEGACY_NEEDS_SUDO" = true ]; then
    sudo rm -f "$LEGACY_BIN" "$SYSTEM_INSTALL_MARKER"
  else
    rm -f "$LEGACY_BIN" "$SYSTEM_INSTALL_MARKER"
  fi
  info "Removed legacy install: ${LEGACY_BIN}"
fi

if [ "$SYSTEM_INSTALL" = false ]; then
  case ":${PATH}:" in
    *":${USER_INSTALL_DIR}:"*) ;;
    *)
      case "${SHELL:-}" in
        */zsh)
          PROFILE_FILE="${HOME}/.zprofile"
          PATH_LINE='export PATH="$HOME/.local/bin:$PATH"'
          ;;
        */bash)
          if [ "$PLATFORM" = "darwin" ]; then
            PROFILE_FILE="${HOME}/.bash_profile"
          else
            PROFILE_FILE="${HOME}/.profile"
          fi
          PATH_LINE='export PATH="$HOME/.local/bin:$PATH"'
          ;;
        */fish)
          PROFILE_FILE="${HOME}/.config/fish/config.fish"
          PATH_LINE='fish_add_path "$HOME/.local/bin"'
          mkdir -p "${HOME}/.config/fish"
          ;;
        *)
          PROFILE_FILE=""
          PATH_LINE=""
          ;;
      esac
      if [ -n "$PROFILE_FILE" ]; then
        if ! grep -F "$PATH_BLOCK_BEGIN" "$PROFILE_FILE" >/dev/null 2>&1; then
          printf '\n%s\n%s\n%s\n' "$PATH_BLOCK_BEGIN" "$PATH_LINE" "$PATH_BLOCK_END" >> "$PROFILE_FILE"
          info "Added ${USER_INSTALL_DIR} to PATH in ${PROFILE_FILE}; restart your shell to apply it."
        fi
      else
        warn "Add ${USER_INSTALL_DIR} to your PATH to run codex-switch-global-pace by name."
      fi
      ;;
  esac
fi

info "Installed: $(${INSTALL_DIR}/${BINARY_NAME} --version)"
info "Run 'codex-switch-global-pace --help' to get started"
