#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=run-provider-gates.sh
source "$SCRIPT_DIRECTORY/run-provider-gates.sh"
trap - EXIT INT TERM HUP

grep -q 'wait_for_wokcore_ready' "$SCRIPT_DIRECTORY/run-provider-gates.sh"
grep -q '"status" "--json"' "$SCRIPT_DIRECTORY/run-provider-gates.sh"

(
    PROFILE=""
    OUTPUT_DIRECTORY=""
    TARGET_DIRECTORY=""
    parse_arguments \
        --profile pull-request \
        --output-directory /tmp/wokcore-portable-test-output
    [[ "$PROFILE_SECONDS" -eq 300 ]]
    [[ "$PROFILE_CONCURRENCY" -eq 256 ]]
    [[ "$RECOVERY_SECONDS" -eq 30 ]]
)

(
    PROFILE=""
    OUTPUT_DIRECTORY=""
    TARGET_DIRECTORY=""
    parse_arguments \
        --profile soak \
        --output-directory /tmp/wokcore-portable-test-output
    [[ "$PROFILE_SECONDS" -eq 1800 ]]
    [[ "$PROFILE_CONCURRENCY" -eq 500 ]]
    [[ "$RECOVERY_SECONDS" -eq 60 ]]
)

for endpoint in \
    "127.0.0.1:40100" \
    "[::1]:40100" \
    "::1:40100"; do
    endpoint_is_loopback "$endpoint"
done
for endpoint in \
    "0.0.0.0:40100" \
    "*:40100" \
    "192.0.2.1:40100" \
    "[2001:db8::1]:40100"; do
    if endpoint_is_loopback "$endpoint"; then
        printf 'non-loopback endpoint was accepted\n' >&2
        exit 1
    fi
done

TEST_ROOT="$(mktemp -d)"
trap 'rm -rf -- "$TEST_ROOT"' EXIT
TEMPORARY_ROOT="$TEST_ROOT"
REPORT_PATH="$TEST_ROOT/report.json"
PLATFORM="linux"
PROFILE="pull-request"
PROFILE_SECONDS=300
PROFILE_CONCURRENCY=256
STANDARD_ROUNDS=2
CANCELLATION_ROUNDS=2
TOTAL_STARTED=1024
TOTAL_COMPLETED=768
TOTAL_CANCELLED=256
TOTAL_ERRORS=0
MAX_PEAK_ACTIVE=256

write_final_report \
    true \
    "" \
    300 \
    20000 \
    70000 \
    24000 \
    85536 \
    20 \
    22 \
    4 \
    4

python3 - "$REPORT_PATH" <<'PY'
import json
import os
import sys

path = sys.argv[1]
with open(path, "r", encoding="utf-8") as handle:
    report = json.load(handle)
assert report["passed"] is True
assert report["profile_requested_seconds"] == 300
assert report["loads"]["peak_active"] == 256
assert report["loads"]["cancelled"] == 256
assert report["network_loopback_only"] is True
assert report["resources"]["recovery_rss_kib"] == 24000
assert os.path.getsize(path) < 131072
encoded = json.dumps(report).lower()
for forbidden in (
    "authorization",
    "bearer",
    "api_key",
    "access_token",
    "prompt",
    "message",
    "session",
    "payload",
):
    assert forbidden not in encoded
PY

REPORT_PATH="$TEST_ROOT/failure-report.json"
write_final_report \
    false \
    "non_loopback_network,fd_growth" \
    300 \
    20000 \
    70000 \
    24000 \
    85536 \
    20 \
    53 \
    4 \
    4
python3 - "$REPORT_PATH" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    report = json.load(handle)
assert report["passed"] is False
assert report["network_loopback_only"] is False
assert report["failures"] == ["non_loopback_network", "fd_growth"]
PY

printf 'portable provider gate shell tests passed\n'
