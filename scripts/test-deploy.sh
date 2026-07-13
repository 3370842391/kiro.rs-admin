#!/usr/bin/env bash
set -Eeuo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(git -C "${SCRIPT_DIR}/.." rev-parse --show-toplevel)"
readonly LOCK_DIR="${TMPDIR:-/tmp}/kiro-rs-test-deploy.lock"
readonly COMPOSE_FILE="${REPO_ROOT}/docker-compose.test.yml"
readonly SERVICE="kiro-rs-test"
readonly HEALTH_URL="http://127.0.0.1:8991/"
readonly REQUESTED_REF="${1:-master}"

SECONDS=0

if ! mkdir "${LOCK_DIR}" 2>/dev/null; then
  echo "Another test deployment is already running (${LOCK_DIR})." >&2
  exit 75
fi
trap 'rmdir "${LOCK_DIR}" 2>/dev/null || true' EXIT

cd "${REPO_ROOT}"

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "Refusing to deploy from a repository with tracked changes." >&2
  exit 1
fi

git fetch --prune --tags deploy \
  "+refs/heads/master:refs/remotes/deploy/master"

case "${REQUESTED_REF}" in
  master | deploy/master | refs/heads/master | refs/remotes/deploy/master)
    TARGET_REF="refs/remotes/deploy/master"
    ;;
  *)
    TARGET_REF="${REQUESTED_REF}"
    ;;
esac

if ! TARGET_COMMIT="$(git rev-parse --verify --quiet --end-of-options "${TARGET_REF}^{commit}")"; then
  echo "Unable to resolve deployment ref: ${REQUESTED_REF}" >&2
  exit 1
fi

git checkout --detach "${TARGET_COMMIT}"

readonly SHORT_SHA="$(git rev-parse --short=12 HEAD)"
readonly NEW_TAG="${SHORT_SHA}"
readonly NEW_IMAGE="kiro-rs-test:${NEW_TAG}"

OLD_IMAGE="$(docker inspect --format '{{.Config.Image}}' "${SERVICE}" 2>/dev/null || true)"
OLD_TAG=""
case "${OLD_IMAGE}" in
  kiro-rs-test:*) OLD_TAG="${OLD_IMAGE#kiro-rs-test:}" ;;
esac

print_test_logs() {
  docker compose -f "${COMPOSE_FILE}" logs --tail=200 "${SERVICE}" >&2 || true
}

wait_for_health() {
  local attempt
  for attempt in $(seq 1 30); do
    if curl --fail --silent --show-error "${HEALTH_URL}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
  done
  return 1
}

restore_previous_image() {
  if [[ -z "${OLD_TAG}" ]]; then
    echo "No previous test image is available for rollback." >&2
    return 1
  fi

  echo "Restoring previous test image: ${OLD_IMAGE}" >&2
  if ! TEST_IMAGE_TAG="${OLD_TAG}" docker compose -f "${COMPOSE_FILE}" \
    up -d --force-recreate --no-build "${SERVICE}"; then
    echo "Failed to start the previous test image." >&2
    print_test_logs
    return 1
  fi

  if ! wait_for_health; then
    echo "Previous test image did not recover at ${HEALTH_URL}." >&2
    print_test_logs
    return 1
  fi

  echo "Previous test image is healthy again at ${HEALTH_URL}." >&2
}

DOCKER_BUILDKIT=1 docker build \
  --file Dockerfile.test \
  --tag "${NEW_IMAGE}" \
  .

docker run --rm "${NEW_IMAGE}" --version

if ! TEST_IMAGE_TAG="${NEW_TAG}" docker compose -f "${COMPOSE_FILE}" \
  up -d --force-recreate --no-build "${SERVICE}"; then
  echo "Failed to replace the test service with ${NEW_IMAGE}." >&2
  print_test_logs
  restore_previous_image || true
  exit 1
fi

if ! wait_for_health; then
  echo "Test service failed its health check at ${HEALTH_URL}." >&2
  print_test_logs
  restore_previous_image || true
  exit 1
fi

printf 'commit=%s\n' "${TARGET_COMMIT}"
printf 'image=%s\n' "${NEW_IMAGE}"
printf 'url=%s\n' "${HEALTH_URL}"
printf 'elapsed=%ss\n' "${SECONDS}"
