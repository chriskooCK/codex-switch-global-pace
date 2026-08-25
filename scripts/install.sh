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
DAEMON_BOUNDARY_PROTOCOL_PREFIX="${BINARY_NAME} daemon update boundary"
DATA_DIR="${HOME}/.codex-switch"
LEGACY_BIN="${SYSTEM_INSTALL_DIR}/${BINARY_NAME}"
SYSTEM_INSTALL_MARKER="${SYSTEM_INSTALL_DIR}/.codex-switch-global-pace-system-install-v1"
PATH_BLOCK_BEGIN="# >>> codex-switch-global-pace PATH >>>"
PATH_BLOCK_END="# <<< codex-switch-global-pace PATH <<<"
INSTALL_STAGE_NAME=".${BINARY_NAME}.install"
INSTALL_BACKUP_NAME=".${BINARY_NAME}.rollback"
INSTALL_DISPLACED_NAME=".${BINARY_NAME}.displaced"
INSTALL_FAILED_NAME=".${BINARY_NAME}.failed"
UNINSTALL_HOLD_NAME=".${BINARY_NAME}.uninstall"
LEGACY_HOLD_NAME=".${BINARY_NAME}.legacy"
LEGACY_DISPLACED_NAME=".${BINARY_NAME}.legacy-displaced"
SYMLINK_RESOLUTION_MAX_HOPS=40

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
    /usr/local/Cellar/codex-switch-global-pace/*) return 0 ;;
    /opt/homebrew/Cellar/codex-switch-global-pace/*) return 0 ;;
    /home/linuxbrew/.linuxbrew/Cellar/codex-switch-global-pace/*) return 0 ;;
    *) return 1 ;;
  esac
}

classify_binary_ownership() {
  local candidate="$1"
  BINARY_KIND="missing"
  BINARY_RESOLVED="$candidate"
  if [ ! -e "$candidate" ] && [ ! -L "$candidate" ]; then
    return 0
  fi
  BINARY_RESOLVED="$(resolve_path_target "$candidate")"
  if is_homebrew_cellar_path "$BINARY_RESOLVED"; then
    BINARY_KIND="homebrew"
  else
    BINARY_KIND="direct"
  fi
}

validate_locked_direct_binary() {
  local candidate="$1" description="$2" allow_missing="$3"
  classify_binary_ownership "$candidate"
  if [ "$BINARY_KIND" = "missing" ]; then
    [ "$allow_missing" = true ] && return 0
    error "The ${description} disappeared before its shared update lock was acquired; nothing else was changed."
  fi
  if [ "$BINARY_KIND" = "homebrew" ]; then
    error "The ${description} changed to a Homebrew-managed binary at ${BINARY_RESOLVED}; the direct transaction changed nothing. Use 'brew uninstall codex-switch-global-pace' for that binary."
  fi
  [ ! -L "$candidate" ] && [ -f "$candidate" ] \
    || error "The ${description} is not a regular direct-install file after locking; nothing was changed."
  [ -x "$candidate" ] \
    || error "The ${description} is not executable after locking; no daemon, service, binary, or PATH configuration was changed."
}

find_homebrew_managed_binary() {
  local candidate path_binary
  HOMEBREW_BIN=""
  HOMEBREW_RESOLVED=""
  path_binary="$(command -v "$BINARY_NAME" 2>/dev/null || true)"
  for candidate in \
    "$path_binary" \
    "/usr/local/bin/${BINARY_NAME}" \
    "/opt/homebrew/bin/${BINARY_NAME}" \
    "/home/linuxbrew/.linuxbrew/bin/${BINARY_NAME}"
  do
    [ -n "$candidate" ] || continue
    classify_binary_ownership "$candidate"
    if [ "$BINARY_KIND" = "homebrew" ]; then
      HOMEBREW_BIN="$candidate"
      HOMEBREW_RESOLVED="$BINARY_RESOLVED"
      return 0
    fi
  done
  return 1
}

install_transaction_residue_exists() {
  local directory="$1" residue
  [ -d "$directory" ] || return 1
  for residue in \
    "${directory}/${INSTALL_STAGE_NAME}" \
    "${directory}/${INSTALL_BACKUP_NAME}" \
    "${directory}/${INSTALL_DISPLACED_NAME}" \
    "${directory}/${INSTALL_FAILED_NAME}" \
    "${directory}/${UNINSTALL_HOLD_NAME}" \
    "${directory}/.${BINARY_NAME}.installer-quarantine-"* \
    "${directory}/.${BINARY_NAME}.install."* \
    "${directory}/.${BINARY_NAME}.backup."* \
    "${directory}/.${BINARY_NAME}.rollback."* \
    "${directory}/.${BINARY_NAME}.failed."*
  do
    if [ -e "$residue" ] || [ -L "$residue" ]; then
      TRANSACTION_RESIDUE="$residue"
      return 0
    fi
  done
  return 1
}

legacy_transaction_residue_exists() {
  local residue
  for residue in \
    "${SYSTEM_INSTALL_DIR}/${LEGACY_HOLD_NAME}" \
    "${SYSTEM_INSTALL_DIR}/${LEGACY_DISPLACED_NAME}" \
    "${SYSTEM_INSTALL_DIR}/.${BINARY_NAME}.installer-quarantine-"* \
    "${SYSTEM_INSTALL_DIR}/.${BINARY_NAME}.legacy."*
  do
    if [ -e "$residue" ] || [ -L "$residue" ]; then
      TRANSACTION_RESIDUE="$residue"
      return 0
    fi
  done
  return 1
}

assert_no_install_transaction_residue() {
  local directory="$1"
  TRANSACTION_RESIDUE=""
  if install_transaction_residue_exists "$directory" \
    || legacy_transaction_residue_exists
  then
    error "An incomplete installer transaction remains at ${TRANSACTION_RESIDUE}; no service, binary, marker, or PATH configuration was changed. Inspect that fixed recovery path before retrying."
  fi
}

read_checked_daemon_status() {
  local status
  DAEMON_STATUS_ERROR=""
  if ! status="$("$CANDIDATE_BIN" daemon status --installer-state 8>&- 9>&- 2>&1)"; then
    DAEMON_STATUS_ERROR="release-verified daemon state probe failed: ${status}"
    return 1
  fi
  case "$status" in
    *$'\n'*|*$'\r'*)
      DAEMON_STATUS_ERROR="daemon state probe returned more than one exact line"
      return 1
      ;;
  esac
  case "$status" in
    'running=true service_installed=true')
      DAEMON_STATUS_RUNNING=true
      DAEMON_STATUS_SERVICE_INSTALLED=true
      ;;
    'running=true service_installed=false')
      DAEMON_STATUS_RUNNING=true
      DAEMON_STATUS_SERVICE_INSTALLED=false
      ;;
    'running=false service_installed=true')
      DAEMON_STATUS_RUNNING=false
      DAEMON_STATUS_SERVICE_INSTALLED=true
      ;;
    'running=false service_installed=false')
      DAEMON_STATUS_RUNNING=false
      DAEMON_STATUS_SERVICE_INSTALLED=false
      ;;
    *)
      DAEMON_STATUS_ERROR="daemon state probe returned an unsupported line: ${status}"
      return 1
      ;;
  esac
}

verify_candidate_version() {
  local candidate="$1" expected="$2" output first_line
  if ! output="$("$candidate" --version 8>&- 9>&- 2>&1)"; then
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

run_installer_file_op() {
  local use_sudo="$1" output
  shift
  INSTALLER_FILE_OP_RESULT=""
  INSTALLER_FILE_OP_ERROR=""
  if [ "$use_sudo" = true ]; then
    if ! output="$(sudo "$CANDIDATE_BIN" __installer-file-op "$@" 8>&- 9>&- 2>&1)"; then
      INSTALLER_FILE_OP_ERROR="$output"
      return 1
    fi
  else
    if ! output="$("$CANDIDATE_BIN" __installer-file-op "$@" 8>&- 9>&- 2>&1)"; then
      INSTALLER_FILE_OP_ERROR="$output"
      return 1
    fi
  fi
  INSTALLER_FILE_OP_RESULT="$output"
}

installer_file_token() {
  local path="$1" use_sudo="$2" token
  run_installer_file_op "$use_sudo" token --source "$path" || return 1
  token="$INSTALLER_FILE_OP_RESULT"
  [[ "$token" =~ ^[0-9]+:[0-9]+\|[0-9a-f]{64}$ ]] || {
    INSTALLER_FILE_OP_ERROR="installer file helper returned an invalid token for ${path}: ${token}"
    return 1
  }
  INSTALLER_FILE_OP_RESULT="$token"
}

copy_installer_file_exclusive() {
  local source="$1" destination="$2" expected="$3" use_sudo="$4"
  local outcome token cleanup_error
  run_installer_file_op "$use_sudo" copy-exclusive \
    --source "$source" --destination "$destination" --expected-token "$expected" || return 1
  outcome="${INSTALLER_FILE_OP_RESULT%%|*}"
  token="${INSTALLER_FILE_OP_RESULT#*|}"
  [[ "$token" =~ ^[0-9]+:[0-9]+\|[0-9a-f]{64}$ ]] || {
    INSTALLER_FILE_OP_ERROR="installer file helper returned an invalid copy token for ${destination}: ${token}"
    return 1
  }
  INSTALLER_FILE_OP_RESULT="$token"
  case "$outcome" in
    created) return 0 ;;
    created-namespace-durability-unconfirmed)
      cleanup_error="copy creation reached ${destination}, but directory durability was not confirmed"
      if ! remove_installer_file_owned "$destination" "$token" "$use_sudo"; then
        cleanup_error="${cleanup_error}; exact cleanup failed: ${INSTALLER_FILE_OP_ERROR}"
      fi
      INSTALLER_FILE_OP_ERROR="$cleanup_error"
      return 1
      ;;
    *)
      INSTALLER_FILE_OP_ERROR="installer file helper returned an unknown copy outcome for ${destination}: ${outcome}"
      return 1
      ;;
  esac
}

create_empty_installer_file_exclusive() {
  local destination="$1" use_sudo="$2" outcome token cleanup_error
  run_installer_file_op "$use_sudo" create-empty-exclusive \
    --destination "$destination" || return 1
  outcome="${INSTALLER_FILE_OP_RESULT%%|*}"
  token="${INSTALLER_FILE_OP_RESULT#*|}"
  [[ "$token" =~ ^[0-9]+:[0-9]+\|[0-9a-f]{64}$ ]] || {
    INSTALLER_FILE_OP_ERROR="installer file helper returned an invalid empty-file token for ${destination}: ${token}"
    return 1
  }
  INSTALLER_FILE_OP_RESULT="$token"
  case "$outcome" in
    created) return 0 ;;
    created-namespace-durability-unconfirmed)
      cleanup_error="empty-file creation reached ${destination}, but directory durability was not confirmed"
      if ! remove_installer_file_owned "$destination" "$token" "$use_sudo"; then
        cleanup_error="${cleanup_error}; exact cleanup failed: ${INSTALLER_FILE_OP_ERROR}"
      fi
      INSTALLER_FILE_OP_ERROR="$cleanup_error"
      return 1
      ;;
    *)
      INSTALLER_FILE_OP_ERROR="installer file helper returned an unknown empty-file outcome for ${destination}: ${outcome}"
      return 1
      ;;
  esac
}

capture_installer_file_token() {
  local path="$1" use_sudo="$2" output_name="$3"
  installer_file_token "$path" "$use_sudo" || return 1
  printf -v "$output_name" '%s' "$INSTALLER_FILE_OP_RESULT"
}

capture_installer_file_copy() {
  local source="$1" destination="$2" expected="$3" use_sudo="$4" output_name="$5"
  copy_installer_file_exclusive "$source" "$destination" "$expected" "$use_sudo" || return 1
  printf -v "$output_name" '%s' "$INSTALLER_FILE_OP_RESULT"
}

capture_empty_installer_file() {
  local destination="$1" use_sudo="$2" output_name="$3"
  create_empty_installer_file_exclusive "$destination" "$use_sudo" || return 1
  printf -v "$output_name" '%s' "$INSTALLER_FILE_OP_RESULT"
}

installer_file_token_matches() {
  local path="$1" use_sudo="$2" expected="$3"
  installer_file_token "$path" "$use_sudo" \
    && [ "$INSTALLER_FILE_OP_RESULT" = "$expected" ]
}

move_installer_file_noreplace() {
  local source="$1" destination="$2" expected="$3" use_sudo="$4" token
  run_installer_file_op "$use_sudo" move-noreplace \
    --source "$source" --destination "$destination" --expected-token "$expected" || return 1
  token="$INSTALLER_FILE_OP_RESULT"
  [ "$token" = "$expected" ] || {
    INSTALLER_FILE_OP_ERROR="installer file helper returned an unexpected move token for ${destination}"
    return 1
  }
}

exchange_installer_files() {
  local source="$1" destination="$2" expected_source="$3" expected_destination="$4"
  local use_sudo="$5" result
  run_installer_file_op "$use_sudo" exchange \
    --source "$source" --destination "$destination" \
    --expected-token "$expected_source" \
    --expected-destination-token "$expected_destination" || return 1
  result="$INSTALLER_FILE_OP_RESULT"
  [ "$result" = "$expected_source" ] || {
    INSTALLER_FILE_OP_ERROR="installer file helper returned an unexpected exchange result"
    return 1
  }
}

remove_installer_file_owned() {
  local source="$1" expected="$2" use_sudo="$3" result
  run_installer_file_op "$use_sudo" remove-owned \
    --source "$source" --expected-token "$expected" || return 1
  result="$INSTALLER_FILE_OP_RESULT"
  case "$result" in
    removed) return 0 ;;
    removed-namespace-durability-unconfirmed)
      INSTALLER_FILE_OP_ERROR="the exact owned file at ${source} was removed, but parent-directory namespace durability was not confirmed"
      return 1
      ;;
    *)
    INSTALLER_FILE_OP_ERROR="installer file helper returned an unexpected removal result for ${source}: ${result}"
    return 1
      ;;
  esac
}

file_token_digest() {
  local token="$1"
  case "$token" in
    *'|'*) printf '%s\n' "${token#*|}" ;;
    *) return 1 ;;
  esac
}

cleanup_install_artifacts() {
  INSTALL_ARTIFACT_CLEANUP_ERROR=""
  if [ "${INSTALL_STAGE_OWNED:-false}" = true ] \
    && [ -n "${INSTALL_STAGE:-}" ] \
    && { [ -e "$INSTALL_STAGE" ] || [ -L "$INSTALL_STAGE" ]; }
  then
    if [ -z "${INSTALL_STAGE_TOKEN:-}" ]; then
      INSTALL_ARTIFACT_CLEANUP_ERROR="staged installer path ${INSTALL_STAGE} has no recorded token and was preserved"
      return 1
    fi
    if ! remove_installer_file_owned \
      "$INSTALL_STAGE" "$INSTALL_STAGE_TOKEN" "${INSTALL_WITH_SUDO:-false}" >/dev/null
    then
      INSTALL_ARTIFACT_CLEANUP_ERROR="staged installer candidate could not be removed through its token-bound quarantine: ${INSTALLER_FILE_OP_ERROR}; fixed residue at ${INSTALL_STAGE} requires inspection"
      return 1
    fi
  fi
  INSTALL_STAGE_OWNED=false
  INSTALL_STAGE_TOKEN=""
  return 0
}

rollback_installed_binary() {
  local restored_token
  if [ "${INSTALL_DEST_EXISTED:-false}" = true ]; then
    installer_file_token_matches \
      "$INSTALL_STAGE" "$INSTALL_WITH_SUDO" "$INSTALL_ORIGINAL_TOKEN" || return 1
    installer_file_token_matches \
      "$INSTALL_DEST" "$INSTALL_WITH_SUDO" "$INSTALL_PUBLISHED_TOKEN" || return 1
    exchange_installer_files \
      "$INSTALL_STAGE" "$INSTALL_DEST" "$INSTALL_ORIGINAL_TOKEN" \
      "$INSTALL_PUBLISHED_TOKEN" "$INSTALL_WITH_SUDO" \
      || return 1
    capture_installer_file_token \
      "$INSTALL_DEST" "$INSTALL_WITH_SUDO" restored_token || return 1
    [ "$restored_token" = "$INSTALL_ORIGINAL_TOKEN" ] || return 1
    INSTALL_STAGE_TOKEN="$INSTALL_PUBLISHED_TOKEN"
    remove_installer_file_owned \
      "$INSTALL_STAGE" "$INSTALL_STAGE_TOKEN" "$INSTALL_WITH_SUDO" || return 1
    INSTALL_STAGE_OWNED=false
    INSTALL_STAGE_TOKEN=""
    remove_installer_file_owned \
      "$INSTALL_BACKUP" "$INSTALL_BACKUP_TOKEN" "$INSTALL_WITH_SUDO" || return 1
    INSTALL_BACKUP_TOKEN=""
  else
    installer_file_token_matches \
      "$INSTALL_DEST" "$INSTALL_WITH_SUDO" "$INSTALL_PUBLISHED_TOKEN" || return 1
    remove_installer_file_owned \
      "$INSTALL_DEST" "$INSTALL_PUBLISHED_TOKEN" "$INSTALL_WITH_SUDO" || return 1
    if [ -e "$INSTALL_BACKUP" ] || [ -L "$INSTALL_BACKUP" ]; then
      return 1
    fi
  fi
  INSTALL_PUBLISHED_TOKEN=""
  return 0
}

stage_and_replace_binary() {
  local candidate="$1" candidate_token stage_after destination_after published_token boundary_error
  INSTALL_DEST="${INSTALL_DIR}/${BINARY_NAME}"
  INSTALL_DEST_EXISTED=false
  INSTALL_ORIGINAL_TOKEN=""
  INSTALL_BACKUP_TOKEN=""
  INSTALL_PUBLISHED_TOKEN=""

  if [ -e "$INSTALL_STAGE" ] || [ -L "$INSTALL_STAGE" ] \
    || [ -e "$INSTALL_BACKUP" ] || [ -L "$INSTALL_BACKUP" ]
  then
    CANDIDATE_ERROR="a fixed installer transaction path appeared before staging"
    return 1
  fi
  if [ -L "$INSTALL_DEST" ]; then
    CANDIDATE_ERROR="refusing to replace symbolic-link install target ${INSTALL_DEST}"
    return 1
  fi
  capture_installer_file_token "$candidate" false candidate_token || {
    CANDIDATE_ERROR="could not bind the verified candidate to the file helper: ${INSTALLER_FILE_OP_ERROR}"
    return 1
  }
  capture_installer_file_copy \
    "$candidate" "$INSTALL_STAGE" "$candidate_token" "$INSTALL_WITH_SUDO" \
    INSTALL_STAGE_TOKEN || {
    CANDIDATE_ERROR="exclusive candidate staging failed: ${INSTALLER_FILE_OP_ERROR}"
    return 1
  }
  INSTALL_STAGE_OWNED=true

  INSTALL_PUBLISHED_TOKEN="$INSTALL_STAGE_TOKEN"
  if [ -e "$INSTALL_DEST" ]; then
    INSTALL_DEST_EXISTED=true
    capture_installer_file_token \
      "$INSTALL_DEST" "$INSTALL_WITH_SUDO" INSTALL_ORIGINAL_TOKEN || return 1
    capture_installer_file_copy \
      "$INSTALL_DEST" "$INSTALL_BACKUP" "$INSTALL_ORIGINAL_TOKEN" \
      "$INSTALL_WITH_SUDO" INSTALL_BACKUP_TOKEN || return 1
    if ! exchange_installer_files \
      "$INSTALL_STAGE" "$INSTALL_DEST" "$INSTALL_STAGE_TOKEN" \
      "$INSTALL_ORIGINAL_TOKEN" "$INSTALL_WITH_SUDO"
    then
      boundary_error="$INSTALLER_FILE_OP_ERROR"
      stage_after=""
      destination_after=""
      capture_installer_file_token \
        "$INSTALL_STAGE" "$INSTALL_WITH_SUDO" stage_after 2>/dev/null || true
      capture_installer_file_token \
        "$INSTALL_DEST" "$INSTALL_WITH_SUDO" destination_after 2>/dev/null || true
      if [ "$stage_after" = "$INSTALL_ORIGINAL_TOKEN" ] \
        && [ "$destination_after" = "$INSTALL_PUBLISHED_TOKEN" ]
      then
        INSTALL_STAGE_TOKEN="$INSTALL_ORIGINAL_TOKEN"
        INSTALL_STAGE_OWNED=false
        BINARY_REPLACED=true
        CANDIDATE_ERROR="exchange reached the published state but its durability boundary failed: ${boundary_error}"
      else
        CANDIDATE_ERROR="atomic exchange failed and its operands were preserved for classification: ${boundary_error}"
      fi
      return 1
    fi
    INSTALL_STAGE_TOKEN="$INSTALL_ORIGINAL_TOKEN"
    INSTALL_STAGE_OWNED=false
    BINARY_REPLACED=true
  else
    if ! move_installer_file_noreplace \
      "$INSTALL_STAGE" "$INSTALL_DEST" "$INSTALL_STAGE_TOKEN" "$INSTALL_WITH_SUDO"
    then
      boundary_error="$INSTALLER_FILE_OP_ERROR"
      destination_after=""
      capture_installer_file_token \
        "$INSTALL_DEST" "$INSTALL_WITH_SUDO" destination_after 2>/dev/null || true
      if [ "$destination_after" = "$INSTALL_PUBLISHED_TOKEN" ] \
        && [ ! -e "$INSTALL_STAGE" ] && [ ! -L "$INSTALL_STAGE" ]
      then
        INSTALL_STAGE_OWNED=false
        INSTALL_STAGE_TOKEN=""
        BINARY_REPLACED=true
      fi
      CANDIDATE_ERROR="no-replace publication failed or its durability boundary was not confirmed: ${boundary_error}"
      return 1
    fi
    BINARY_REPLACED=true
    INSTALL_STAGE_OWNED=false
    INSTALL_STAGE_TOKEN=""
  fi
  capture_installer_file_token \
    "$INSTALL_DEST" "$INSTALL_WITH_SUDO" published_token || return 1
  [ "$published_token" = "$INSTALL_PUBLISHED_TOKEN" ] || return 1
  if [ "$INSTALL_DEST_EXISTED" = true ]; then
    installer_file_token_matches \
      "$INSTALL_BACKUP" "$INSTALL_WITH_SUDO" "$INSTALL_BACKUP_TOKEN" || return 1
    installer_file_token_matches \
      "$INSTALL_STAGE" "$INSTALL_WITH_SUDO" "$INSTALL_ORIGINAL_TOKEN" || return 1
  fi
  return 0
}

commit_installed_binary() {
  installer_file_token_matches \
    "$INSTALL_DEST" "$INSTALL_WITH_SUDO" "$INSTALL_PUBLISHED_TOKEN" || return 1
  if [ "$INSTALL_DEST_EXISTED" = true ]; then
    remove_installer_file_owned \
      "$INSTALL_STAGE" "$INSTALL_ORIGINAL_TOKEN" "$INSTALL_WITH_SUDO" || return 1
    INSTALL_STAGE_OWNED=false
    INSTALL_STAGE_TOKEN=""
    remove_installer_file_owned \
      "$INSTALL_BACKUP" "$INSTALL_BACKUP_TOKEN" "$INSTALL_WITH_SUDO" || return 1
    INSTALL_BACKUP_TOKEN=""
  elif [ -e "$INSTALL_BACKUP" ] || [ -L "$INSTALL_BACKUP" ]; then
    return 1
  fi
}

preserve_install_backup() {
  if [ -e "$INSTALL_BACKUP" ] || [ -L "$INSTALL_BACKUP" ]; then
    warn "Previous executable was preserved at ${INSTALL_BACKUP}; automatic rollback was not safe."
  fi
}

hold_legacy_install_for_commit() {
  local boundary_error legacy_after displaced_after
  [ "$MIGRATE_LEGACY" = true ] || return 0
  [ ! -e "$LEGACY_HOLD" ] && [ ! -L "$LEGACY_HOLD" ] \
    && [ ! -e "$LEGACY_DISPLACED" ] && [ ! -L "$LEGACY_DISPLACED" ] \
    || return 1
  capture_installer_file_token \
    "$LEGACY_BIN" "$LEGACY_NEEDS_SUDO" LEGACY_ORIGINAL_TOKEN || return 1
  capture_installer_file_copy \
    "$LEGACY_BIN" "$LEGACY_HOLD" "$LEGACY_ORIGINAL_TOKEN" \
    "$LEGACY_NEEDS_SUDO" LEGACY_HOLD_TOKEN || return 1
  capture_empty_installer_file \
    "$LEGACY_DISPLACED" "$LEGACY_NEEDS_SUDO" LEGACY_PLACEHOLDER_TOKEN || return 1
  LEGACY_DISPLACED_TOKEN="$LEGACY_PLACEHOLDER_TOKEN"
  LEGACY_HELD=true
  if ! exchange_installer_files \
    "$LEGACY_DISPLACED" "$LEGACY_BIN" "$LEGACY_PLACEHOLDER_TOKEN" \
    "$LEGACY_ORIGINAL_TOKEN" "$LEGACY_NEEDS_SUDO"
  then
    boundary_error="$INSTALLER_FILE_OP_ERROR"
    legacy_after=""
    displaced_after=""
    capture_installer_file_token \
      "$LEGACY_BIN" "$LEGACY_NEEDS_SUDO" legacy_after 2>/dev/null || true
    capture_installer_file_token \
      "$LEGACY_DISPLACED" "$LEGACY_NEEDS_SUDO" displaced_after 2>/dev/null || true
    if [ "$legacy_after" = "$LEGACY_PLACEHOLDER_TOKEN" ] \
      && [ "$displaced_after" = "$LEGACY_ORIGINAL_TOKEN" ]
    then
      LEGACY_DISPLACED_TOKEN="$LEGACY_ORIGINAL_TOKEN"
    fi
    INSTALLER_FILE_OP_ERROR="$boundary_error"
    return 1
  fi
  LEGACY_DISPLACED_TOKEN="$LEGACY_ORIGINAL_TOKEN"
}

restore_held_legacy_install() {
  local legacy_token displaced_token
  [ "${LEGACY_HELD:-false}" = true ] || return 0
  capture_installer_file_token \
    "$LEGACY_BIN" "$LEGACY_NEEDS_SUDO" legacy_token || return 1
  capture_installer_file_token \
    "$LEGACY_DISPLACED" "$LEGACY_NEEDS_SUDO" displaced_token || return 1
  if [ "$legacy_token" = "$LEGACY_PLACEHOLDER_TOKEN" ] \
    && [ "$displaced_token" = "$LEGACY_ORIGINAL_TOKEN" ]
  then
    exchange_installer_files \
      "$LEGACY_DISPLACED" "$LEGACY_BIN" "$LEGACY_ORIGINAL_TOKEN" \
      "$LEGACY_PLACEHOLDER_TOKEN" "$LEGACY_NEEDS_SUDO" || return 1
    LEGACY_DISPLACED_TOKEN="$LEGACY_PLACEHOLDER_TOKEN"
  elif [ "$legacy_token" != "$LEGACY_ORIGINAL_TOKEN" ] \
    || [ "$displaced_token" != "$LEGACY_PLACEHOLDER_TOKEN" ]
  then
    return 1
  fi
  remove_installer_file_owned \
    "$LEGACY_DISPLACED" "$LEGACY_PLACEHOLDER_TOKEN" "$LEGACY_NEEDS_SUDO" || return 1
  LEGACY_DISPLACED_TOKEN=""
  remove_installer_file_owned \
    "$LEGACY_HOLD" "$LEGACY_HOLD_TOKEN" "$LEGACY_NEEDS_SUDO" || return 1
  LEGACY_HELD=false
}

commit_held_legacy_install() {
  [ "${LEGACY_HELD:-false}" = true ] || return 0
  installer_file_token_matches \
    "$LEGACY_BIN" "$LEGACY_NEEDS_SUDO" "$LEGACY_PLACEHOLDER_TOKEN" || return 1
  installer_file_token_matches \
    "$LEGACY_DISPLACED" "$LEGACY_NEEDS_SUDO" "$LEGACY_ORIGINAL_TOKEN" || return 1
  if [ -e "$SYSTEM_INSTALL_MARKER" ]; then
    [ -n "${SYSTEM_MARKER_ORIGINAL_TOKEN:-}" ] \
      || return 1
    remove_installer_file_owned \
      "$SYSTEM_INSTALL_MARKER" "$SYSTEM_MARKER_ORIGINAL_TOKEN" "$LEGACY_NEEDS_SUDO" \
      || return 1
  fi
  remove_installer_file_owned \
    "$LEGACY_BIN" "$LEGACY_PLACEHOLDER_TOKEN" "$LEGACY_NEEDS_SUDO" || return 1
  remove_installer_file_owned \
    "$LEGACY_HOLD" "$LEGACY_HOLD_TOKEN" "$LEGACY_NEEDS_SUDO" || return 1
  remove_installer_file_owned \
    "$LEGACY_DISPLACED" "$LEGACY_DISPLACED_TOKEN" "$LEGACY_NEEDS_SUDO" || return 1
  LEGACY_DISPLACED_TOKEN=""
  LEGACY_HELD=false
}

close_daemon_update_boundary_channels() {
  exec 7>&- 6<&-
}

wait_daemon_update_boundary() {
  local expected_success="$1" wait_status=0
  close_daemon_update_boundary_channels
  if [ -n "${DAEMON_BOUNDARY_PID:-}" ]; then
    if wait "$DAEMON_BOUNDARY_PID"; then
      wait_status=0
    else
      wait_status=$?
    fi
  fi
  DAEMON_BOUNDARY_PID=""
  DAEMON_BOUNDARY_ACTIVE=false
  DAEMON_BOUNDARY_ROLLBACK_SAFE=false
  if [ "$expected_success" = true ] && [ "$wait_status" -ne 0 ]; then
    DAEMON_BOUNDARY_ERROR="daemon lifecycle holder exited with status ${wait_status}"
    return 1
  fi
  if [ "$expected_success" = false ] && [ "$wait_status" -eq 0 ]; then
    DAEMON_BOUNDARY_ERROR="abandoned daemon lifecycle holder accepted an incomplete transaction"
    return 1
  fi
  return 0
}

close_failed_daemon_update_boundary() {
  local primary_error="$1" holder_error
  if wait_daemon_update_boundary false; then
    DAEMON_BOUNDARY_ERROR="$primary_error"
  else
    holder_error="$DAEMON_BOUNDARY_ERROR"
    DAEMON_BOUNDARY_ERROR="${primary_error}; ${holder_error}"
  fi
}

start_daemon_update_boundary() {
  local candidate="$1" initial_executable="$2" replacement_executable="$3"
  local control="${TMP_DIR}/daemon-update-boundary.control"
  local status="${TMP_DIR}/daemon-update-boundary.status"
  local marker pid

  DAEMON_BOUNDARY_ERROR=""
  [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = false ] || {
    DAEMON_BOUNDARY_ERROR="a daemon lifecycle holder is already active"
    return 1
  }
  mkfifo "$control" "$status" || {
    DAEMON_BOUNDARY_ERROR="could not create private daemon lifecycle channels"
    return 1
  }
  (
    # Terminal Ctrl+C targets the installer's process group. Keep the authority
    # holder alive so the parent's EXIT cleanup can close stdin and let the
    # holder run its phase-aware EOF finalizer before the update locks close.
    trap '' INT QUIT
    exec "$candidate" __hold-daemon-update-boundary \
      --initial-executable "$initial_executable" \
      --replacement-executable "$replacement_executable" \
      6>&- 7>&- 8>&- 9>&- < "$control" > "$status"
  ) &
  pid=$!
  DAEMON_BOUNDARY_PID="$pid"
  exec 7>"$control"
  exec 6<"$status"

  if ! IFS= read -r marker <&6; then
    DAEMON_BOUNDARY_ERROR="candidate did not establish the daemon lifecycle boundary"
    close_failed_daemon_update_boundary "$DAEMON_BOUNDARY_ERROR"
    return 1
  fi
  case "$marker" in
    "${DAEMON_BOUNDARY_PROTOCOL_PREFIX} ready running=true service_installed=true")
      DAEMON_WAS_RUNNING=true
      DAEMON_SERVICE_INSTALLED=true
      ;;
    "${DAEMON_BOUNDARY_PROTOCOL_PREFIX} ready running=true service_installed=false")
      DAEMON_WAS_RUNNING=true
      DAEMON_SERVICE_INSTALLED=false
      ;;
    "${DAEMON_BOUNDARY_PROTOCOL_PREFIX} ready running=false service_installed=true")
      DAEMON_WAS_RUNNING=false
      DAEMON_SERVICE_INSTALLED=true
      ;;
    "${DAEMON_BOUNDARY_PROTOCOL_PREFIX} ready running=false service_installed=false")
      DAEMON_WAS_RUNNING=false
      DAEMON_SERVICE_INSTALLED=false
      ;;
    *)
      DAEMON_BOUNDARY_ERROR="candidate returned an invalid daemon lifecycle readiness marker"
      close_failed_daemon_update_boundary "$DAEMON_BOUNDARY_ERROR"
      return 1
      ;;
  esac
  if ! kill -0 "$pid" 2>/dev/null; then
    DAEMON_BOUNDARY_ERROR="candidate exited instead of retaining daemon lifecycle authority"
    close_failed_daemon_update_boundary "$DAEMON_BOUNDARY_ERROR"
    return 1
  fi

  DAEMON_BOUNDARY_ACTIVE=true
  DAEMON_BOUNDARY_ROLLBACK_SAFE=true
  DAEMON_BOUNDARY_PHASE=stopped
  if [ "$DAEMON_SERVICE_INSTALLED" = true ] \
    && [ "$initial_executable" != "$replacement_executable" ]
  then
    if [ "$DAEMON_WAS_RUNNING" != true ]; then
      DAEMON_BOUNDARY_ERROR="the installed daemon service is stopped and cannot be migrated without changing its prior state"
      return 1
    fi
  fi
  DAEMON_STATE_CAPTURED=true
  return 0
}

request_daemon_update_boundary_new_state() {
  local marker
  [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ] \
    && [ "${DAEMON_BOUNDARY_PHASE:-}" = stopped ] || {
    DAEMON_BOUNDARY_ERROR="daemon lifecycle holder was not in its stopped replacement phase"
    return 1
  }
  if ! printf 'new\n' >&7 || ! IFS= read -r marker <&6; then
    DAEMON_BOUNDARY_ROLLBACK_SAFE=false
    DAEMON_BOUNDARY_ERROR="daemon lifecycle holder exited while publishing the replacement state"
    return 1
  fi
  case "$marker" in
    "${DAEMON_BOUNDARY_PROTOCOL_PREFIX} new state ready")
      DAEMON_BOUNDARY_PHASE=new
      DAEMON_BOUNDARY_ROLLBACK_SAFE=false
      return 0
      ;;
    "${DAEMON_BOUNDARY_PROTOCOL_PREFIX} new state failed")
      DAEMON_BOUNDARY_ERROR="replacement daemon state was rejected; daemon absence was re-established for rollback"
      return 1
      ;;
    *)
      DAEMON_BOUNDARY_ROLLBACK_SAFE=false
      DAEMON_BOUNDARY_ERROR="daemon lifecycle holder returned an invalid replacement-state marker"
      return 1
      ;;
  esac
}

request_daemon_update_boundary_uninstall_state() {
  local marker
  [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ] \
    && [ "${DAEMON_BOUNDARY_PHASE:-}" = stopped ] || {
    DAEMON_BOUNDARY_ERROR="daemon lifecycle holder was not in its stopped uninstall phase"
    return 1
  }
  if ! printf 'uninstall\n' >&7 || ! IFS= read -r marker <&6; then
    DAEMON_BOUNDARY_ROLLBACK_SAFE=false
    DAEMON_BOUNDARY_ERROR="daemon lifecycle holder exited while applying the uninstall state"
    return 1
  fi
  case "$marker" in
    "${DAEMON_BOUNDARY_PROTOCOL_PREFIX} uninstall state ready")
      DAEMON_BOUNDARY_PHASE=uninstall
      DAEMON_BOUNDARY_ROLLBACK_SAFE=false
      return 0
      ;;
    "${DAEMON_BOUNDARY_PROTOCOL_PREFIX} uninstall state failed")
      DAEMON_BOUNDARY_ERROR="daemon uninstall state was rejected; exact stopped state was retained for rollback"
      return 1
      ;;
    *)
      DAEMON_BOUNDARY_ROLLBACK_SAFE=false
      DAEMON_BOUNDARY_ERROR="daemon lifecycle holder returned an invalid uninstall-state marker"
      return 1
      ;;
  esac
}

restore_daemon_update_boundary_old_state() {
  local marker
  [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ] \
    && [ "${DAEMON_BOUNDARY_PHASE:-}" = stopped ] \
    && [ "${DAEMON_BOUNDARY_ROLLBACK_SAFE:-false}" = true ] || {
    DAEMON_BOUNDARY_ERROR="daemon lifecycle rollback authority was not retained"
    return 1
  }
  if ! printf 'rollback\n' >&7 || ! IFS= read -r marker <&6; then
    DAEMON_BOUNDARY_ERROR="daemon lifecycle holder exited before restoring the old state"
    close_failed_daemon_update_boundary "$DAEMON_BOUNDARY_ERROR"
    return 1
  fi
  case "$marker" in
    "${DAEMON_BOUNDARY_PROTOCOL_PREFIX} old state restored")
      DAEMON_BOUNDARY_PHASE=restored
      wait_daemon_update_boundary true
      ;;
    "${DAEMON_BOUNDARY_PROTOCOL_PREFIX} old state failed")
      DAEMON_BOUNDARY_ERROR="the prior daemon could not be restarted; exact daemon absence was re-established"
      close_failed_daemon_update_boundary "$DAEMON_BOUNDARY_ERROR"
      return 1
      ;;
    *)
      DAEMON_BOUNDARY_ERROR="daemon lifecycle holder returned an invalid rollback marker"
      close_failed_daemon_update_boundary "$DAEMON_BOUNDARY_ERROR"
      return 1
      ;;
  esac
}

finish_daemon_update_boundary() {
  local marker
  [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ] \
    && { [ "${DAEMON_BOUNDARY_PHASE:-}" = new ] \
      || [ "${DAEMON_BOUNDARY_PHASE:-}" = uninstall ]; } || {
    DAEMON_BOUNDARY_ERROR="daemon lifecycle holder was not ready for final commit"
    return 1
  }
  if ! printf 'finish\n' >&7 || ! IFS= read -r marker <&6; then
    DAEMON_BOUNDARY_ERROR="daemon lifecycle holder exited before final state confirmation"
    close_failed_daemon_update_boundary "$DAEMON_BOUNDARY_ERROR"
    return 1
  fi
  if [ "$marker" != "${DAEMON_BOUNDARY_PROTOCOL_PREFIX} final state confirmed" ]; then
    DAEMON_BOUNDARY_ERROR="daemon lifecycle holder returned an invalid final-state marker"
    close_failed_daemon_update_boundary "$DAEMON_BOUNDARY_ERROR"
    return 1
  fi
  DAEMON_BOUNDARY_PHASE=confirmed
}

release_daemon_update_boundary() {
  local marker
  [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ] \
    && [ "${DAEMON_BOUNDARY_PHASE:-}" = confirmed ] || {
    DAEMON_BOUNDARY_ERROR="daemon lifecycle holder was not ready to release final authority"
    return 1
  }
  if ! printf 'release\n' >&7 || ! IFS= read -r marker <&6; then
    DAEMON_BOUNDARY_ERROR="daemon lifecycle holder exited before authority release confirmation"
    close_failed_daemon_update_boundary "$DAEMON_BOUNDARY_ERROR"
    return 1
  fi
  if [ "$marker" != "${DAEMON_BOUNDARY_PROTOCOL_PREFIX} lifecycle authority released" ]; then
    DAEMON_BOUNDARY_ERROR="daemon lifecycle holder returned an invalid authority-release marker"
    close_failed_daemon_update_boundary "$DAEMON_BOUNDARY_ERROR"
    return 1
  fi
  DAEMON_BOUNDARY_PHASE=released
  wait_daemon_update_boundary true
}

abandon_daemon_update_boundary() {
  [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ] || return 0
  wait_daemon_update_boundary false
}

cleanup_daemon_update_boundary_on_exit() {
  DAEMON_BOUNDARY_EXIT_CLEANUP_ERROR=""
  [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ] || return 0
  if ! abandon_daemon_update_boundary; then
    DAEMON_BOUNDARY_EXIT_CLEANUP_ERROR="$DAEMON_BOUNDARY_ERROR"
    return 1
  fi
  DAEMON_BOUNDARY_EXIT_CLEANUP_ERROR="daemon lifecycle transaction ended without an explicit finish or rollback; its PID/service authority was released fail-closed"
  return 1
}

cleanup_update_locks_on_exit() {
  local wait_status
  UPDATE_LOCK_EXIT_CLEANUP_ERROR=""
  exec 9>&- 8>&-
  if [ -n "${UPDATE_LOCK_PID_9:-}" ]; then
    if wait "$UPDATE_LOCK_PID_9" >/dev/null 2>&1; then
      :
    else
      wait_status=$?
      UPDATE_LOCK_EXIT_CLEANUP_ERROR="lock-holder PID ${UPDATE_LOCK_PID_9} exited with status ${wait_status} during EXIT cleanup"
    fi
    UPDATE_LOCK_PID_9=""
  fi
  if [ -n "${UPDATE_LOCK_PID_8:-}" ]; then
    if wait "$UPDATE_LOCK_PID_8" >/dev/null 2>&1; then
      :
    else
      wait_status=$?
      if [ -n "$UPDATE_LOCK_EXIT_CLEANUP_ERROR" ]; then
        UPDATE_LOCK_EXIT_CLEANUP_ERROR="${UPDATE_LOCK_EXIT_CLEANUP_ERROR}; lock-holder PID ${UPDATE_LOCK_PID_8} exited with status ${wait_status} during EXIT cleanup"
      else
        UPDATE_LOCK_EXIT_CLEANUP_ERROR="lock-holder PID ${UPDATE_LOCK_PID_8} exited with status ${wait_status} during EXIT cleanup"
      fi
    fi
    UPDATE_LOCK_PID_8=""
  fi
  [ -z "$UPDATE_LOCK_EXIT_CLEANUP_ERROR" ]
}

cleanup_install_exit() {
  local original_status=$? cleanup_status=0 cleanup_errors=""
  trap - EXIT
  # Lock order is update -> daemon/service. End the inner lifecycle holder
  # before cleanup. Keep the outer update locks through exact fixed-stage
  # cleanup so another installer cannot enter those public transaction slots.
  if ! cleanup_daemon_update_boundary_on_exit; then
    cleanup_errors="$DAEMON_BOUNDARY_EXIT_CLEANUP_ERROR"
  fi
  if ! cleanup_install_artifacts; then
    if [ -n "$cleanup_errors" ]; then
      cleanup_errors="${cleanup_errors}; ${INSTALL_ARTIFACT_CLEANUP_ERROR}"
    else
      cleanup_errors="$INSTALL_ARTIFACT_CLEANUP_ERROR"
    fi
  fi
  if ! cleanup_update_locks_on_exit; then
    if [ -n "$cleanup_errors" ]; then
      cleanup_errors="${cleanup_errors}; ${UPDATE_LOCK_EXIT_CLEANUP_ERROR}"
    else
      cleanup_errors="$UPDATE_LOCK_EXIT_CLEANUP_ERROR"
    fi
  fi
  if ! cleanup_installer_temp_directory; then
    if [ -n "$cleanup_errors" ]; then
      cleanup_errors="${cleanup_errors}; ${TMP_CLEANUP_ERROR}"
    else
      cleanup_errors="$TMP_CLEANUP_ERROR"
    fi
  fi
  if [ -n "$cleanup_errors" ]; then
    printf '\033[0;31m[error]\033[0m Installer EXIT cleanup failed: %s\n' \
      "$cleanup_errors" >&2
    cleanup_status=1
  fi
  if [ "$original_status" -ne 0 ]; then
    exit "$original_status"
  fi
  exit "$cleanup_status"
}

cleanup_installer_temp_directory() {
  local observed_parent observed_parent_identity observed_identity
  [ -n "${TMP_DIR:-}" ] || return 0
  case "$TMP_DIR" in
    /*) ;;
    *)
      TMP_CLEANUP_ERROR="recorded temporary path is not absolute: ${TMP_DIR}"
      return 1
      ;;
  esac
  observed_parent="$(dirname "$TMP_DIR")"
  if [ "$observed_parent" != "${TMP_DIR_PARENT:-}" ]; then
    TMP_CLEANUP_ERROR="recorded temporary parent changed: ${TMP_DIR}"
    return 1
  fi
  case "$TMP_DIR" in
    "${TMP_DIR_PARENT%/}/"*) ;;
    *)
      TMP_CLEANUP_ERROR="temporary path is not a direct descendant of its recorded root: ${TMP_DIR}"
      return 1
      ;;
  esac
  observed_parent_identity="$(file_identity "$observed_parent" 2>/dev/null)" || {
    TMP_CLEANUP_ERROR="could not identify recorded temporary parent ${observed_parent}"
    return 1
  }
  if [ "$observed_parent_identity" != "${TMP_DIR_PARENT_IDENTITY:-}" ]; then
    TMP_CLEANUP_ERROR="temporary parent identity changed; preserved ${TMP_DIR}"
    return 1
  fi
  if [ -L "$TMP_DIR" ] || [ ! -d "$TMP_DIR" ]; then
    TMP_CLEANUP_ERROR="temporary path is no longer the direct directory created by this installer: ${TMP_DIR}"
    return 1
  fi
  observed_identity="$(file_identity "$TMP_DIR" 2>/dev/null)" || {
    TMP_CLEANUP_ERROR="could not identify installer temporary directory ${TMP_DIR}"
    return 1
  }
  if [ "$observed_identity" != "${TMP_DIR_IDENTITY:-}" ]; then
    TMP_CLEANUP_ERROR="temporary directory identity changed; preserved ${TMP_DIR}"
    return 1
  fi
  if ! rm -rf -- "$TMP_DIR"; then
    TMP_CLEANUP_ERROR="recursive removal failed for verified temporary directory ${TMP_DIR}"
    return 1
  fi
  if [ -e "$TMP_DIR" ] || [ -L "$TMP_DIR" ]; then
    TMP_CLEANUP_ERROR="verified temporary directory still exists after removal: ${TMP_DIR}"
    return 1
  fi
  TMP_DIR=""
  return 0
}

start_update_lock() {
  local candidate="$1" target="$2" slot="$3" use_sudo="$4"
  local control="${TMP_DIR}/update-lock-${slot}.control"
  local ready="${TMP_DIR}/update-lock-${slot}.ready"
  local marker pid
  mkfifo "$control" "$ready" || return 1

  if [ "$use_sudo" = true ]; then
    sudo env CS_UPDATE_LOCK_TARGET="$target" \
      "$candidate" __hold-update-lock 8>&- 9>&- \
      < "$control" > "$ready" &
  else
    CS_UPDATE_LOCK_TARGET="$target" \
      "$candidate" __hold-update-lock 8>&- 9>&- \
      < "$control" > "$ready" &
  fi
  pid=$!
  if [ "$slot" = 8 ]; then
    UPDATE_LOCK_PID_8="$pid"
    exec 8>"$control"
  else
    UPDATE_LOCK_PID_9="$pid"
    exec 9>"$control"
  fi

  if ! IFS= read -r marker < "$ready"; then
    UPDATE_LOCK_ERROR="candidate did not acquire the shared update lock for ${target}"
    cleanup_update_locks_on_exit
    return 1
  fi
  if [ "$marker" != "codex-switch-global-pace update lock ready" ]; then
    UPDATE_LOCK_ERROR="candidate returned an invalid update-lock readiness marker for ${target}"
    cleanup_update_locks_on_exit
    return 1
  fi
  if ! kill -0 "$pid" 2>/dev/null; then
    UPDATE_LOCK_ERROR="candidate exited instead of holding the shared update lock for ${target}"
    cleanup_update_locks_on_exit
    return 1
  fi
}

start_install_update_locks() {
  local candidate="$1"
  # Every transaction that touches the system target acquires it first. A
  # user migration then acquires its user target, so two installers can never
  # hold these two shared locks in opposite order.
  if [ "$MIGRATE_LEGACY" = true ]; then
    start_update_lock "$candidate" "$LEGACY_BIN" 8 "$LEGACY_NEEDS_SUDO" || return 1
    start_update_lock "$candidate" "$INSTALL_DEST" 9 false || return 1
  else
    start_update_lock "$candidate" "$INSTALL_DEST" 8 "$INSTALL_WITH_SUDO"
  fi
}

release_update_locks() {
  local failed=false
  if [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ]; then
    UPDATE_LOCK_ERROR="refusing to release update locks while the daemon lifecycle holder is active"
    return 1
  fi
  exec 9>&- 8>&-
  if [ -n "${UPDATE_LOCK_PID_9:-}" ]; then
    wait "$UPDATE_LOCK_PID_9" || failed=true
    UPDATE_LOCK_PID_9=""
  fi
  if [ -n "${UPDATE_LOCK_PID_8:-}" ]; then
    wait "$UPDATE_LOCK_PID_8" || failed=true
    UPDATE_LOCK_PID_8=""
  fi
  if [ "$failed" = true ]; then
    UPDATE_LOCK_ERROR="an update-lock helper did not exit successfully after the transaction"
    return 1
  fi
}

prepare_daemon_upgrade() {
  DAEMON_PREVIOUS_BIN=""
  DAEMON_STATE_CAPTURED=false
  DAEMON_WAS_RUNNING=false
  DAEMON_SERVICE_INSTALLED=false
  DAEMON_STATUS_ERROR=""

  if [ -x "$INSTALL_DEST" ] && [ ! -L "$INSTALL_DEST" ]; then
    DAEMON_PREVIOUS_BIN="$INSTALL_DEST"
  elif [ "$MIGRATE_LEGACY" = true ] && [ -x "$LEGACY_BIN" ] && [ ! -L "$LEGACY_BIN" ]; then
    DAEMON_PREVIOUS_BIN="$LEGACY_BIN"
  else
    # A fresh publication still needs the PID absence lease: without it a
    # foreground daemon can enter after publication and survive a later PATH
    # or marker rollback. The explicit path may not exist yet; service/PID
    # state is captured authoritatively by the holder below.
    DAEMON_PREVIOUS_BIN="$INSTALL_DEST"
    return 0
  fi

  if ! read_checked_daemon_status; then
    DAEMON_STATUS_ERROR="Could not determine the existing daemon state with ${DAEMON_PREVIOUS_BIN}: ${DAEMON_STATUS_ERROR}"
    return 1
  fi
  DAEMON_STATE_CAPTURED=true
  DAEMON_WAS_RUNNING="$DAEMON_STATUS_RUNNING"
  DAEMON_SERVICE_INSTALLED="$DAEMON_STATUS_SERVICE_INSTALLED"

  if [ "$DAEMON_SERVICE_INSTALLED" = true ]; then
    local current_owner_error legacy_owner_error
    if check_candidate_uninstall_owner "$INSTALL_DEST"; then
      :
    else
      current_owner_error="$SERVICE_OWNER_ERROR"
      if [ "$MIGRATE_LEGACY" = true ] \
        && check_candidate_uninstall_owner "$LEGACY_BIN"
      then
        DAEMON_PREVIOUS_BIN="$LEGACY_BIN"
        if [ "$DAEMON_WAS_RUNNING" != true ]; then
          DAEMON_STATUS_ERROR="The installed daemon service is owned by ${LEGACY_BIN}, but it is not running. Uninstall that service before migrating the executable so its stopped state is not changed."
          return 1
        fi
      else
        legacy_owner_error="$SERVICE_OWNER_ERROR"
        DAEMON_STATUS_ERROR="The installed daemon service is not exactly owned by ${INSTALL_DEST} (${current_owner_error})"
        if [ "$MIGRATE_LEGACY" = true ]; then
          DAEMON_STATUS_ERROR="${DAEMON_STATUS_ERROR} or ${LEGACY_BIN} (${legacy_owner_error})"
        fi
        DAEMON_STATUS_ERROR="${DAEMON_STATUS_ERROR}; no binary or service was changed."
        return 1
      fi
    fi
  fi

}

abort_install_upgrade() {
  local reason="$1" rollback_errors=""

  if [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ] \
    && { [ "${DAEMON_BOUNDARY_PHASE:-}" != stopped ] \
      || [ "${DAEMON_BOUNDARY_ROLLBACK_SAFE:-false}" != true ]; }
  then
    rollback_errors="${rollback_errors} daemon lifecycle rollback authority was lost (${DAEMON_BOUNDARY_ERROR});"
  fi

  if [ -z "$rollback_errors" ] && ! restore_held_legacy_install; then
    rollback_errors="${rollback_errors} could not restore the legacy executable;"
  fi

  if [ -z "$rollback_errors" ] && [ "${BINARY_REPLACED:-false}" = true ]; then
    if rollback_installed_binary; then
      BINARY_REPLACED=false
    else
      rollback_errors="${rollback_errors} could not restore the previous executable;"
    fi
  fi

  if [ "${BINARY_REPLACED:-false}" = false ] \
    && { [ -e "${INSTALL_BACKUP:-}" ] || [ -L "${INSTALL_BACKUP:-}" ]; }
  then
    rollback_errors="${rollback_errors} executable rollback state remains at ${INSTALL_BACKUP};"
  fi

  if [ "${SYSTEM_MARKER_CREATED:-false}" = true ]; then
    if [ "${BINARY_REPLACED:-false}" = false ]; then
      if remove_installer_file_owned \
        "$SYSTEM_INSTALL_MARKER" "$SYSTEM_MARKER_CREATED_TOKEN" "$INSTALL_WITH_SUDO"
      then
        SYSTEM_MARKER_CREATED=false
        SYSTEM_MARKER_CREATED_TOKEN=""
      else
        rollback_errors="${rollback_errors} could not prove and remove the new system-install marker;"
      fi
    else
      rollback_errors="${rollback_errors} the new system-install marker was preserved because the replacement system binary remains installed;"
    fi
  fi

  if [ -z "$rollback_errors" ] && ! rollback_managed_path_changes; then
    rollback_errors="${rollback_errors} ${PATH_TRANSACTION_ERROR};"
  fi

  if [ -z "$rollback_errors" ] \
    && [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ] \
    && ! restore_daemon_update_boundary_old_state
  then
    rollback_errors="${rollback_errors} could not restore and verify the prior daemon state: ${DAEMON_BOUNDARY_ERROR};"
  fi

  if [ -n "$rollback_errors" ] && [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ]; then
    if ! abandon_daemon_update_boundary; then
      rollback_errors="${rollback_errors} lifecycle holder shutdown was not confirmed: ${DAEMON_BOUNDARY_ERROR};"
    fi
  fi

  if [ -n "$rollback_errors" ]; then
    preserve_install_backup
  fi
  if ! cleanup_install_artifacts; then
    rollback_errors="${rollback_errors} ${INSTALL_ARTIFACT_CLEANUP_ERROR};"
  fi
  if ! release_update_locks; then
    rollback_errors="${rollback_errors} ${UPDATE_LOCK_ERROR};"
  fi
  if [ -n "$rollback_errors" ]; then
    error "${reason} Rollback was incomplete:${rollback_errors} No unverified executable was removed."
  fi
  error "${reason} The previous executable and daemon state were restored."
}

resolve_path_target() (
  local profile_target="$1"
  local link_target link_hops=0 physical_dir
  while [ -L "$profile_target" ]; do
    link_hops=$((link_hops + 1))
    [ "$link_hops" -le "$SYMLINK_RESOLUTION_MAX_HOPS" ] \
      || error "Symbolic-link resolution exceeded ${SYMLINK_RESOLUTION_MAX_HOPS} hops for $1."
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

render_profile_without_managed_path_block() {
  local source="$1" destination="$2"
  awk -v begin="$PATH_BLOCK_BEGIN" -v end="$PATH_BLOCK_END" '
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
  ' "$source" > "$destination"
}

reset_managed_path_transaction() {
  PATH_TRANSACTION_LOGICAL=()
  PATH_TRANSACTION_TARGET=()
  PATH_TRANSACTION_IDENTITY=()
  PATH_TRANSACTION_ORIGINAL_EXISTS=()
  PATH_TRANSACTION_UPDATED=()
  PATH_TRANSACTION_STAGE=()
  PATH_TRANSACTION_STAGE_TOKEN=()
  PATH_TRANSACTION_COMMITTED_IDENTITY=()
  PATH_TRANSACTION_ACTION=()
  PATH_TRANSACTION_COUNT=0
  PATH_TRANSACTION_COMMITTED=0
  PATH_TRANSACTION_PROFILE_SELECTED=false
  PATH_TRANSACTION_CREATED_PARENT=()
  PATH_TRANSACTION_CREATED_PARENT_IDENTITY=()
  PATH_TRANSACTION_CREATED_PARENT_COUNT=0
  PATH_TRANSACTION_ERROR=""
}

create_profile_parent_chain() {
  local required_parent="$1" ancestor parent identity index
  local missing=()
  ancestor="$required_parent"
  while [ ! -e "$ancestor" ] && [ ! -L "$ancestor" ]; do
    missing[${#missing[@]}]="$ancestor"
    parent="$(dirname "$ancestor")"
    [ "$parent" != "$ancestor" ] || {
      PATH_TRANSACTION_ERROR="could not find an existing ancestor for ${required_parent}"
      return 1
    }
    ancestor="$parent"
  done
  [ ! -L "$ancestor" ] && [ -d "$ancestor" ] || {
    PATH_TRANSACTION_ERROR="profile ancestor is not a direct directory: ${ancestor}"
    return 1
  }

  index=${#missing[@]}
  while [ "$index" -gt 0 ]; do
    index=$((index - 1))
    parent="${missing[$index]}"
    if ! mkdir "$parent"; then
      PATH_TRANSACTION_ERROR="failed to create profile directory ${parent} without adopting another writer's path"
      return 1
    fi
    identity="$(file_identity "$parent")" || {
      if rmdir "$parent"; then
        PATH_TRANSACTION_ERROR="failed to identify new profile directory ${parent}; its exact empty directory was removed"
      else
        PATH_TRANSACTION_ERROR="failed to identify new profile directory ${parent}; exact empty-directory cleanup also failed and the path was preserved"
      fi
      return 1
    }
    PATH_TRANSACTION_CREATED_PARENT[$PATH_TRANSACTION_CREATED_PARENT_COUNT]="$parent"
    PATH_TRANSACTION_CREATED_PARENT_IDENTITY[$PATH_TRANSACTION_CREATED_PARENT_COUNT]="$identity"
    PATH_TRANSACTION_CREATED_PARENT_COUNT=$((PATH_TRANSACTION_CREATED_PARENT_COUNT + 1))
  done
}

assert_no_profile_transaction_residue() {
  local profile_file="$1" profile_target residue suffix
  for suffix in install displaced failed; do
    residue="${profile_file}.${BINARY_NAME}.${suffix}"
    if [ -e "$residue" ] || [ -L "$residue" ]; then
      PATH_TRANSACTION_ERROR="an incomplete PATH transaction remains at ${residue}"
      return 1
    fi
  done
  if [ ! -e "$profile_file" ] && [ ! -L "$profile_file" ]; then
    return 0
  fi
  if ! profile_target="$(resolve_path_target "$profile_file")"; then
    PATH_TRANSACTION_ERROR="failed to resolve ${profile_file} while checking transaction residue"
    return 1
  fi
  for suffix in install displaced failed; do
    residue="${profile_target}.${BINARY_NAME}.${suffix}"
    if [ -e "$residue" ] || [ -L "$residue" ]; then
      PATH_TRANSACTION_ERROR="an incomplete PATH transaction remains at ${residue}"
      return 1
    fi
  done
}

assert_no_managed_path_transaction_residue() {
  local profile_file
  for profile_file in \
    "${HOME}/.zprofile" \
    "${HOME}/.bash_profile" \
    "${HOME}/.profile" \
    "${HOME}/.config/fish/config.fish"
  do
    assert_no_profile_transaction_residue "$profile_file" || return 1
  done
}

prepare_path_block_removal() {
  local profile_file="$1" profile_target profile_identity original original_token updated
  local profile_stage profile_displaced profile_failed
  local index existing_index
  [ -f "$profile_file" ] || return 0
  if ! grep -F "$PATH_BLOCK_BEGIN" "$profile_file" >/dev/null 2>&1 \
    && ! grep -F "$PATH_BLOCK_END" "$profile_file" >/dev/null 2>&1
  then
    return 0
  fi
  if ! profile_target="$(resolve_path_target "$profile_file")"; then
    PATH_TRANSACTION_ERROR="failed to resolve ${profile_file}"
    return 1
  fi
  [ ! -L "$profile_target" ] && [ -f "$profile_target" ] || {
    PATH_TRANSACTION_ERROR="${profile_file} does not resolve to a regular profile file"
    return 1
  }

  existing_index=0
  while [ "$existing_index" -lt "$PATH_TRANSACTION_COUNT" ]; do
    if [ "${PATH_TRANSACTION_TARGET[$existing_index]}" = "$profile_target" ]; then
      return 0
    fi
    existing_index=$((existing_index + 1))
  done

  if ! capture_installer_file_token "$profile_target" false profile_identity; then
    PATH_TRANSACTION_ERROR="failed to identify ${profile_file}"
    return 1
  fi
  index="$PATH_TRANSACTION_COUNT"
  original="${TMP_DIR}/path-${index}.original"
  updated="${TMP_DIR}/path-${index}.updated"
  profile_stage="${profile_target}.${BINARY_NAME}.install"
  profile_displaced="${profile_target}.${BINARY_NAME}.displaced"
  profile_failed="${profile_target}.${BINARY_NAME}.failed"
  if [ -e "$profile_stage" ] || [ -L "$profile_stage" ] \
    || [ -e "$profile_displaced" ] || [ -L "$profile_displaced" ] \
    || [ -e "$profile_failed" ] || [ -L "$profile_failed" ]
  then
    PATH_TRANSACTION_ERROR="an incomplete PATH transaction remains beside ${profile_target}"
    return 1
  fi
  if ! capture_installer_file_copy \
    "$profile_target" "$original" "$profile_identity" false original_token \
    || ! cp -p "$original" "$updated"
  then
    PATH_TRANSACTION_ERROR="failed to stage ${profile_file} for PATH removal"
    return 1
  fi
  if ! render_profile_without_managed_path_block "$original" "$updated"; then
    PATH_TRANSACTION_ERROR="${profile_file} does not contain exactly one complete managed PATH block"
    return 1
  fi

  PATH_TRANSACTION_LOGICAL[$index]="$profile_file"
  PATH_TRANSACTION_TARGET[$index]="$profile_target"
  PATH_TRANSACTION_IDENTITY[$index]="$profile_identity"
  PATH_TRANSACTION_ORIGINAL_EXISTS[$index]=true
  PATH_TRANSACTION_UPDATED[$index]="$updated"
  PATH_TRANSACTION_STAGE[$index]="$profile_stage"
  PATH_TRANSACTION_STAGE_TOKEN[$index]=""
  PATH_TRANSACTION_COMMITTED_IDENTITY[$index]=""
  PATH_TRANSACTION_ACTION[$index]="Removed codex-switch-global-pace PATH entry from ${profile_file}."
  PATH_TRANSACTION_COUNT=$((PATH_TRANSACTION_COUNT + 1))
}

prepare_managed_path_removals() {
  local profile_file
  reset_managed_path_transaction
  assert_no_managed_path_transaction_residue || return 1
  for profile_file in \
    "${HOME}/.zprofile" \
    "${HOME}/.bash_profile" \
    "${HOME}/.profile" \
    "${HOME}/.config/fish/config.fish"
  do
    prepare_path_block_removal "$profile_file" || return 1
  done
}

prepare_managed_path_addition() {
  local profile_file path_line profile_parent profile_target profile_identity
  local original original_token updated profile_stage profile_displaced profile_failed validation index=0
  reset_managed_path_transaction
  assert_no_managed_path_transaction_residue || return 1
  case ":${PATH}:" in
    *":${USER_INSTALL_DIR}:"*) return 0 ;;
  esac
  case "${SHELL:-}" in
    */zsh)
      profile_file="${HOME}/.zprofile"
      path_line='export PATH="$HOME/.local/bin:$PATH"'
      ;;
    */bash)
      if [ "$PLATFORM" = "darwin" ]; then
        profile_file="${HOME}/.bash_profile"
      else
        profile_file="${HOME}/.profile"
      fi
      path_line='export PATH="$HOME/.local/bin:$PATH"'
      ;;
    */fish)
      profile_file="${HOME}/.config/fish/config.fish"
      path_line='fish_add_path "$HOME/.local/bin"'
      ;;
    *) return 0 ;;
  esac
  PATH_TRANSACTION_PROFILE_SELECTED=true
  profile_parent="$(dirname "$profile_file")"
  create_profile_parent_chain "$profile_parent" || return 1
  [ ! -L "$profile_parent" ] && [ -d "$profile_parent" ] || {
    PATH_TRANSACTION_ERROR="profile parent is not a direct directory: ${profile_parent}"
    return 1
  }

  original="${TMP_DIR}/path-${index}.original"
  updated="${TMP_DIR}/path-${index}.updated"
  if [ -e "$profile_file" ] || [ -L "$profile_file" ]; then
    profile_target="$(resolve_path_target "$profile_file")" || {
      PATH_TRANSACTION_ERROR="failed to resolve ${profile_file}"
      return 1
    }
    [ ! -L "$profile_target" ] && [ -f "$profile_target" ] || {
      PATH_TRANSACTION_ERROR="${profile_file} does not resolve to a regular profile file"
      return 1
    }
    if grep -F "$PATH_BLOCK_BEGIN" "$profile_target" >/dev/null 2>&1 \
      || grep -F "$PATH_BLOCK_END" "$profile_target" >/dev/null 2>&1
    then
      validation="${TMP_DIR}/path-${index}.validation"
      render_profile_without_managed_path_block "$profile_target" "$validation" || {
        PATH_TRANSACTION_ERROR="${profile_file} contains an invalid managed PATH block"
        return 1
      }
      return 0
    fi
    capture_installer_file_token "$profile_target" false profile_identity || {
      PATH_TRANSACTION_ERROR="failed to identify ${profile_file}"
      return 1
    }
    capture_installer_file_copy \
      "$profile_target" "$original" "$profile_identity" false original_token \
      && cp -p "$original" "$updated" || {
      PATH_TRANSACTION_ERROR="failed to stage ${profile_file} for PATH addition"
      return 1
      }
    PATH_TRANSACTION_ORIGINAL_EXISTS[$index]=true
  else
    profile_target="$(resolve_path_target "$profile_file")" || {
      PATH_TRANSACTION_ERROR="failed to resolve new profile path ${profile_file}"
      return 1
    }
    profile_identity=""
    original=""
    : > "$updated" || {
      PATH_TRANSACTION_ERROR="failed to stage new profile ${profile_file}"
      return 1
    }
    PATH_TRANSACTION_ORIGINAL_EXISTS[$index]=false
  fi
  profile_stage="${profile_target}.${BINARY_NAME}.install"
  profile_displaced="${profile_target}.${BINARY_NAME}.displaced"
  profile_failed="${profile_target}.${BINARY_NAME}.failed"
  [ ! -e "$profile_stage" ] && [ ! -L "$profile_stage" ] \
    && [ ! -e "$profile_displaced" ] && [ ! -L "$profile_displaced" ] \
    && [ ! -e "$profile_failed" ] && [ ! -L "$profile_failed" ] || {
    PATH_TRANSACTION_ERROR="an incomplete PATH transaction remains beside ${profile_target}"
    return 1
  }
  printf '\n%s\n%s\n%s\n' "$PATH_BLOCK_BEGIN" "$path_line" "$PATH_BLOCK_END" \
    >> "$updated" || {
      PATH_TRANSACTION_ERROR="failed to render managed PATH addition for ${profile_file}"
      return 1
    }

  PATH_TRANSACTION_LOGICAL[$index]="$profile_file"
  PATH_TRANSACTION_TARGET[$index]="$profile_target"
  PATH_TRANSACTION_IDENTITY[$index]="$profile_identity"
  PATH_TRANSACTION_UPDATED[$index]="$updated"
  PATH_TRANSACTION_STAGE[$index]="$profile_stage"
  PATH_TRANSACTION_STAGE_TOKEN[$index]=""
  PATH_TRANSACTION_COMMITTED_IDENTITY[$index]=""
  PATH_TRANSACTION_ACTION[$index]="Added ${USER_INSTALL_DIR} to PATH in ${profile_file}; restart your shell to apply it."
  PATH_TRANSACTION_COUNT=1
}

