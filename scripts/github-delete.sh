#!/usr/bin/env bash
set -euo pipefail

# Callers verify the complete resource state before invoking this helper. The
# mutation is issued once. Only an exact, still-visible identity is safe to
# observe again while confirming the requested deletion.
readonly -a DELETION_CONFIRMATION_DELAYS=(0 0.5 1 2 4)

usage() {
  cat >&2 <<'EOF'
Usage:
  github-delete.sh tag-ref <tag> <expected-object-type> <expected-sha>
  github-delete.sh release <positive-release-id>
EOF
  exit 2
}

if [[ ! "${GITHUB_REPOSITORY:-}" =~ ^[^/[:space:]]+/[^/[:space:]]+$ ]]; then
  echo "GITHUB_REPOSITORY must identify one owner/repository." >&2
  exit 2
fi

resource_kind="${1:-}"
case "$resource_kind" in
  tag-ref)
    (( $# == 4 )) || usage
    tag="$2"
    expected_type="$3"
    expected_sha="${4,,}"
    if [[ -z "$tag" || \
          ( "$expected_type" != commit && "$expected_type" != tag ) || \
          ! "$expected_sha" =~ ^[0-9a-f]{40}$ ]]; then
      usage
    fi
    full_ref="refs/tags/${tag}"
    endpoint="repos/${GITHUB_REPOSITORY}/git/ref/tags/${tag}"
    description="Git ref ${full_ref}"
    if git -c credential.helper= \
      -c 'credential.helper=!gh auth git-credential' \
      push --porcelain --no-verify \
      "--force-with-lease=${full_ref}:${expected_sha}" \
      "https://github.com/${GITHUB_REPOSITORY}.git" ":${full_ref}"; then
      mutation_status=0
    else
      mutation_status=$?
      echo "Leased deletion of ${description} returned status ${mutation_status}; checking its exact final state." >&2
    fi
    ;;
  release)
    (( $# == 2 )) || usage
    release_id="$2"
    [[ "$release_id" =~ ^[1-9][0-9]*$ ]] || usage
    endpoint="repos/${GITHUB_REPOSITORY}/releases/${release_id}"
    description="GitHub Release ${release_id}"
    if gh api --method DELETE "$endpoint"; then
      mutation_status=0
    else
      mutation_status=$?
      echo "Deletion of ${description} returned status ${mutation_status}; checking its exact final state." >&2
    fi
    ;;
  *)
    usage
    ;;
esac

for delay in "${DELETION_CONFIRMATION_DELAYS[@]}"; do
  if [[ "$delay" != 0 ]]; then
    sleep "$delay"
  fi

  if response=$(gh api "$endpoint" 2>&1); then
    case "$resource_kind" in
      tag-ref)
        if ! jq -e '
          (type == "object")
          and (.ref | type == "string")
          and (.object | type == "object")
          and (.object.type | type == "string")
          and (.object.sha | type == "string")
        ' <<<"$response" > /dev/null; then
          echo "Post-delete state for ${description} is malformed; nothing else was deleted." >&2
          exit 1
        fi
        if [[ "$(jq -r '.ref' <<<"$response")" != "$full_ref" || \
              "$(jq -r '.object.type' <<<"$response")" != "$expected_type" || \
              "$(jq -r '.object.sha' <<<"$response" | tr '[:upper:]' '[:lower:]')" != "$expected_sha" ]]; then
          echo "${description} changed identity after deletion was requested; the new ref was preserved." >&2
          exit 1
        fi
        ;;
      release)
        if ! jq -e \
          --argjson expected_id "$release_id" \
          '(type == "object") and (.id == $expected_id)' \
          <<<"$response" > /dev/null; then
          echo "Post-delete state for ${description} is malformed or changed identity; nothing else was deleted." >&2
          exit 1
        fi
        ;;
    esac
    continue
  else
    read_status=$?
  fi

  if grep -Eq '^gh: .*\(HTTP 404\)\r?$' <<<"$response"; then
    exit 0
  fi
  printf '%s\n' "$response" >&2
  echo "Could not confirm deletion of ${description} (read status ${read_status}); deletion was not retried." >&2
  exit 1
done

echo "${description} still has the expected identity after bounded confirmation (mutation status ${mutation_status}); deletion was not retried." >&2
exit 1
