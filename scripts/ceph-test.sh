#!/usr/bin/env bash
#
# Provision a throwaway Ceph RGW (quay.io/ceph/demo) and run the read-only
# conformance suite against it.
#
# Usage:
#   scripts/ceph-test.sh              # start container, run tests, tear down
#   KEEP=1 scripts/ceph-test.sh       # keep the container after the run
#   scripts/ceph-test.sh -- --exact ceph_readonly_suite
#
# To test against an existing cluster instead, set CEPH_TEST_ENDPOINT (and
# CEPH_TEST_ACCESS_KEY / CEPH_TEST_SECRET_KEY); no container is started.
#
set -euo pipefail

# NixOS installs the setuid fusermount3 wrapper here; prefer it over any
# non-setuid copy on PATH so unprivileged mounts work.
if [ -d /run/wrappers/bin ]; then
  PATH="/run/wrappers/bin:$PATH"
  export PATH
fi

ROOT="${RUSTY_BUCKET_ROOT:-$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

IMAGE="${CEPH_TEST_IMAGE:-quay.io/ceph/demo:latest}"
NAME="${CEPH_TEST_CONTAINER:-rusty-bucket-ceph}"
HOST_PORT="${CEPH_TEST_PORT:-18080}"
ACCESS_KEY="${CEPH_TEST_ACCESS_KEY:-rustybucketaccess}"
SECRET_KEY="${CEPH_TEST_SECRET_KEY:-rustybucketsecretkey1234567890}"
BUCKET="${CEPH_TEST_BUCKET:-rusty-bucket-ceph-test}"
REGION="${CEPH_TEST_REGION:-us-east-1}"

log() { printf '==> %s\n' "$*" >&2; }

run_tests() {
  local endpoint="$1"
  shift
  log "running ceph_readonly_suite against ${endpoint}"
  CEPH_TEST_ENDPOINT="$endpoint" \
  CEPH_TEST_ACCESS_KEY="$ACCESS_KEY" \
  CEPH_TEST_SECRET_KEY="$SECRET_KEY" \
  CEPH_TEST_BUCKET="$BUCKET" \
  CEPH_TEST_REGION="$REGION" \
    cargo test --test ceph -- --test-threads=1 --nocapture "$@"
}

# ---------------------------------------------------------------------------
# External cluster: just run the tests.
# ---------------------------------------------------------------------------
if [ -n "${CEPH_TEST_ENDPOINT:-}" ]; then
  run_tests "$CEPH_TEST_ENDPOINT" "$@"
  exit $?
fi

# ---------------------------------------------------------------------------
# Provision a local Ceph RGW.
# ---------------------------------------------------------------------------
DOCKER=""
for candidate in docker podman; do
  if command -v "$candidate" >/dev/null 2>&1; then
    DOCKER="$candidate"
    break
  fi
done

if [ -z "$DOCKER" ]; then
  echo "error: neither docker nor podman found; set CEPH_TEST_ENDPOINT to use an existing cluster" >&2
  exit 1
fi

if ! "$DOCKER" info >/dev/null 2>&1; then
  echo "error: '$DOCKER' daemon is not reachable" >&2
  exit 1
fi

cleanup() {
  if [ "${KEEP:-0}" != "1" ]; then
    log "removing container $NAME"
    "$DOCKER" rm -f "$NAME" >/dev/null 2>&1 || true
  else
    log "keeping container $NAME"
  fi
}
trap cleanup EXIT

log "starting $IMAGE as $NAME (RGW on 127.0.0.1:${HOST_PORT})"
"$DOCKER" rm -f "$NAME" >/dev/null 2>&1 || true
# --hostname localhost makes RGW's `rgw dns name` resolve to `localhost`, which
# is required for path-style addressing (what the client uses).
"$DOCKER" run -d --name "$NAME" --hostname localhost \
  -e MON_IP=127.0.0.1 \
  -e CEPH_PUBLIC_NETWORK=127.0.0.0/8 \
  -e CEPH_DEMO_UID=demo \
  -e CEPH_DEMO_ACCESS_KEY="$ACCESS_KEY" \
  -e CEPH_DEMO_SECRET_KEY="$SECRET_KEY" \
  -p "${HOST_PORT}:8080" \
  "$IMAGE" >/dev/null

endpoint="http://127.0.0.1:${HOST_PORT}"
log "waiting for RGW at ${endpoint} (this can take a couple of minutes)"

ready=0
for _ in $(seq 1 90); do
  if ! "$DOCKER" inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null | grep -q true; then
    echo "error: container exited during bootstrap:" >&2
    "$DOCKER" logs --tail 40 "$NAME" >&2 || true
    exit 1
  fi

  code="$(curl -s -o /dev/null -w '%{http_code}' "$endpoint/" || true)"
  if [ "$code" != "000" ] && [ -n "$code" ]; then
    ready=1
    break
  fi
  sleep 2
done

if [ "$ready" != "1" ]; then
  echo "error: RGW did not become ready; logs:" >&2
  "$DOCKER" logs --tail 40 "$NAME" >&2 || true
  exit 1
fi

log "RGW is ready"
run_tests "$endpoint" "$@"