commit_managed_path_changes() {
  local index logical target expected_identity original_exists current_target
  local stage stage_token updated_token committed_identity stage_after target_after boundary_error
  index=0
  while [ "$index" -lt "$PATH_TRANSACTION_COUNT" ]; do
    logical="${PATH_TRANSACTION_LOGICAL[$index]}"
    target="${PATH_TRANSACTION_TARGET[$index]}"
    expected_identity="${PATH_TRANSACTION_IDENTITY[$index]}"
    original_exists="${PATH_TRANSACTION_ORIGINAL_EXISTS[$index]}"
    stage="${PATH_TRANSACTION_STAGE[$index]}"
    if [ -e "$stage" ] || [ -L "$stage" ]; then
      PATH_TRANSACTION_ERROR="transaction stage ${stage} appeared before ${logical} could be committed"
      return 1
    fi
    capture_installer_file_token \
      "${PATH_TRANSACTION_UPDATED[$index]}" false updated_token || {
      PATH_TRANSACTION_ERROR="failed to bind the rendered PATH update for ${logical}"
      return 1
    }
    capture_installer_file_copy \
      "${PATH_TRANSACTION_UPDATED[$index]}" "$stage" "$updated_token" false \
      stage_token || {
      PATH_TRANSACTION_ERROR="failed to create the fixed PATH transaction stage ${stage}: ${INSTALLER_FILE_OP_ERROR}"
      return 1
    }
    PATH_TRANSACTION_STAGE_TOKEN[$index]="$stage_token"
    if ! current_target="$(resolve_path_target "$logical")"; then
      PATH_TRANSACTION_ERROR="failed to re-resolve ${logical} before commit"
      return 1
    fi
    if [ "$current_target" != "$target" ]; then
      PATH_TRANSACTION_ERROR="profile link changed while updating ${logical}"
      return 1
    fi
    PATH_TRANSACTION_COMMITTED_IDENTITY[$index]="$stage_token"
    if [ "$original_exists" = true ]; then
      if ! exchange_installer_files \
        "$stage" "$target" "$stage_token" "$expected_identity" false
      then
        boundary_error="$INSTALLER_FILE_OP_ERROR"
        stage_after=""
        target_after=""
        capture_installer_file_token "$stage" false stage_after 2>/dev/null || true
        capture_installer_file_token "$target" false target_after 2>/dev/null || true
        if [ "$stage_after" = "$expected_identity" ] \
          && [ "$target_after" = "$stage_token" ]
        then
          PATH_TRANSACTION_STAGE_TOKEN[$index]="$expected_identity"
          PATH_TRANSACTION_COMMITTED=$((index + 1))
          PATH_TRANSACTION_ERROR="profile exchange reached its published state but did not confirm durability: ${boundary_error}"
        else
          PATH_TRANSACTION_ERROR="profile exchange failed without overwriting another writer: ${boundary_error}"
        fi
        return 1
      fi
      PATH_TRANSACTION_STAGE_TOKEN[$index]="$expected_identity"
    else
      if ! move_installer_file_noreplace "$stage" "$target" "$stage_token" false; then
        boundary_error="$INSTALLER_FILE_OP_ERROR"
        target_after=""
        capture_installer_file_token "$target" false target_after 2>/dev/null || true
        if [ "$target_after" = "$stage_token" ] \
          && [ ! -e "$stage" ] && [ ! -L "$stage" ]
        then
          PATH_TRANSACTION_STAGE_TOKEN[$index]=""
          PATH_TRANSACTION_COMMITTED=$((index + 1))
        fi
        PATH_TRANSACTION_ERROR="failed to publish new profile ${logical} without replacing another writer: ${boundary_error}"
        return 1
      fi
      PATH_TRANSACTION_STAGE_TOKEN[$index]=""
    fi
    PATH_TRANSACTION_COMMITTED=$((index + 1))
    if ! capture_installer_file_token "$target" false committed_identity; then
      PATH_TRANSACTION_ERROR="failed to identify committed profile ${logical}"
      return 1
    fi
    if [ "$committed_identity" != "${PATH_TRANSACTION_COMMITTED_IDENTITY[$index]}" ]; then
      PATH_TRANSACTION_ERROR="committed profile changed while updating ${logical}"
      return 1
    fi
    info "${PATH_TRANSACTION_ACTION[$index]}"
    index=$((index + 1))
  done
}

