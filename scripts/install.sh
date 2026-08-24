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
INSTALL_STAGE_NAME=".${BINARY_NAME}.install"
INSTALL_BACKUP_NAME=".${BINARY_NAME}.rollback"
UNINSTALL_HOLD_NAME=".${BINARY_NAME}.uninstall"
LEGACY_HOLD_NAME=".${BINARY_NAME}.legacy"

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
    "${directory}/${UNINSTALL_HOLD_NAME}" \
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

stop_and_confirm_daemon_absent() {
  local binary="$1"
  if ! "$binary" daemon stop 8>&- 9>&-; then
    DAEMON_STATUS_ERROR="daemon stop failed"
    return 1
  fi
  if ! read_checked_daemon_status; then
    return 1
  fi
  if [ "$DAEMON_STATUS_RUNNING" = true ]; then
    DAEMON_STATUS_ERROR="daemon still reports running after the stop boundary"
    return 1
  fi
}

confirm_daemon_running() {
  if ! read_checked_daemon_status; then
    return 1
  fi
  if [ "$DAEMON_STATUS_RUNNING" != true ]; then
    DAEMON_STATUS_ERROR="daemon did not report running after restart"
    return 1
  fi
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

run_install_fs() {
  if [ "${INSTALL_WITH_SUDO:-false}" = true ]; then
    sudo "$@"
  else
    "$@"
  fi
}

run_legacy_fs() {
  if [ "${LEGACY_NEEDS_SUDO:-false}" = true ]; then
    sudo "$@"
  else
    "$@"
  fi
}

cleanup_install_artifacts() {
  if [ "${INSTALL_STAGE_OWNED:-false}" = true ] \
    && [ -n "${INSTALL_STAGE:-}" ] \
    && { [ -e "$INSTALL_STAGE" ] || [ -L "$INSTALL_STAGE" ]; }
  then
    if ! run_install_fs rm -f "$INSTALL_STAGE" >/dev/null 2>&1; then
      warn "Staged installer candidate remains at ${INSTALL_STAGE}; the fixed residue will block later transactions until it is inspected."
      return
    fi
  fi
  INSTALL_STAGE_OWNED=false
}

rollback_installed_binary() {
  if [ "${INSTALL_DEST_EXISTED:-false}" = true ]; then
    [ ! -L "$INSTALL_BACKUP" ] && [ -f "$INSTALL_BACKUP" ] || return 1
    run_install_fs mv -f "$INSTALL_BACKUP" "$INSTALL_DEST" || return 1
  else
    run_install_fs rm -f "$INSTALL_DEST" || return 1
  fi
  return 0
}

stage_and_replace_binary() {
  local candidate="$1"
  INSTALL_DEST="${INSTALL_DIR}/${BINARY_NAME}"
  INSTALL_DEST_EXISTED=false

  if [ -L "$INSTALL_DEST" ]; then
    CANDIDATE_ERROR="refusing to replace symbolic-link install target ${INSTALL_DEST}"
    return 1
  fi
  INSTALL_STAGE_OWNED=true
  run_install_fs install -m 0755 "$candidate" "$INSTALL_STAGE" || return 1

  if [ -e "$INSTALL_DEST" ]; then
    INSTALL_DEST_EXISTED=true
    run_install_fs cp -p "$INSTALL_DEST" "$INSTALL_BACKUP" || return 1
  fi

  run_install_fs mv -f "$INSTALL_STAGE" "$INSTALL_DEST" || return 1
  INSTALL_STAGE_OWNED=false
  return 0
}

commit_installed_binary() {
  if [ "$INSTALL_DEST_EXISTED" = true ]; then
    [ ! -L "$INSTALL_BACKUP" ] && [ -f "$INSTALL_BACKUP" ] || return 1
    run_install_fs rm -f "$INSTALL_BACKUP" || return 1
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
  [ "$MIGRATE_LEGACY" = true ] || return 0
  [ ! -e "$LEGACY_HOLD" ] && [ ! -L "$LEGACY_HOLD" ] || return 1
  run_legacy_fs mv "$LEGACY_BIN" "$LEGACY_HOLD" || return 1
  LEGACY_HELD=true
}

restore_held_legacy_install() {
  [ "${LEGACY_HELD:-false}" = true ] || return 0
  run_legacy_fs mv "$LEGACY_HOLD" "$LEGACY_BIN" || return 1
  LEGACY_HELD=false
}

commit_held_legacy_install() {
  [ "${LEGACY_HELD:-false}" = true ] || return 0
  if [ -e "$SYSTEM_INSTALL_MARKER" ]; then
    run_legacy_fs rm -f "$SYSTEM_INSTALL_MARKER" || return 1
  fi
  run_legacy_fs rm -f "$LEGACY_HOLD" || return 1
  LEGACY_HELD=false
}

cleanup_update_locks_on_exit() {
  exec 9>&- 8>&-
  if [ -n "${UPDATE_LOCK_PID_9:-}" ]; then
    wait "$UPDATE_LOCK_PID_9" >/dev/null 2>&1 || true
    UPDATE_LOCK_PID_9=""
  fi
  if [ -n "${UPDATE_LOCK_PID_8:-}" ]; then
    wait "$UPDATE_LOCK_PID_8" >/dev/null 2>&1 || true
    UPDATE_LOCK_PID_8=""
  fi
}

cleanup_install_exit() {
  cleanup_update_locks_on_exit
  cleanup_install_artifacts
  if [ -n "${TMP_DIR:-}" ]; then
    rm -rf "$TMP_DIR"
  fi
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
  DAEMON_SERVICE_REWRITE=false
  DAEMON_RESTART_ATTEMPTED=false
  DAEMON_SERVICE_REWRITE_ATTEMPTED=false
  DAEMON_STATUS_ERROR=""

  if [ -x "$INSTALL_DEST" ] && [ ! -L "$INSTALL_DEST" ]; then
    DAEMON_PREVIOUS_BIN="$INSTALL_DEST"
  elif [ "$MIGRATE_LEGACY" = true ] && [ -x "$LEGACY_BIN" ] && [ ! -L "$LEGACY_BIN" ]; then
    DAEMON_PREVIOUS_BIN="$LEGACY_BIN"
  else
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
        DAEMON_SERVICE_REWRITE=true
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

  if [ "$DAEMON_WAS_RUNNING" = true ] || [ "$DAEMON_SERVICE_INSTALLED" = true ]; then
    info "Stopping the existing daemon before replacing ${DAEMON_PREVIOUS_BIN}..."
    if ! stop_and_confirm_daemon_absent "$DAEMON_PREVIOUS_BIN"; then
      DAEMON_STATUS_ERROR="The existing daemon could not be stopped safely: ${DAEMON_STATUS_ERROR}"
      return 1
    fi
  fi
}

ensure_previous_daemon_running() {
  [ "$DAEMON_WAS_RUNNING" = true ] || return 0
  if read_checked_daemon_status && [ "$DAEMON_STATUS_RUNNING" = true ]; then
    return 0
  fi
  if ! "$DAEMON_PREVIOUS_BIN" daemon start 8>&- 9>&-; then
    DAEMON_STATUS_ERROR="could not restart the previous daemon"
    return 1
  fi
  confirm_daemon_running
}

stop_restarted_daemon_for_rollback() {
  [ "$DAEMON_RESTART_ATTEMPTED" = true ] || return 0
  if ! read_checked_daemon_status; then
    return 1
  fi
  if [ "$DAEMON_STATUS_RUNNING" = true ]; then
    stop_and_confirm_daemon_absent "$INSTALL_DEST"
  fi
}

abort_install_upgrade() {
  local reason="$1" rollback_errors="" service_restored=false

  if ! stop_restarted_daemon_for_rollback; then
    rollback_errors="${rollback_errors} could not prove the replacement daemon was stopped: ${DAEMON_STATUS_ERROR};"
  fi

  if [ -z "$rollback_errors" ] && ! restore_held_legacy_install; then
    rollback_errors="${rollback_errors} could not restore the legacy executable;"
  fi

  if [ -z "$rollback_errors" ] && [ "$DAEMON_SERVICE_REWRITE_ATTEMPTED" = true ]; then
    if check_candidate_uninstall_owner "$DAEMON_PREVIOUS_BIN" \
      && confirm_daemon_running \
      && [ "$DAEMON_STATUS_SERVICE_INSTALLED" = true ]
    then
      service_restored=true
    elif check_candidate_uninstall_owner "$INSTALL_DEST" \
      && "$DAEMON_PREVIOUS_BIN" daemon install \
        --expected-existing-executable "$INSTALL_DEST" 8>&- 9>&- \
      && check_candidate_uninstall_owner "$DAEMON_PREVIOUS_BIN" \
      && confirm_daemon_running \
      && [ "$DAEMON_STATUS_SERVICE_INSTALLED" = true ]
    then
      service_restored=true
    else
      rollback_errors="${rollback_errors} could not restore and verify the previous daemon service: ${DAEMON_STATUS_ERROR};"
    fi
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
      if run_install_fs rm -f "$SYSTEM_INSTALL_MARKER"; then
        SYSTEM_MARKER_CREATED=false
      else
        rollback_errors="${rollback_errors} could not remove the new system-install marker;"
      fi
    else
      rollback_errors="${rollback_errors} the new system-install marker was preserved because the replacement system binary remains installed;"
    fi
  fi

  if [ -z "$rollback_errors" ] && [ "$service_restored" = false ]; then
    if ! ensure_previous_daemon_running; then
      rollback_errors="${rollback_errors} ${DAEMON_STATUS_ERROR};"
    fi
  fi

  if [ -n "$rollback_errors" ]; then
    preserve_install_backup
  fi
  cleanup_install_artifacts
  if ! release_update_locks; then
    rollback_errors="${rollback_errors} ${UPDATE_LOCK_ERROR};"
  fi
  if [ -n "$rollback_errors" ]; then
    error "${reason} Rollback was incomplete:${rollback_errors} No unverified executable was removed."
  fi
  error "${reason} The previous executable and daemon state were restored."
}

restart_daemon_after_upgrade() {
  if [ "$DAEMON_SERVICE_REWRITE" = true ]; then
    DAEMON_RESTART_ATTEMPTED=true
    DAEMON_SERVICE_REWRITE_ATTEMPTED=true
    info "Moving the running daemon service to ${INSTALL_DEST}..."
    if ! "$INSTALL_DEST" daemon install \
      --expected-existing-executable "$LEGACY_BIN" 8>&- 9>&-; then
      DAEMON_STATUS_ERROR="new daemon service installation failed"
      return 1
    fi
    if ! check_candidate_uninstall_owner "$INSTALL_DEST"; then
      DAEMON_STATUS_ERROR="new daemon service is not exactly owned by ${INSTALL_DEST}: ${SERVICE_OWNER_ERROR}"
      return 1
    fi
    if ! confirm_daemon_running; then
      return 1
    fi
    if [ "$DAEMON_STATUS_SERVICE_INSTALLED" != true ]; then
      DAEMON_STATUS_ERROR="new daemon status did not report the service as installed"
      return 1
    fi
    return 0
  fi

  if [ "$DAEMON_WAS_RUNNING" = true ]; then
    DAEMON_RESTART_ATTEMPTED=true
    info "Restarting the previously running daemon with ${INSTALL_DEST}..."
    if ! "$INSTALL_DEST" daemon start 8>&- 9>&-; then
      DAEMON_STATUS_ERROR="daemon restart failed"
      return 1
    fi
    if ! confirm_daemon_running; then
      return 1
    fi
  elif [ "$DAEMON_STATE_CAPTURED" = true ]; then
    if ! read_checked_daemon_status; then
      return 1
    fi
    if [ "$DAEMON_STATUS_RUNNING" = true ]; then
      DAEMON_STATUS_ERROR="an existing stopped daemon unexpectedly started during upgrade"
      return 1
    fi
  fi

  if [ "$DAEMON_STATE_CAPTURED" = true ] \
    && [ "$DAEMON_STATUS_SERVICE_INSTALLED" != "$DAEMON_SERVICE_INSTALLED" ]
  then
    DAEMON_STATUS_ERROR="daemon service-installed state changed during upgrade"
    return 1
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
  PATH_TRANSACTION_ORIGINAL=()
  PATH_TRANSACTION_UPDATED=()
  PATH_TRANSACTION_STAGE=()
  PATH_TRANSACTION_COMMITTED_IDENTITY=()
  PATH_TRANSACTION_COUNT=0
  PATH_TRANSACTION_COMMITTED=0
  PATH_TRANSACTION_ERROR=""
}

assert_no_profile_transaction_residue() {
  local profile_file="$1" profile_target profile_stage
  if [ ! -e "$profile_file" ] && [ ! -L "$profile_file" ]; then
    profile_stage="${profile_file}.${BINARY_NAME}.install"
    if [ -e "$profile_stage" ] || [ -L "$profile_stage" ]; then
      PATH_TRANSACTION_ERROR="an incomplete PATH transaction remains at ${profile_stage}"
      return 1
    fi
    return 0
  fi
  if ! profile_target="$(resolve_path_target "$profile_file")"; then
    PATH_TRANSACTION_ERROR="failed to resolve ${profile_file} while checking transaction residue"
    return 1
  fi
  profile_stage="${profile_target}.${BINARY_NAME}.install"
  if [ -e "$profile_stage" ] || [ -L "$profile_stage" ]; then
    PATH_TRANSACTION_ERROR="an incomplete PATH transaction remains at ${profile_stage}"
    return 1
  fi
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
  local profile_file="$1" profile_target profile_identity original updated profile_stage
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

  if ! profile_identity="$(file_identity "$profile_target")"; then
    PATH_TRANSACTION_ERROR="failed to identify ${profile_file}"
    return 1
  fi
  index="$PATH_TRANSACTION_COUNT"
  original="${TMP_DIR}/path-${index}.original"
  updated="${TMP_DIR}/path-${index}.updated"
  profile_stage="${profile_target}.${BINARY_NAME}.install"
  if [ -e "$profile_stage" ] || [ -L "$profile_stage" ]; then
    PATH_TRANSACTION_ERROR="an incomplete PATH transaction remains at ${profile_stage}"
    return 1
  fi
  if ! cp -p "$profile_target" "$original" \
    || ! cp -p "$profile_target" "$updated"
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
  PATH_TRANSACTION_ORIGINAL[$index]="$original"
  PATH_TRANSACTION_UPDATED[$index]="$updated"
  PATH_TRANSACTION_STAGE[$index]="$profile_stage"
  PATH_TRANSACTION_COMMITTED_IDENTITY[$index]=""
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

commit_managed_path_removals() {
  local index logical target expected_identity current_target current_identity stage committed_identity
  index=0
  while [ "$index" -lt "$PATH_TRANSACTION_COUNT" ]; do
    logical="${PATH_TRANSACTION_LOGICAL[$index]}"
    target="${PATH_TRANSACTION_TARGET[$index]}"
    expected_identity="${PATH_TRANSACTION_IDENTITY[$index]}"
    stage="${PATH_TRANSACTION_STAGE[$index]}"
    if [ -e "$stage" ] || [ -L "$stage" ]; then
      PATH_TRANSACTION_ERROR="transaction stage ${stage} appeared before ${logical} could be committed"
      return 1
    fi
    if ! cp -p "${PATH_TRANSACTION_UPDATED[$index]}" "$stage"; then
      PATH_TRANSACTION_ERROR="failed to create the fixed PATH transaction stage ${stage}"
      return 1
    fi
    [ ! -L "$stage" ] && [ -f "$stage" ] || {
      PATH_TRANSACTION_ERROR="PATH transaction stage ${stage} is not a regular file"
      return 1
    }
    if ! current_target="$(resolve_path_target "$logical")"; then
      PATH_TRANSACTION_ERROR="failed to re-resolve ${logical} before commit"
      return 1
    fi
    if [ "$current_target" != "$target" ]; then
      PATH_TRANSACTION_ERROR="profile link changed while updating ${logical}"
      return 1
    fi
    if ! current_identity="$(file_identity "$current_target")"; then
      PATH_TRANSACTION_ERROR="failed to re-identify ${logical} before commit"
      return 1
    fi
    if [ "$current_identity" != "$expected_identity" ]; then
      PATH_TRANSACTION_ERROR="profile file changed while updating ${logical}; newer contents were left unchanged"
      return 1
    fi
    if ! mv -f "$stage" "$target"; then
      PATH_TRANSACTION_ERROR="failed to atomically replace ${logical}"
      return 1
    fi
    PATH_TRANSACTION_COMMITTED=$((index + 1))
    if ! committed_identity="$(file_identity "$target")"; then
      PATH_TRANSACTION_ERROR="failed to identify committed profile ${logical}"
      return 1
    fi
    PATH_TRANSACTION_COMMITTED_IDENTITY[$index]="$committed_identity"
    info "Removed codex-switch-global-pace PATH entry from ${logical}."
    index=$((index + 1))
  done
}

rollback_managed_path_removals() {
  local index logical target expected_identity current_target current_identity stage failed=false
  index="$PATH_TRANSACTION_COMMITTED"
  while [ "$index" -gt 0 ]; do
    index=$((index - 1))
    logical="${PATH_TRANSACTION_LOGICAL[$index]}"
    target="${PATH_TRANSACTION_TARGET[$index]}"
    expected_identity="${PATH_TRANSACTION_COMMITTED_IDENTITY[$index]}"
    stage="${PATH_TRANSACTION_STAGE[$index]}"
    if [ -z "$expected_identity" ] \
      || ! current_target="$(resolve_path_target "$logical")" \
      || [ "$current_target" != "$target" ] \
      || ! current_identity="$(file_identity "$current_target")" \
      || [ "$current_identity" != "$expected_identity" ] \
      || [ -e "$stage" ] || [ -L "$stage" ] \
      || ! cp -p "${PATH_TRANSACTION_ORIGINAL[$index]}" "$stage" \
      || [ -L "$stage" ] || [ ! -f "$stage" ] \
      || ! mv -f "$stage" "$target"
    then
      failed=true
      PATH_TRANSACTION_ERROR="could not safely restore ${logical}; inspect the fixed transaction paths before retrying"
    else
      PATH_TRANSACTION_COMMITTED="$index"
    fi
  done
  index=0
  while [ "$index" -lt "$PATH_TRANSACTION_COUNT" ]; do
    stage="${PATH_TRANSACTION_STAGE[$index]}"
    if [ -e "$stage" ] || [ -L "$stage" ]; then
      failed=true
      PATH_TRANSACTION_ERROR="PATH transaction residue remains at ${stage}"
    fi
    index=$((index + 1))
  done
  [ "$failed" = false ]
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

capture_uninstall_daemon_state() {
  UNINSTALL_DAEMON_WAS_RUNNING=false
  UNINSTALL_SERVICE_PRESENT=false
  UNINSTALL_RESTART_REQUIRED=false
  if ! read_checked_daemon_status; then
    UNINSTALL_TRANSACTION_ERROR="could not capture daemon state with the release-verified candidate: ${DAEMON_STATUS_ERROR}"
    return 1
  fi
  UNINSTALL_DAEMON_WAS_RUNNING="$DAEMON_STATUS_RUNNING"
  UNINSTALL_SERVICE_PRESENT="$DAEMON_STATUS_SERVICE_INSTALLED"

  if [ "$UNINSTALL_BINARY_PRESENT" = true ] \
    && [ "$UNINSTALL_DAEMON_WAS_RUNNING" = true ]
  then
    UNINSTALL_RESTART_REQUIRED=true
    if ! stop_and_confirm_daemon_absent "$BIN_PATH"; then
      UNINSTALL_TRANSACTION_ERROR="the locked installed daemon could not be stopped safely: ${DAEMON_STATUS_ERROR}"
      return 1
    fi
    if [ "$DAEMON_STATUS_SERVICE_INSTALLED" != "$UNINSTALL_SERVICE_PRESENT" ]; then
      UNINSTALL_TRANSACTION_ERROR="daemon service-installed state changed while stopping the locked installed daemon"
      return 1
    fi
  fi
}

restore_uninstall_daemon_if_needed() {
  [ "${UNINSTALL_RESTART_REQUIRED:-false}" = true ] || return 0
  [ ! -L "$BIN_PATH" ] && [ -x "$BIN_PATH" ] || return 1
  if ! read_checked_daemon_status; then
    return 1
  fi
  if [ "$DAEMON_STATUS_SERVICE_INSTALLED" != "$UNINSTALL_SERVICE_PRESENT" ]; then
    DAEMON_STATUS_ERROR="daemon service-installed state changed during uninstall rollback"
    return 1
  fi
  if [ "$DAEMON_STATUS_RUNNING" = true ]; then
    UNINSTALL_RESTART_REQUIRED=false
    return 0
  fi
  if ! "$BIN_PATH" daemon start 8>&- 9>&-; then
    DAEMON_STATUS_ERROR="daemon restart failed during uninstall rollback"
    return 1
  fi
  if ! read_checked_daemon_status; then
    return 1
  fi
  if [ "$DAEMON_STATUS_RUNNING" != true ]; then
    DAEMON_STATUS_ERROR="daemon did not report running after uninstall rollback"
    return 1
  fi
  if [ "$DAEMON_STATUS_SERVICE_INSTALLED" != "$UNINSTALL_SERVICE_PRESENT" ]; then
    DAEMON_STATUS_ERROR="daemon service-installed state was not restored after restart"
    return 1
  fi
  UNINSTALL_RESTART_REQUIRED=false
}

begin_uninstall_file_transaction() {
  UNINSTALL_HOLD="${BIN_DIR}/${UNINSTALL_HOLD_NAME}"
  [ ! -e "$UNINSTALL_HOLD" ] && [ ! -L "$UNINSTALL_HOLD" ] || {
    UNINSTALL_TRANSACTION_ERROR="uninstall transaction residue already exists at ${UNINSTALL_HOLD}"
    return 1
  }
  UNINSTALL_FILE_TRANSACTION_OPEN=true
  UNINSTALL_BINARY_HELD=false
  if [ "$UNINSTALL_BINARY_PRESENT" = true ]; then
    if ! run_install_fs cp -p "$BIN_PATH" "$UNINSTALL_HOLD"; then
      UNINSTALL_TRANSACTION_ERROR="failed to create the fixed uninstall hold ${UNINSTALL_HOLD}"
      return 1
    fi
  elif ! run_install_fs install -m 0600 /dev/null "$UNINSTALL_HOLD"; then
    UNINSTALL_TRANSACTION_ERROR="failed to create the fixed uninstall boundary ${UNINSTALL_HOLD}"
    return 1
  fi
  if [ -L "$UNINSTALL_HOLD" ] || [ ! -f "$UNINSTALL_HOLD" ]; then
    UNINSTALL_TRANSACTION_ERROR="fixed uninstall hold ${UNINSTALL_HOLD} is not a regular file"
    return 1
  fi
}

hold_uninstall_binary_for_commit() {
  [ "${UNINSTALL_FILE_TRANSACTION_OPEN:-false}" = true ] || return 1
  [ ! -L "$UNINSTALL_HOLD" ] && [ -f "$UNINSTALL_HOLD" ] || return 1
  if [ "$UNINSTALL_BINARY_PRESENT" = true ]; then
    UNINSTALL_BINARY_HELD=true
    run_install_fs mv -f "$BIN_PATH" "$UNINSTALL_HOLD" || return 1
  fi
}

rollback_uninstall_file_transaction() {
  [ "${UNINSTALL_FILE_TRANSACTION_OPEN:-false}" = true ] || return 0
  if [ "$UNINSTALL_BINARY_PRESENT" = true ]; then
    if [ "${UNINSTALL_BINARY_HELD:-false}" = true ]; then
      if [ ! -e "$BIN_PATH" ] && [ ! -L "$BIN_PATH" ]; then
        [ ! -L "$UNINSTALL_HOLD" ] && [ -f "$UNINSTALL_HOLD" ] \
          && run_install_fs mv "$UNINSTALL_HOLD" "$BIN_PATH" \
          || return 1
      else
        [ ! -L "$BIN_PATH" ] && [ -f "$BIN_PATH" ] \
          && [ ! -L "$UNINSTALL_HOLD" ] && [ -f "$UNINSTALL_HOLD" ] \
          && run_install_fs rm -f "$UNINSTALL_HOLD" \
          || return 1
      fi
    else
      [ ! -L "$BIN_PATH" ] && [ -f "$BIN_PATH" ] \
        && [ ! -L "$UNINSTALL_HOLD" ] && [ -f "$UNINSTALL_HOLD" ] \
        && run_install_fs rm -f "$UNINSTALL_HOLD" \
        || return 1
    fi
  elif [ -e "$UNINSTALL_HOLD" ] || [ -L "$UNINSTALL_HOLD" ]; then
    [ ! -L "$UNINSTALL_HOLD" ] && [ -f "$UNINSTALL_HOLD" ] \
      && run_install_fs rm -f "$UNINSTALL_HOLD" \
      || return 1
  fi
  UNINSTALL_BINARY_HELD=false
  UNINSTALL_FILE_TRANSACTION_OPEN=false
}

commit_uninstall_file_transaction() {
  [ "${UNINSTALL_FILE_TRANSACTION_OPEN:-false}" = true ] || return 1
  [ ! -e "$BIN_PATH" ] && [ ! -L "$BIN_PATH" ] || return 1
  [ ! -L "$UNINSTALL_HOLD" ] && [ -f "$UNINSTALL_HOLD" ] || return 1
  if [ "$UNINSTALL_SYSTEM_MARKER_PRESENT" = true ]; then
    run_install_fs rm -f "$SYSTEM_INSTALL_MARKER" || return 1
  fi
  run_install_fs rm -f "$UNINSTALL_HOLD" || return 1
  UNINSTALL_FILE_TRANSACTION_OPEN=false
}

abort_uninstall_transaction() {
  local reason="$1" rollback_errors=""
  if ! rollback_managed_path_removals; then
    rollback_errors="${rollback_errors} ${PATH_TRANSACTION_ERROR};"
  fi
  if ! rollback_uninstall_file_transaction; then
    rollback_errors="${rollback_errors} could not safely restore ${BIN_PATH} from ${UNINSTALL_HOLD};"
  fi
  if ! restore_uninstall_daemon_if_needed; then
    rollback_errors="${rollback_errors} could not restore the previously running daemon with ${BIN_PATH}: ${DAEMON_STATUS_ERROR};"
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
    if [ "$BIN_PATH" = "$LEGACY_BIN" ] \
      && { [ -e "$SYSTEM_INSTALL_MARKER" ] || [ -L "$SYSTEM_INSTALL_MARKER" ]; }
    then
      [ ! -L "$SYSTEM_INSTALL_MARKER" ] && [ -f "$SYSTEM_INSTALL_MARKER" ] \
        || error "The system-install marker is not a regular direct-installer file after locking; nothing was changed."
      UNINSTALL_SYSTEM_MARKER_PRESENT=true
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
    if ! capture_uninstall_daemon_state; then
      abort_uninstall_transaction "${UNINSTALL_TRANSACTION_ERROR}."
    fi
    if ! begin_uninstall_file_transaction; then
      abort_uninstall_transaction "${UNINSTALL_TRANSACTION_ERROR}."
    fi

    if [ "$SYSTEM_INSTALL" = false ] && ! commit_managed_path_removals; then
      abort_uninstall_transaction "PATH cleanup failed: ${PATH_TRANSACTION_ERROR}."
    fi

    if ! hold_uninstall_binary_for_commit; then
      abort_uninstall_transaction "The binary could not be moved to the fixed uninstall hold ${UNINSTALL_HOLD}."
    fi

    if [ "$UNINSTALL_BINARY_PRESENT" = true ] \
      || [ "$UNINSTALL_SERVICE_PRESENT" = true ]
    then
      if ! "$CANDIDATE_BIN" daemon uninstall \
        --expected-executable "$BIN_PATH" 8>&- 9>&-
      then
        abort_uninstall_transaction "Daemon service cleanup failed."
      fi
      info "Removed daemon service."
    elif [ "$UNINSTALL_DAEMON_WAS_RUNNING" = true ]; then
      if ! stop_and_confirm_daemon_absent "$CANDIDATE_BIN"; then
        abort_uninstall_transaction "Detached daemon cleanup failed: ${DAEMON_STATUS_ERROR}."
      fi
      if [ "$DAEMON_STATUS_SERVICE_INSTALLED" != false ]; then
        abort_uninstall_transaction "A daemon service appeared during detached-daemon cleanup."
      fi
      info "Stopped detached daemon."
    fi

    if ! commit_uninstall_file_transaction; then
      release_update_locks \
        || error "Daemon service and PATH cleanup committed, the fixed binary hold remains at ${UNINSTALL_HOLD}, and ${UPDATE_LOCK_ERROR}."
      error "Daemon service and PATH cleanup committed, but the fixed binary hold remains at ${UNINSTALL_HOLD}; inspect it before retrying."
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

# Download, verify, and extract
TMP_DIR="$(mktemp -d)"
INSTALL_STAGE="${INSTALL_DIR}/${INSTALL_STAGE_NAME}"
INSTALL_BACKUP="${INSTALL_DIR}/${INSTALL_BACKUP_NAME}"
INSTALL_STAGE_OWNED=false
INSTALL_WITH_SUDO=false
UPDATE_LOCK_PID_8=""
UPDATE_LOCK_PID_9=""
UPDATE_LOCK_ERROR=""
LEGACY_HOLD="${SYSTEM_INSTALL_DIR}/${LEGACY_HOLD_NAME}"
LEGACY_HELD=false
UNINSTALL_HOLD=""
trap 'cleanup_install_exit' EXIT

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
if [ -e "$SYSTEM_INSTALL_MARKER" ] || [ -L "$SYSTEM_INSTALL_MARKER" ]; then
  [ ! -L "$SYSTEM_INSTALL_MARKER" ] && [ -f "$SYSTEM_INSTALL_MARKER" ] \
    || error "The system-install marker is not a regular direct-installer file after locking; no service, binary, or PATH configuration was changed."
  MARKER_WAS_PRESENT=true
fi

if ! prepare_daemon_upgrade; then
  if [ "$DAEMON_STATE_CAPTURED" = true ]; then
    abort_install_upgrade "$DAEMON_STATUS_ERROR"
  fi
  cleanup_install_artifacts
  release_update_locks || error "${DAEMON_STATUS_ERROR} ${UPDATE_LOCK_ERROR}"
  error "$DAEMON_STATUS_ERROR"
fi

CANDIDATE_ERROR=""
if ! stage_and_replace_binary "$CANDIDATE_BIN"; then
  abort_install_upgrade "Failed to stage an atomic binary replacement. ${CANDIDATE_ERROR}"
fi
BINARY_REPLACED=true

if [ "$SYSTEM_INSTALL" = true ] && ! run_install_fs install -m 0644 /dev/null "$SYSTEM_INSTALL_MARKER"; then
  abort_install_upgrade "System install marker creation failed."
elif [ "$SYSTEM_INSTALL" = true ] && [ "$MARKER_WAS_PRESENT" = false ]; then
  SYSTEM_MARKER_CREATED=true
fi

if ! verify_candidate_version "${INSTALL_DIR}/${BINARY_NAME}" "$EXPECTED_RELEASE_VERSION"; then
  abort_install_upgrade "Installed binary verification failed: ${CANDIDATE_ERROR}"
fi

if ! restart_daemon_after_upgrade; then
  abort_install_upgrade "The new executable could not restore the previous daemon state: ${DAEMON_STATUS_ERROR}."
fi

if ! hold_legacy_install_for_commit; then
  abort_install_upgrade "The legacy direct install could not be staged for removal."
fi

if ! commit_installed_binary; then
  abort_install_upgrade "The executable transaction could not remove its rollback backup."
fi
BINARY_REPLACED=false

if ! commit_held_legacy_install; then
  cleanup_install_artifacts
  release_update_locks || error "The new install was committed, but legacy cleanup and update-lock release both failed: ${UPDATE_LOCK_ERROR}"
  error "The new executable and daemon were committed, but the held legacy install could not be removed."
fi

cleanup_install_artifacts

if [ "$MIGRATE_LEGACY" = true ]; then
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

if ! release_update_locks; then
  error "$UPDATE_LOCK_ERROR"
fi

info "Installed: $(${INSTALL_DIR}/${BINARY_NAME} --version 8>&- 9>&-)"
info "Run 'codex-switch-global-pace --help' to get started"
