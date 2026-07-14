#!/usr/bin/env bash
set -Eeuo pipefail

# Read-only by default. Set ERROR_SNAPSHOT_SMOKE_MUTATE=1 only when an operator
# explicitly wants to exercise the cleanup endpoint on the isolated 8991 service.
readonly BASE_URL="${ERROR_SNAPSHOT_BASE_URL:-http://127.0.0.1:8991/api/admin}"
readonly ADMIN_TOKEN="${ERROR_SNAPSHOT_ADMIN_TOKEN:-}"
export ERROR_SNAPSHOT_SMOKE_MUTATE="${ERROR_SNAPSHOT_SMOKE_MUTATE:-0}"

if [[ -z "${BASE_URL}" ]]; then
  echo "ERROR_SNAPSHOT_BASE_URL must not be empty" >&2
  exit 2
fi

auth_args=()
if [[ -n "${ADMIN_TOKEN}" ]]; then
  auth_args+=( -H "Authorization: Bearer ${ADMIN_TOKEN}" )
fi

request() {
  local method="$1"
  local path="$2"
  shift 2
  curl --fail --silent --show-error \
    --request "${method}" \
    --header 'Accept: application/json' \
    "${auth_args[@]}" \
    "$@" \
    "${BASE_URL%/}${path}"
}

echo "Checking error snapshot storage..."
storage_json="$(request GET /error-snapshots/storage)"
if ! jq -e 'type == "object"' >/dev/null <<<"${storage_json}"; then
  echo "storage endpoint did not return a JSON object" >&2
  exit 1
fi

echo "Checking error snapshot listing..."
list_json="$(request GET '/error-snapshots?limit=1')"
if ! jq -e '(.records | type == "array") and (.total | type == "number")' >/dev/null <<<"${list_json}"; then
  echo "list endpoint returned an unexpected response shape" >&2
  exit 1
fi

if [[ "${ERROR_SNAPSHOT_SMOKE_MUTATE}" == "1" ]]; then
  echo "Running explicit cleanup smoke check..."
  cleanup_json="$(request POST /error-snapshots/cleanup)"
  if ! jq -e 'type == "object"' >/dev/null <<<"${cleanup_json}"; then
    echo "cleanup endpoint did not return a JSON object" >&2
    exit 1
  fi
else
  echo "Skipping cleanup (set ERROR_SNAPSHOT_SMOKE_MUTATE=1 to enable)."
fi

echo "error snapshot smoke check passed"