rollback_managed_path_changes() {
  local index logical target expected_identity original_identity original_exists
  local current_target current_identity stage stage_after target_after failed=false
  index="$PATH_TRANSACTION_COMMITTED"
  while [ "$index" -gt 0 ]; do
    index=$((index - 1))
    logical="${PATH_TRANSACTION_LOGICAL[$index]}"
    target="${PATH_TRANSACTION_TARGET[$index]}"
    expected_identity="${PATH_TRANSACTION_COMMITTED_IDENTITY[$index]}"
    original_identity="${PATH_TRANSACTION_IDENTITY[$index]}"
    original_exists="${PATH_TRANSACTION_ORIGINAL_EXISTS[$index]}"
    stage="${PATH_TRANSACTION_STAGE[$index]}"
    if [ "$original_exists" = true ]; then
      if [ -z "$expected_identity" ] \
        || ! current_target="$(resolve_path_target "$logical")" \
        || [ "$current_target" != "$target" ] \
        || ! capture_installer_file_token "$current_target" false current_identity \
        || [ "$current_identity" != "$expected_identity" ] \
        || ! installer_file_token_matches "$stage" false "$original_identity"
      then
        failed=true
        PATH_TRANSACTION_ERROR="could not safely restore ${logical}; the exact displaced original remains at ${stage}"
      else
        if ! exchange_installer_files \
          "$stage" "$target" "$original_identity" "$expected_identity" false
        then
          stage_after=""
          target_after=""
          capture_installer_file_token "$stage" false stage_after 2>/dev/null || true
          capture_installer_file_token "$target" false target_after 2>/dev/null || true
          failed=true
          if [ "$stage_after" = "$expected_identity" ] \
            && [ "$target_after" = "$original_identity" ]
          then
            PATH_TRANSACTION_STAGE_TOKEN[$index]="$expected_identity"
            PATH_TRANSACTION_COMMITTED="$index"
            PATH_TRANSACTION_ERROR="restored ${logical}, but rollback durability was not confirmed; the failed candidate remains at ${stage}"
          else
            PATH_TRANSACTION_ERROR="could not atomically restore ${logical}; all exchange operands were preserved"
          fi
        else
          PATH_TRANSACTION_STAGE_TOKEN[$index]="$expected_identity"
          if ! installer_file_token_matches "$target" false "$original_identity"; then
            failed=true
            PATH_TRANSACTION_ERROR="restored profile identity did not match the captured ${logical}"
          elif ! remove_installer_file_owned "$stage" "$expected_identity" false; then
            failed=true
            PATH_TRANSACTION_ERROR="restored ${logical}, but the failed candidate remains at ${stage}"
          else
            PATH_TRANSACTION_STAGE_TOKEN[$index]=""
            PATH_TRANSACTION_COMMITTED="$index"
          fi
        fi
      fi
    elif [ -z "$expected_identity" ] \
      || ! current_target="$(resolve_path_target "$logical")" \
      || [ "$current_target" != "$target" ] \
      || ! capture_installer_file_token "$current_target" false current_identity \
      || [ "$current_identity" != "$expected_identity" ]
    then
      failed=true
      PATH_TRANSACTION_ERROR="could not safely remove the profile created at ${logical}; a different path was left unchanged"
    else
      if ! remove_installer_file_owned "$target" "$expected_identity" false; then
        failed=true
        PATH_TRANSACTION_ERROR="could not safely remove the profile created at ${logical}"
      else
        PATH_TRANSACTION_COMMITTED="$index"
      fi
    fi
  done
  index=0
  while [ "$index" -lt "$PATH_TRANSACTION_COUNT" ]; do
    stage="${PATH_TRANSACTION_STAGE[$index]}"
    if [ -e "$stage" ] || [ -L "$stage" ]; then
      if [ "${PATH_TRANSACTION_ORIGINAL_EXISTS[$index]}" = true ] \
        && [ "$index" -lt "$PATH_TRANSACTION_COMMITTED" ]
      then
        failed=true
        PATH_TRANSACTION_ERROR="exact displaced profile remains at ${stage}"
      elif [ -n "${PATH_TRANSACTION_STAGE_TOKEN[$index]}" ] \
        && remove_installer_file_owned \
          "$stage" "${PATH_TRANSACTION_STAGE_TOKEN[$index]}" false
      then
        PATH_TRANSACTION_STAGE_TOKEN[$index]=""
      else
        failed=true
        PATH_TRANSACTION_ERROR="PATH transaction residue remains at ${stage}"
      fi
    fi
    index=$((index + 1))
  done
  index="$PATH_TRANSACTION_CREATED_PARENT_COUNT"
  while [ "$index" -gt 0 ]; do
    index=$((index - 1))
    if [ "$(file_identity "${PATH_TRANSACTION_CREATED_PARENT[$index]}" 2>/dev/null || true)" \
        = "${PATH_TRANSACTION_CREATED_PARENT_IDENTITY[$index]}" ] \
      && rmdir "${PATH_TRANSACTION_CREATED_PARENT[$index]}" 2>/dev/null
    then
      PATH_TRANSACTION_CREATED_PARENT_COUNT="$index"
    else
      failed=true
      PATH_TRANSACTION_ERROR="could not safely remove new profile directory ${PATH_TRANSACTION_CREATED_PARENT[$index]}"
      break
    fi
  done
  [ "$failed" = false ]
}

finalize_managed_path_changes() {
  local index stage token
  index=0
  while [ "$index" -lt "$PATH_TRANSACTION_COUNT" ]; do
    if [ "${PATH_TRANSACTION_ORIGINAL_EXISTS[$index]}" = true ]; then
      stage="${PATH_TRANSACTION_STAGE[$index]}"
      token="${PATH_TRANSACTION_STAGE_TOKEN[$index]}"
      if [ -z "$token" ] \
        || ! remove_installer_file_owned "$stage" "$token" false
      then
        PATH_TRANSACTION_ERROR="committed PATH recovery file could not be removed safely: ${stage}"
        return 1
      fi
      PATH_TRANSACTION_STAGE_TOKEN[$index]=""
    fi
    index=$((index + 1))
  done
}

managed_path_block_exists() {
  local profile_file
  for profile_file in \
    "${HOME}/.zprofile" \
    "${HOME}/.bash_profile" \
    "${HOME}/.profile" \
    "${HOME}/.config/fish/config.fish"
  do
    if [ -f "$profile_file" ] && grep -F "$PATH_BLOCK_BEGIN" "$profile_file" >/dev/null 2>&1; then
      return 0
    fi
  done
  return 1
}

daemon_pid_state_exists() {
  local path
  for path in "${DATA_DIR}/daemon.pid" "${DATA_DIR}/daemon.pid.lock"; do
    if [ -e "$path" ] || [ -L "$path" ]; then
      return 0
    fi
  done
  return 1
}

check_candidate_uninstall_owner() {
  SERVICE_OWNER_ERROR=""
  SERVICE_OWNER_ERROR="$("$CANDIDATE_BIN" daemon uninstall \
    --expected-executable "$1" --check-owner 8>&- 9>&- 2>&1)"
}

begin_uninstall_file_transaction() {
  UNINSTALL_HOLD="${BIN_DIR}/${UNINSTALL_HOLD_NAME}"
  UNINSTALL_STAGE="${BIN_DIR}/${INSTALL_STAGE_NAME}"
  [ ! -e "$UNINSTALL_HOLD" ] && [ ! -L "$UNINSTALL_HOLD" ] \
    && [ ! -e "$UNINSTALL_STAGE" ] && [ ! -L "$UNINSTALL_STAGE" ] || {
    UNINSTALL_TRANSACTION_ERROR="uninstall transaction residue already exists in ${BIN_DIR}"
    return 1
  }
  UNINSTALL_FILE_TRANSACTION_OPEN=false
  UNINSTALL_ORIGINAL_TOKEN=""
  UNINSTALL_STAGE_TOKEN=""
  UNINSTALL_PUBLIC_TOKEN=""
  if [ "$UNINSTALL_BINARY_PRESENT" = true ]; then
    capture_installer_file_token \
      "$BIN_PATH" "$UNINSTALL_WITH_SUDO" UNINSTALL_ORIGINAL_TOKEN || {
      UNINSTALL_TRANSACTION_ERROR="failed to identify ${BIN_PATH} before opening its uninstall transaction"
      return 1
    }
    capture_installer_file_copy \
      "$BIN_PATH" "$UNINSTALL_HOLD" "$UNINSTALL_ORIGINAL_TOKEN" \
      "$UNINSTALL_WITH_SUDO" UNINSTALL_HOLD_TOKEN || {
      UNINSTALL_TRANSACTION_ERROR="failed to create an independent fixed uninstall recovery copy: ${INSTALLER_FILE_OP_ERROR}"
      return 1
    }
    [ "$(file_token_digest "$UNINSTALL_HOLD_TOKEN")" \
      = "$(file_token_digest "$UNINSTALL_ORIGINAL_TOKEN")" ] || {
      UNINSTALL_TRANSACTION_ERROR="the independent uninstall recovery copy does not match the captured binary"
      return 1
    }
    if ! capture_empty_installer_file \
      "$UNINSTALL_STAGE" "$UNINSTALL_WITH_SUDO" UNINSTALL_STAGE_TOKEN
    then
      local placeholder_error="${INSTALLER_FILE_OP_ERROR}"
      if remove_installer_file_owned \
        "$UNINSTALL_HOLD" "$UNINSTALL_HOLD_TOKEN" "$UNINSTALL_WITH_SUDO"
      then
        UNINSTALL_TRANSACTION_ERROR="failed to create the fixed uninstall placeholder: ${placeholder_error}; the exact independent recovery copy was removed"
      else
        UNINSTALL_TRANSACTION_ERROR="failed to create the fixed uninstall placeholder: ${placeholder_error}; exact independent recovery cleanup also failed: ${INSTALLER_FILE_OP_ERROR}"
      fi
      return 1
    fi
    UNINSTALL_PUBLIC_TOKEN="$UNINSTALL_STAGE_TOKEN"
  else
    capture_empty_installer_file \
      "$UNINSTALL_HOLD" "$UNINSTALL_WITH_SUDO" UNINSTALL_HOLD_TOKEN || {
      UNINSTALL_TRANSACTION_ERROR="failed to create the fixed uninstall boundary: ${INSTALLER_FILE_OP_ERROR}"
      return 1
    }
  fi
  UNINSTALL_FILE_TRANSACTION_OPEN=true
}

hold_uninstall_binary_for_commit() {
  local boundary_error binary_after stage_after
  [ "${UNINSTALL_FILE_TRANSACTION_OPEN:-false}" = true ] || return 1
  installer_file_token_matches \
    "$UNINSTALL_HOLD" "$UNINSTALL_WITH_SUDO" "$UNINSTALL_HOLD_TOKEN" || return 1
  if [ "$UNINSTALL_BINARY_PRESENT" = true ]; then
    if ! exchange_installer_files \
      "$UNINSTALL_STAGE" "$BIN_PATH" "$UNINSTALL_STAGE_TOKEN" \
      "$UNINSTALL_ORIGINAL_TOKEN" "$UNINSTALL_WITH_SUDO"
    then
      boundary_error="$INSTALLER_FILE_OP_ERROR"
      binary_after=""
      stage_after=""
      capture_installer_file_token \
        "$BIN_PATH" "$UNINSTALL_WITH_SUDO" binary_after 2>/dev/null || true
      capture_installer_file_token \
        "$UNINSTALL_STAGE" "$UNINSTALL_WITH_SUDO" stage_after 2>/dev/null || true
      if [ "$binary_after" = "$UNINSTALL_PUBLIC_TOKEN" ] \
        && [ "$stage_after" = "$UNINSTALL_ORIGINAL_TOKEN" ]
      then
        UNINSTALL_STAGE_TOKEN="$UNINSTALL_ORIGINAL_TOKEN"
      fi
      UNINSTALL_TRANSACTION_ERROR="uninstall placeholder exchange failed or did not confirm durability: ${boundary_error}"
      return 1
    fi
    UNINSTALL_STAGE_TOKEN="$UNINSTALL_ORIGINAL_TOKEN"
  fi
}

rollback_uninstall_file_transaction() {
  local current_token stage_token restored_token restored_digest
  [ "${UNINSTALL_FILE_TRANSACTION_OPEN:-false}" = true ] || return 0
  installer_file_token_matches \
    "$UNINSTALL_HOLD" "$UNINSTALL_WITH_SUDO" "$UNINSTALL_HOLD_TOKEN" || return 1
  if [ "$UNINSTALL_BINARY_PRESENT" = true ]; then
    capture_installer_file_token \
      "$BIN_PATH" "$UNINSTALL_WITH_SUDO" current_token || return 1
    capture_installer_file_token \
      "$UNINSTALL_STAGE" "$UNINSTALL_WITH_SUDO" stage_token || return 1
    if [ "$current_token" = "$UNINSTALL_PUBLIC_TOKEN" ] \
      && [ "$stage_token" = "$UNINSTALL_ORIGINAL_TOKEN" ]
    then
      exchange_installer_files \
        "$UNINSTALL_STAGE" "$BIN_PATH" "$UNINSTALL_ORIGINAL_TOKEN" \
        "$UNINSTALL_PUBLIC_TOKEN" "$UNINSTALL_WITH_SUDO" || return 1
      UNINSTALL_STAGE_TOKEN="$UNINSTALL_PUBLIC_TOKEN"
    elif [ "$current_token" != "$UNINSTALL_ORIGINAL_TOKEN" ] \
      || [ "$stage_token" != "$UNINSTALL_PUBLIC_TOKEN" ]
    then
      return 1
    fi
    capture_installer_file_token \
      "$BIN_PATH" "$UNINSTALL_WITH_SUDO" restored_token || return 1
    [ "$restored_token" = "$UNINSTALL_ORIGINAL_TOKEN" ] || return 1
    restored_digest="$(file_token_digest "$restored_token")" || return 1
    [ "$restored_digest" = "$(file_token_digest "$UNINSTALL_HOLD_TOKEN")" ] || return 1
    remove_installer_file_owned \
      "$UNINSTALL_STAGE" "$UNINSTALL_PUBLIC_TOKEN" "$UNINSTALL_WITH_SUDO" || return 1
  elif [ -e "$BIN_PATH" ] || [ -L "$BIN_PATH" ]; then
    return 1
  fi
  remove_installer_file_owned \
    "$UNINSTALL_HOLD" "$UNINSTALL_HOLD_TOKEN" "$UNINSTALL_WITH_SUDO" || return 1
  UNINSTALL_FILE_TRANSACTION_OPEN=false
}

commit_uninstall_file_transaction() {
  [ "${UNINSTALL_FILE_TRANSACTION_OPEN:-false}" = true ] || return 1
  if [ "$UNINSTALL_BINARY_PRESENT" = true ]; then
    installer_file_token_matches \
      "$BIN_PATH" "$UNINSTALL_WITH_SUDO" "$UNINSTALL_PUBLIC_TOKEN" || return 1
    installer_file_token_matches \
      "$UNINSTALL_STAGE" "$UNINSTALL_WITH_SUDO" "$UNINSTALL_ORIGINAL_TOKEN" || return 1
  else
    [ ! -e "$BIN_PATH" ] && [ ! -L "$BIN_PATH" ] || return 1
  fi
  if [ "$UNINSTALL_SYSTEM_MARKER_PRESENT" = true ]; then
    remove_installer_file_owned \
      "$SYSTEM_INSTALL_MARKER" "$UNINSTALL_SYSTEM_MARKER_TOKEN" "$UNINSTALL_WITH_SUDO" \
      || return 1
  fi
  if [ "$UNINSTALL_BINARY_PRESENT" = true ]; then
    remove_installer_file_owned \
      "$BIN_PATH" "$UNINSTALL_PUBLIC_TOKEN" "$UNINSTALL_WITH_SUDO" || return 1
  fi
  remove_installer_file_owned \
    "$UNINSTALL_HOLD" "$UNINSTALL_HOLD_TOKEN" "$UNINSTALL_WITH_SUDO" || return 1
  if [ "$UNINSTALL_BINARY_PRESENT" = true ]; then
    remove_installer_file_owned \
      "$UNINSTALL_STAGE" "$UNINSTALL_ORIGINAL_TOKEN" "$UNINSTALL_WITH_SUDO" \
      || return 1
  fi
  UNINSTALL_FILE_TRANSACTION_OPEN=false
}

abort_uninstall_transaction() {
  local reason="$1" rollback_errors=""
  if ! rollback_managed_path_changes; then
    rollback_errors="${rollback_errors} ${PATH_TRANSACTION_ERROR};"
  fi
  if ! rollback_uninstall_file_transaction; then
    rollback_errors="${rollback_errors} could not safely restore ${BIN_PATH} from its exact displaced/recovery files;"
  fi
  if [ -z "$rollback_errors" ] \
    && [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ] \
    && ! restore_daemon_update_boundary_old_state
  then
    rollback_errors="${rollback_errors} could not restore the prior daemon/service state: ${DAEMON_BOUNDARY_ERROR};"
  elif [ -n "$rollback_errors" ] \
    && [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ]
  then
    if ! abandon_daemon_update_boundary; then
      rollback_errors="${rollback_errors} daemon lifecycle holder did not close cleanly: ${DAEMON_BOUNDARY_ERROR};"
    else
      rollback_errors="${rollback_errors} daemon was kept stopped because exact file/PATH rollback was not established;"
    fi
  fi
  if ! release_update_locks; then
    rollback_errors="${rollback_errors} ${UPDATE_LOCK_ERROR};"
  fi
  if [ -n "$rollback_errors" ]; then
    error "${reason} Rollback was incomplete:${rollback_errors} Fixed transaction residue was preserved for inspection."
  fi
  error "${reason} The binary and managed PATH configuration were restored."
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
INSTALL_DEST="${INSTALL_DIR}/${BINARY_NAME}"

# ── Uninstall ────────────────────────────────────────────
run_uninstall() {
  info "Uninstalling codex-switch-global-pace..."

  BIN_PATH=""
  classify_binary_ownership "$INSTALL_DEST"
  if [ "$BINARY_KIND" = "homebrew" ]; then
    error "Homebrew-managed install detected at ${BINARY_RESOLVED}. Run 'brew uninstall codex-switch-global-pace'; the direct uninstaller did not change Homebrew files."
  elif [ "$BINARY_KIND" = "direct" ]; then
    [ ! -L "$INSTALL_DEST" ] && [ -f "$INSTALL_DEST" ] \
      || error "The direct install target ${INSTALL_DEST} is not a regular file; nothing was changed."
    BIN_PATH="$INSTALL_DEST"
  fi

  if [ -z "$BIN_PATH" ] && [ "$SYSTEM_INSTALL" = false ]; then
    classify_binary_ownership "$LEGACY_BIN"
    if [ "$BINARY_KIND" = "homebrew" ]; then
      error "Homebrew-managed install detected at ${BINARY_RESOLVED}. Run 'brew uninstall codex-switch-global-pace'; the direct uninstaller did not change Homebrew files."
    elif [ "$BINARY_KIND" = "direct" ]; then
      [ ! -L "$LEGACY_BIN" ] && [ -f "$LEGACY_BIN" ] \
        || error "The legacy direct install target ${LEGACY_BIN} is not a regular file; nothing was changed."
      BIN_PATH="$LEGACY_BIN"
    fi
  fi

  if [ -z "$BIN_PATH" ] && find_homebrew_managed_binary; then
    error "Homebrew-managed install detected at ${HOMEBREW_RESOLVED}. Run 'brew uninstall codex-switch-global-pace'; the direct uninstaller did not change Homebrew files."
  fi

  if [ -z "$BIN_PATH" ] \
    && { [ -e "$SYSTEM_INSTALL_MARKER" ] || [ -L "$SYSTEM_INSTALL_MARKER" ]; }
  then
    BIN_PATH="$LEGACY_BIN"
  fi
  if [ -z "$BIN_PATH" ] && daemon_pid_state_exists; then
    BIN_PATH="$INSTALL_DEST"
  fi
  if [ -z "$BIN_PATH" ] && [ "$SYSTEM_INSTALL" = false ] && managed_path_block_exists; then
    BIN_PATH="$INSTALL_DEST"
  fi
  if [ -z "$BIN_PATH" ] && install_transaction_residue_exists "$INSTALL_DIR"; then
    BIN_PATH="$INSTALL_DEST"
  fi
  if [ -z "$BIN_PATH" ] && legacy_transaction_residue_exists; then
    BIN_PATH="$LEGACY_BIN"
  fi
  if [ -z "$BIN_PATH" ]; then
    if [ "$SYSTEM_INSTALL" = false ]; then
      reset_managed_path_transaction
      if ! assert_no_managed_path_transaction_residue; then
        error "${PATH_TRANSACTION_ERROR}; no service, binary, or PATH configuration was changed. Inspect that fixed recovery path before retrying."
      fi
    fi
    if ! check_candidate_uninstall_owner "$INSTALL_DEST"; then
      error "Daemon service ownership preflight failed for ${INSTALL_DEST}: ${SERVICE_OWNER_ERROR}. No service, binary, or PATH configuration was changed."
    fi
    if ! read_checked_daemon_status; then
      error "Could not verify an already-uninstalled state: ${DAEMON_STATUS_ERROR}. No service, binary, or PATH configuration was changed."
    fi
    if [ "$DAEMON_STATUS_RUNNING" = false ] \
      && [ "$DAEMON_STATUS_SERVICE_INSTALLED" = false ]
    then
      info "No direct install, daemon service, PID state, marker, managed PATH block, or transaction residue was found; already uninstalled."
    else
      BIN_PATH="$INSTALL_DEST"
    fi
  fi

  if [ -n "$BIN_PATH" ]; then
    case "$BIN_PATH" in
      /*) ;;
      *) error "The uninstall target ${BIN_PATH} is not absolute; no service, binary, or PATH configuration was changed." ;;
    esac
    BIN_DIR="${BIN_PATH%/*}"
    [ -d "$BIN_DIR" ] \
      || error "Cannot acquire the shared uninstall lock because target parent ${BIN_DIR} does not exist. No directory, lock residue, service, binary, or PATH configuration was changed."
    UNINSTALL_WITH_SUDO=false
    if [ ! -w "$BIN_DIR" ]; then
      info "Removing ${BIN_PATH} requires sudo."
      sudo -v || error "Cannot uninstall ${BIN_PATH} without sudo; nothing was changed."
      UNINSTALL_WITH_SUDO=true
    fi

    INSTALL_WITH_SUDO="$UNINSTALL_WITH_SUDO"

    if ! start_update_lock "$CANDIDATE_BIN" "$BIN_PATH" 8 "$UNINSTALL_WITH_SUDO"; then
      error "The release-verified uninstall helper could not acquire the shared update lock: ${UPDATE_LOCK_ERROR}. No daemon, service, binary, or PATH configuration was changed."
    fi

    assert_no_install_transaction_residue "$BIN_DIR"

    UNINSTALL_BINARY_PRESENT=false
    classify_binary_ownership "$BIN_PATH"
    if [ "$BINARY_KIND" = "homebrew" ]; then
      error "The uninstall target changed to a Homebrew-managed binary at ${BINARY_RESOLVED}; the direct uninstaller changed nothing."
    elif [ "$BINARY_KIND" = "direct" ]; then
      [ ! -L "$BIN_PATH" ] && [ -f "$BIN_PATH" ] \
        || error "The direct uninstall target is not a regular file after locking; nothing was changed."
      [ -x "$BIN_PATH" ] \
        || error "The direct uninstall target is not executable after locking; no daemon, service, binary, or PATH configuration was changed."
      UNINSTALL_BINARY_PRESENT=true
    fi

    UNINSTALL_SYSTEM_MARKER_PRESENT=false
    UNINSTALL_SYSTEM_MARKER_TOKEN=""
    if [ "$BIN_PATH" = "$LEGACY_BIN" ] \
      && { [ -e "$SYSTEM_INSTALL_MARKER" ] || [ -L "$SYSTEM_INSTALL_MARKER" ]; }
    then
      [ ! -L "$SYSTEM_INSTALL_MARKER" ] && [ -f "$SYSTEM_INSTALL_MARKER" ] \
        || error "The system-install marker is not a regular direct-installer file after locking; nothing was changed."
      UNINSTALL_SYSTEM_MARKER_PRESENT=true
      capture_installer_file_token \
        "$SYSTEM_INSTALL_MARKER" "$UNINSTALL_WITH_SUDO" UNINSTALL_SYSTEM_MARKER_TOKEN \
        || error "The system-install marker could not be bound to this uninstall transaction; nothing was changed."
    fi

    if ! check_candidate_uninstall_owner "$BIN_PATH"; then
      release_update_locks \
        || error "Daemon service ownership preflight and update-lock release both failed: ${UPDATE_LOCK_ERROR}. No service, binary, or PATH configuration was changed."
      error "Daemon service ownership preflight failed for ${BIN_PATH}: ${SERVICE_OWNER_ERROR}. No service, binary, or PATH configuration was changed."
    fi

    reset_managed_path_transaction
    if [ "$SYSTEM_INSTALL" = false ] && ! prepare_managed_path_removals; then
      release_update_locks \
        || error "PATH preflight failed (${PATH_TRANSACTION_ERROR}), and ${UPDATE_LOCK_ERROR}. No service, binary, or PATH configuration was changed."
      error "PATH preflight failed: ${PATH_TRANSACTION_ERROR}. No service, binary, or PATH configuration was changed."
    fi

    UNINSTALL_TRANSACTION_ERROR=""
    UNINSTALL_FILE_TRANSACTION_OPEN=false
    UNINSTALL_HOLD="${BIN_DIR}/${UNINSTALL_HOLD_NAME}"
    if ! begin_uninstall_file_transaction; then
      abort_uninstall_transaction "${UNINSTALL_TRANSACTION_ERROR}."
    fi

    if ! start_daemon_update_boundary \
      "$CANDIDATE_BIN" "$BIN_PATH" "$BIN_PATH"
    then
      abort_uninstall_transaction "The daemon lifecycle boundary could not be established: ${DAEMON_BOUNDARY_ERROR}."
    fi
    UNINSTALL_DAEMON_WAS_RUNNING="$DAEMON_WAS_RUNNING"
    UNINSTALL_SERVICE_PRESENT="$DAEMON_SERVICE_INSTALLED"

    if [ "$SYSTEM_INSTALL" = false ] && ! commit_managed_path_changes; then
      abort_uninstall_transaction "PATH cleanup failed: ${PATH_TRANSACTION_ERROR}."
    fi

    if ! hold_uninstall_binary_for_commit; then
      abort_uninstall_transaction "The binary could not be moved to the fixed uninstall hold ${UNINSTALL_HOLD}."
    fi

    if ! request_daemon_update_boundary_uninstall_state; then
      if [ "${DAEMON_BOUNDARY_ROLLBACK_SAFE:-false}" = true ]; then
        abort_uninstall_transaction "Daemon service cleanup failed: ${DAEMON_BOUNDARY_ERROR}."
      fi
      local ambiguous_uninstall_error="$DAEMON_BOUNDARY_ERROR"
      if ! abandon_daemon_update_boundary; then
        ambiguous_uninstall_error="${ambiguous_uninstall_error}; lifecycle holder cleanup failed: ${DAEMON_BOUNDARY_ERROR}"
      fi
      release_update_locks \
        || error "Daemon uninstall state became ambiguous (${ambiguous_uninstall_error}), and ${UPDATE_LOCK_ERROR}. Fixed recovery files were preserved."
      error "Daemon uninstall state became ambiguous: ${ambiguous_uninstall_error}. Fixed recovery files were preserved."
    fi
    if [ "$UNINSTALL_SERVICE_PRESENT" = true ]; then
      info "Removed daemon service."
    elif [ "$UNINSTALL_DAEMON_WAS_RUNNING" = true ]; then
      info "Stopped detached daemon."
    fi

    if ! finish_daemon_update_boundary; then
      local final_confirmation_error="$DAEMON_BOUNDARY_ERROR"
      release_update_locks \
        || error "Daemon uninstall state finalization failed (${final_confirmation_error}), fixed recovery files were preserved, and ${UPDATE_LOCK_ERROR}."
      error "Daemon uninstall state finalization failed: ${final_confirmation_error}. Fixed recovery files were preserved."
    fi

    if ! commit_uninstall_file_transaction; then
      release_daemon_update_boundary \
        || UNINSTALL_TRANSACTION_ERROR="${UNINSTALL_TRANSACTION_ERROR} daemon lifecycle authority release failed: ${DAEMON_BOUNDARY_ERROR};"
      release_update_locks \
        || error "Daemon service and PATH cleanup committed, the fixed binary hold remains at ${UNINSTALL_HOLD}, ${UNINSTALL_TRANSACTION_ERROR} and ${UPDATE_LOCK_ERROR}."
      error "Daemon service and PATH cleanup committed, but the fixed binary hold remains at ${UNINSTALL_HOLD}; inspect it before retrying.${UNINSTALL_TRANSACTION_ERROR}"
    fi
    if [ "$SYSTEM_INSTALL" = false ] && ! finalize_managed_path_changes; then
      UNINSTALL_TRANSACTION_ERROR="PATH recovery cleanup failed (${PATH_TRANSACTION_ERROR})"
    fi
    if ! release_daemon_update_boundary; then
      UNINSTALL_TRANSACTION_ERROR="${UNINSTALL_TRANSACTION_ERROR:+${UNINSTALL_TRANSACTION_ERROR}; }daemon lifecycle authority release failed: ${DAEMON_BOUNDARY_ERROR}"
    fi
    if [ -n "$UNINSTALL_TRANSACTION_ERROR" ]; then
      release_update_locks \
        || error "The uninstall committed, ${UNINSTALL_TRANSACTION_ERROR}, and ${UPDATE_LOCK_ERROR}."
      error "The uninstall committed, but ${UNINSTALL_TRANSACTION_ERROR}."
    fi
    if [ "$UNINSTALL_BINARY_PRESENT" = true ]; then
      info "Removed ${BIN_PATH}"
    fi
    if ! release_update_locks; then
      error "The uninstall committed, but ${UPDATE_LOCK_ERROR}."
    fi
    # The lock inode is persistent protocol state. Deleting it after release
    # could split the lock from an installer that was already waiting on it.
    info "Kept shared update lock: ${BIN_DIR}/.${BINARY_NAME}.self-update.lock"
  fi

  # This directory is deliberately shared with codex-switch. Removing it here
  # would delete profiles and credentials still used by the other program.
  if [ -d "$DATA_DIR" ]; then
    info "Kept shared profile data: ${DATA_DIR}"
  fi

  info "codex-switch-global-pace has been uninstalled."
  exit 0
}

# ── Install ──────────────────────────────────────────────

if [ "$UNINSTALL" = true ]; then
  [ -n "$PACKAGED_RELEASE_VERSION" ] \
    || error "This uninstaller is not bound to a GitHub Release. Download install.sh from that Release before uninstalling."
  validate_version "$PACKAGED_RELEASE_VERSION"
  EXPECTED_RELEASE_VERSION="$PACKAGED_RELEASE_VERSION"
  case "$PACKAGED_RELEASE_VERSION" in
    *-dev|*-dev.*)
      USE_DEV=true
      VERSION="dev"
      ;;
    *)
      USE_DEV=false
      VERSION="$PACKAGED_RELEASE_VERSION"
      ;;
  esac
elif [ "$USE_DEV" = true ]; then
  VERSION="dev"
else
  VERSION="${CS_VERSION:-latest}"
  if [ "$VERSION" != "latest" ]; then
    validate_version "$VERSION"
  fi
fi

if [ "$UNINSTALL" = false ] && { [ "$VERSION" = "latest" ] || [ "$VERSION" = "dev" ]; }; then
  [ -n "$PACKAGED_RELEASE_VERSION" ] || error "This installer is not bound to a GitHub Release. Download install.sh from the stable or dev Release assets instead of running the repository copy directly."
  EXPECTED_RELEASE_VERSION="$PACKAGED_RELEASE_VERSION"
elif [ "$UNINSTALL" = false ]; then
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
LEGACY_NEEDS_SUDO=false
if [ "$UNINSTALL" = false ]; then
  classify_binary_ownership "$INSTALL_DEST"
  if [ "$BINARY_KIND" = "homebrew" ]; then
    error "Homebrew-managed install detected at ${BINARY_RESOLVED}. Run 'brew uninstall codex-switch-global-pace' before using the direct installer; no Homebrew files were changed."
  fi
  if [ "$BINARY_KIND" = "direct" ] && { [ -L "$INSTALL_DEST" ] || [ ! -f "$INSTALL_DEST" ]; }; then
    error "The direct install target ${INSTALL_DEST} is not a regular file; nothing was changed."
  fi
  if [ "$BINARY_KIND" = "direct" ] && [ ! -x "$INSTALL_DEST" ]; then
    error "The existing direct install ${INSTALL_DEST} is not executable; no daemon, service, or binary was changed."
  fi
  if find_homebrew_managed_binary; then
    error "Homebrew-managed install detected at ${HOMEBREW_RESOLVED}. Run 'brew uninstall codex-switch-global-pace' before using the direct installer; no Homebrew files were changed."
  fi

  if [ "$SYSTEM_INSTALL" = false ] && { [ -e "$LEGACY_BIN" ] || [ -L "$LEGACY_BIN" ]; }; then
    classify_binary_ownership "$LEGACY_BIN"
    if [ "$BINARY_KIND" = "homebrew" ]; then
      error "Homebrew-managed install detected at ${BINARY_RESOLVED}. Run 'brew uninstall codex-switch-global-pace' before using the direct installer; no Homebrew files were changed."
    fi
    if [ -L "$LEGACY_BIN" ] || [ ! -f "$LEGACY_BIN" ]; then
      error "The legacy direct install target ${LEGACY_BIN} is not a regular file; nothing was changed."
    fi
    if [ ! -x "$LEGACY_BIN" ]; then
      error "The existing legacy direct install ${LEGACY_BIN} is not executable; no daemon, service, or binary was changed."
    fi
    if [ ! -w "$SYSTEM_INSTALL_DIR" ]; then
      info "Legacy system install detected at ${BINARY_RESOLVED}; migration requires sudo once."
      LEGACY_NEEDS_SUDO=true
    else
      info "Legacy system install detected at ${BINARY_RESOLVED}; it will be migrated."
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

# Download, verify, and extract. Record both the direct directory and its
# direct parent's identity before placing any artifact inside it; EXIT cleanup
# refuses to recurse if either identity changes.
TMP_DIR="$(mktemp -d)" || error "Could not create an installer temporary directory."
case "$TMP_DIR" in
  /*) ;;
  *)
    rmdir "$TMP_DIR" 2>/dev/null \
      || error "mktemp returned a relative path and its empty directory could not be removed: ${TMP_DIR}"
    error "mktemp returned a relative path; refusing recursive cleanup: ${TMP_DIR}"
    ;;
esac
TMP_DIR_PARENT="$(dirname "$TMP_DIR")"
case "$TMP_DIR" in
  "${TMP_DIR_PARENT%/}/"*) ;;
  *) error "Installer temporary directory is not below its direct recorded root: ${TMP_DIR}" ;;
esac
[ ! -L "$TMP_DIR_PARENT" ] && [ -d "$TMP_DIR_PARENT" ] \
  || error "Installer temporary parent is not a direct directory: ${TMP_DIR_PARENT}"
TMP_DIR_PARENT_IDENTITY="$(file_identity "$TMP_DIR_PARENT")" \
  || error "Could not identify installer temporary parent ${TMP_DIR_PARENT}."
[ ! -L "$TMP_DIR" ] && [ -d "$TMP_DIR" ] \
  || error "mktemp did not create a direct temporary directory: ${TMP_DIR}"
TMP_DIR_IDENTITY="$(file_identity "$TMP_DIR")" \
  || error "Could not identify installer temporary directory ${TMP_DIR}."
TMP_CLEANUP_ERROR=""
INSTALL_STAGE="${INSTALL_DIR}/${INSTALL_STAGE_NAME}"
INSTALL_BACKUP="${INSTALL_DIR}/${INSTALL_BACKUP_NAME}"
INSTALL_STAGE_OWNED=false
INSTALL_STAGE_TOKEN=""
INSTALL_BACKUP_TOKEN=""
INSTALL_ORIGINAL_TOKEN=""
INSTALL_PUBLISHED_TOKEN=""
INSTALL_WITH_SUDO=false
UPDATE_LOCK_PID_8=""
UPDATE_LOCK_PID_9=""
UPDATE_LOCK_ERROR=""
DAEMON_BOUNDARY_PID=""
DAEMON_BOUNDARY_ACTIVE=false
DAEMON_BOUNDARY_ROLLBACK_SAFE=false
DAEMON_BOUNDARY_PHASE=""
DAEMON_BOUNDARY_ERROR=""
DAEMON_BOUNDARY_EXIT_CLEANUP_ERROR=""
LEGACY_HOLD="${SYSTEM_INSTALL_DIR}/${LEGACY_HOLD_NAME}"
LEGACY_DISPLACED="${SYSTEM_INSTALL_DIR}/${LEGACY_DISPLACED_NAME}"
LEGACY_HELD=false
LEGACY_HOLD_TOKEN=""
LEGACY_DISPLACED_TOKEN=""
LEGACY_PLACEHOLDER_TOKEN=""
LEGACY_ORIGINAL_TOKEN=""
UNINSTALL_HOLD=""
UNINSTALL_STAGE=""
reset_managed_path_transaction
trap 'cleanup_install_exit' EXIT
trap 'exit 130' INT

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

CANDIDATE_BIN="${TMP_DIR}/${BINARY_NAME}"
CANDIDATE_ERROR=""
if ! verify_candidate_version "$CANDIDATE_BIN" "$EXPECTED_RELEASE_VERSION"; then
  error "Downloaded binary failed its pre-install check; the existing installation was not changed: ${CANDIDATE_ERROR}"
fi

if [ "$UNINSTALL" = true ]; then
  run_uninstall
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

SYSTEM_MARKER_CREATED=false
SYSTEM_MARKER_CREATED_TOKEN=""
BINARY_REPLACED=false

if ! start_install_update_locks "$CANDIDATE_BIN"; then
  error "The downloaded binary cannot participate in a safe installer/self-update transaction: ${UPDATE_LOCK_ERROR}. The existing installation was not changed."
fi

assert_no_install_transaction_residue "$INSTALL_DIR"
validate_locked_direct_binary "$INSTALL_DEST" "direct install target" true
if [ "$MIGRATE_LEGACY" = true ]; then
  validate_locked_direct_binary "$LEGACY_BIN" "legacy direct install target" false
fi

MARKER_WAS_PRESENT=false
SYSTEM_MARKER_ORIGINAL_TOKEN=""
MARKER_WITH_SUDO=false
if [ "$INSTALL_WITH_SUDO" = true ] || [ "$LEGACY_NEEDS_SUDO" = true ]; then
  MARKER_WITH_SUDO=true
fi
if [ -e "$SYSTEM_INSTALL_MARKER" ] || [ -L "$SYSTEM_INSTALL_MARKER" ]; then
  [ ! -L "$SYSTEM_INSTALL_MARKER" ] && [ -f "$SYSTEM_INSTALL_MARKER" ] \
    || error "The system-install marker is not a regular direct-installer file after locking; no service, binary, or PATH configuration was changed."
  MARKER_WAS_PRESENT=true
  capture_installer_file_token \
    "$SYSTEM_INSTALL_MARKER" "$MARKER_WITH_SUDO" SYSTEM_MARKER_ORIGINAL_TOKEN \
    || error "The system-install marker could not be bound to this installer transaction; nothing was changed."
fi

if [ "$SYSTEM_INSTALL" = false ] && ! prepare_managed_path_addition; then
  PATH_PREFLIGHT_ERROR="$PATH_TRANSACTION_ERROR"
  if ! rollback_managed_path_changes; then
    PATH_PREFLIGHT_ERROR="${PATH_PREFLIGHT_ERROR}; preparation cleanup was incomplete: ${PATH_TRANSACTION_ERROR}"
  fi
  if ! cleanup_install_artifacts; then
    PATH_PREFLIGHT_ERROR="${PATH_PREFLIGHT_ERROR}; ${INSTALL_ARTIFACT_CLEANUP_ERROR}"
  fi
  release_update_locks \
    || error "PATH preflight failed (${PATH_PREFLIGHT_ERROR}), and ${UPDATE_LOCK_ERROR}."
  error "PATH preflight failed: ${PATH_PREFLIGHT_ERROR}."
fi

if ! prepare_daemon_upgrade; then
  if ! rollback_managed_path_changes; then
    DAEMON_STATUS_ERROR="${DAEMON_STATUS_ERROR} PATH preparation cleanup was incomplete: ${PATH_TRANSACTION_ERROR}."
  fi
  if ! cleanup_install_artifacts; then
    DAEMON_STATUS_ERROR="${DAEMON_STATUS_ERROR} ${INSTALL_ARTIFACT_CLEANUP_ERROR}."
  fi
  release_update_locks || error "${DAEMON_STATUS_ERROR} ${UPDATE_LOCK_ERROR}"
  error "$DAEMON_STATUS_ERROR"
fi

info "Holding the daemon lifecycle boundary while replacing ${DAEMON_PREVIOUS_BIN}..."
if ! start_daemon_update_boundary \
  "$CANDIDATE_BIN" "$DAEMON_PREVIOUS_BIN" "$INSTALL_DEST"
then
  if [ "${DAEMON_BOUNDARY_ACTIVE:-false}" = true ]; then
    abort_install_upgrade "The daemon lifecycle boundary could not be established: ${DAEMON_BOUNDARY_ERROR}."
  fi
  if ! rollback_managed_path_changes; then
    DAEMON_BOUNDARY_ERROR="${DAEMON_BOUNDARY_ERROR}; PATH preparation cleanup was incomplete: ${PATH_TRANSACTION_ERROR}"
  fi
  if ! cleanup_install_artifacts; then
    DAEMON_BOUNDARY_ERROR="${DAEMON_BOUNDARY_ERROR}; ${INSTALL_ARTIFACT_CLEANUP_ERROR}"
  fi
  release_update_locks \
    || error "The daemon lifecycle boundary failed (${DAEMON_BOUNDARY_ERROR}), and ${UPDATE_LOCK_ERROR}."
  error "The daemon lifecycle boundary failed before file replacement: ${DAEMON_BOUNDARY_ERROR}."
fi

CANDIDATE_ERROR=""
if ! stage_and_replace_binary "$CANDIDATE_BIN"; then
  abort_install_upgrade "Failed to stage an atomic binary replacement. ${CANDIDATE_ERROR}"
fi

if [ "$SYSTEM_INSTALL" = true ] && [ "$MARKER_WAS_PRESENT" = false ] \
  && ! capture_empty_installer_file \
    "$SYSTEM_INSTALL_MARKER" "$INSTALL_WITH_SUDO" SYSTEM_MARKER_CREATED_TOKEN
then
  abort_install_upgrade "System install marker creation failed: ${INSTALLER_FILE_OP_ERROR}."
elif [ "$SYSTEM_INSTALL" = true ] && [ "$MARKER_WAS_PRESENT" = false ]; then
  SYSTEM_MARKER_CREATED=true
fi

if ! verify_candidate_version "${INSTALL_DIR}/${BINARY_NAME}" "$EXPECTED_RELEASE_VERSION"; then
  abort_install_upgrade "Installed binary verification failed: ${CANDIDATE_ERROR}"
fi

if ! hold_legacy_install_for_commit; then
  abort_install_upgrade "The legacy direct install could not be staged for removal."
fi

if [ "$SYSTEM_INSTALL" = false ] && ! commit_managed_path_changes; then
  abort_install_upgrade "PATH update failed: ${PATH_TRANSACTION_ERROR}."
fi

if [ "$DAEMON_STATE_CAPTURED" = true ] \
  && ! request_daemon_update_boundary_new_state
then
  abort_install_upgrade "The replacement daemon state could not be established: ${DAEMON_BOUNDARY_ERROR}."
fi

# The replacement daemon now owns the PID identity (or the initially stopped
# daemon remains protected by its absence lease) while the service-operation
# lease is still held. Confirm that final state before removing any exact
# recovery material. A failed final confirmation preserves every fixed recovery
# path for inspection instead of partially committing cleanup.
POST_COMMIT_ERRORS=""
if [ "$DAEMON_STATE_CAPTURED" = true ] && ! finish_daemon_update_boundary; then
  POST_COMMIT_ERRORS="final daemon state confirmation failed: ${DAEMON_BOUNDARY_ERROR}"
  preserve_install_backup
else
  if ! commit_installed_binary; then
    POST_COMMIT_ERRORS="executable rollback-backup cleanup failed"
    preserve_install_backup
  fi
  BINARY_REPLACED=false

  if ! commit_held_legacy_install; then
    POST_COMMIT_ERRORS="${POST_COMMIT_ERRORS}${POST_COMMIT_ERRORS:+; }held legacy-install cleanup failed"
  fi

  if [ "$SYSTEM_INSTALL" = false ] && ! finalize_managed_path_changes; then
    POST_COMMIT_ERRORS="${POST_COMMIT_ERRORS}${POST_COMMIT_ERRORS:+; }PATH recovery cleanup failed: ${PATH_TRANSACTION_ERROR}"
  fi

  if ! cleanup_install_artifacts; then
    POST_COMMIT_ERRORS="${POST_COMMIT_ERRORS}${POST_COMMIT_ERRORS:+; }${INSTALL_ARTIFACT_CLEANUP_ERROR}"
  fi

  if ! release_daemon_update_boundary; then
    POST_COMMIT_ERRORS="${POST_COMMIT_ERRORS}${POST_COMMIT_ERRORS:+; }daemon lifecycle authority release failed: ${DAEMON_BOUNDARY_ERROR}"
  fi
fi

if [ "$MIGRATE_LEGACY" = true ]; then
  info "Removed legacy install: ${LEGACY_BIN}"
fi

if [ "$SYSTEM_INSTALL" = false ] \
  && [ "$PATH_TRANSACTION_PROFILE_SELECTED" = false ] \
  && [[ ":${PATH}:" != *":${USER_INSTALL_DIR}:"* ]]
then
  warn "Add ${USER_INSTALL_DIR} to your PATH to run codex-switch-global-pace by name."
fi

if ! release_update_locks; then
  POST_COMMIT_ERRORS="${POST_COMMIT_ERRORS}${POST_COMMIT_ERRORS:+; }${UPDATE_LOCK_ERROR}"
fi

if [ -n "$POST_COMMIT_ERRORS" ]; then
  error "The new install was committed, but cleanup was incomplete: ${POST_COMMIT_ERRORS}."
fi

info "Installed: $(${INSTALL_DIR}/${BINARY_NAME} --version 8>&- 9>&-)"
info "Run 'codex-switch-global-pace --help' to get started"
