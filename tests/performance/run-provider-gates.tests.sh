#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=run-provider-gates.sh
source "$SCRIPT_DIRECTORY/run-provider-gates.sh"
trap - EXIT INT TERM HUP

grep -q 'wait_for_wokcore_ready' "$SCRIPT_DIRECTORY/run-provider-gates.sh"
grep -q '"status" "--json"' "$SCRIPT_DIRECTORY/run-provider-gates.sh"
grep -q 'report_wokcore_start_failure' "$SCRIPT_DIRECTORY/run-provider-gates.sh"
grep -q 'tail -c 4096' "$SCRIPT_DIRECTORY/run-provider-gates.sh"
grep -q 'vmmap -summary -resident' "$SCRIPT_DIRECTORY/run-provider-gates.sh"
grep -q 'vmmap_diagnostic' "$SCRIPT_DIRECTORY/run-provider-gates.sh"

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
LEGACY_VMMAP="$(
    printf '%s\n' \
        'Physical footprint:             128.5M' \
        '                                VIRTUAL   RESIDENT' \
        'MALLOC                           64.0M      32.0M' |
        python3 "$SCRIPT_DIRECTORY/parse-vmmap-summary.py"
)"
MODERN_VMMAP="$(
    printf '%s\n' \
        'Physical footprint:             1024K' \
        '                                VIRTUAL   REGION' \
        'MALLOC                           64.0M          3' |
        python3 "$SCRIPT_DIRECTORY/parse-vmmap-summary.py"
)"
if printf '%s\n' 'MALLOC 64.0M 32.0M' |
    python3 "$SCRIPT_DIRECTORY/parse-vmmap-summary.py" >/dev/null 2>&1; then
    printf 'vmmap parser accepted a missing physical footprint\n' >&2
    exit 1
fi
python3 - "$LEGACY_VMMAP" "$MODERN_VMMAP" <<'PY'
import json
import sys

legacy = json.loads(sys.argv[1])
modern = json.loads(sys.argv[2])
assert legacy == {
    "malloc_resident_kib": 32768,
    "malloc_resident_parser_status": "parsed",
    "physical_footprint_kib": 131584,
}
assert modern == {
    "malloc_resident_kib": None,
    "malloc_resident_parser_status": "unavailable",
    "physical_footprint_kib": 1024,
}
PY
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
RESOURCE_SAMPLES_FILE="$TEST_ROOT/resource-samples.tsv"
RECOVERY_STARTED_AT=100
printf '%s\n' \
    $'99\t70000\t20\t4' \
    $'100\t68000\t20\t4' \
    $'105\t42000\t19\t4' \
    $'130\t24000\t22\t4' \
    >"$RESOURCE_SAMPLES_FILE"

ARTIFACT_DIRECTORY="$TEST_ROOT/startup-diagnostics"
mkdir -p "$ARTIFACT_DIRECTORY"
printf '%s\n' \
    '{"code":"internal_error","secret":"must-not-appear"}' \
    >"$ARTIFACT_DIRECTORY/wokcore.stdout"
printf '%s\n' \
    'wokcore startup event_code=startup_diagnostics_segment_invalid' \
    'Bearer must-not-appear' \
    >"$ARTIFACT_DIRECTORY/wokcore.stderr"
STARTUP_DIAGNOSTICS="$(report_wokcore_start_failure 2>&1)"
[[ "$STARTUP_DIAGNOSTICS" == *"command_code=internal_error"* ]]
[[ "$STARTUP_DIAGNOSTICS" == *"event_code=startup_diagnostics_segment_invalid"* ]]
[[ "$STARTUP_DIAGNOSTICS" != *"must-not-appear"* ]]

(
    PLATFORM="macos"
    TEMPORARY_ROOT="$TEST_ROOT/macos-runtime"
    HOME="$TEST_ROOT/macos-original-home"
    SECURITY_CALLS="$TEST_ROOT/macos-security-calls.tsv"
    mkdir -p "$HOME" "$TEMPORARY_ROOT"
    SECURITY_DEFAULT_KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
    mkdir() {
        local argument
        for argument in "$@"; do
            case "$argument" in
                -m | -p | 700) ;;
                *) command mkdir -p "$argument" ;;
            esac
        done
    }
    security() {
        printf '%s\t%s\n' "$HOME" "$1" >>"$SECURITY_CALLS"
        case "$*" in
            "list-keychains -d user")
                printf '"%s"\n' "$HOME/Library/Keychains/login.keychain-db"
                ;;
            "default-keychain -d user")
                printf '"%s"\n' "$SECURITY_DEFAULT_KEYCHAIN"
                ;;
            "default-keychain -d user -s "*)
                SECURITY_DEFAULT_KEYCHAIN="${@: -1}"
                ;;
        esac
    }

    ORIGINAL_SECURITY_HOME="$HOME"
    isolate_environment

    [[ "$HOME" == "$ORIGINAL_SECURITY_HOME" ]]
    [[ "$WOKCORE_HOME" == "$TEMPORARY_ROOT/wokcore-home" ]]
    [[ "$CODEX_HOME" == "$TEMPORARY_ROOT/sessions/codex" ]]
    [[ "$CLAUDE_CONFIG_DIR" == "$TEMPORARY_ROOT/sessions/claude" ]]
    [[ "$GEMINI_CLI_HOME" == "$TEMPORARY_ROOT/sessions/gemini" ]]
    printf '%s\n' "${RUNTIME_ENVIRONMENT[@]}" |
        grep -Fx "HOME=$ORIGINAL_SECURITY_HOME" >/dev/null
    printf '%s\n' "${RUNTIME_ENVIRONMENT[@]}" |
        grep -Fx "WOKCORE_HOME=$TEMPORARY_ROOT/wokcore-home" >/dev/null

    python3 - "$SECURITY_CALLS" <<'PY'
import sys

calls = []
with open(sys.argv[1], "r", encoding="utf-8") as handle:
    for line in handle:
        home, command = line.rstrip("\n").split("\t", 1)
        calls.append((home, command))
assert len(calls) >= 13, calls
original_home = calls[0][0]
assert all(home == original_home for home, _ in calls), calls
assert calls[0][1] == "list-keychains", calls
assert calls[1][1] == "default-keychain", calls
assert any(command == "create-keychain" for _, command in calls[2:]), calls
assert any(command == "default-keychain" for _, command in calls[2:]), calls
assert sum(command == "default-keychain" for _, command in calls[7:]) >= 2, calls
assert any(command == "add-generic-password" for _, command in calls[7:]), calls
assert any(command == "delete-generic-password" for _, command in calls[7:]), calls
PY
)

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
assert report["resources"]["recovery_timeline"] == [
    {
        "elapsed_seconds": 0,
        "fd_count": 20,
        "rss_kib": 68000,
        "task_count": 4,
    },
    {
        "elapsed_seconds": 5,
        "fd_count": 19,
        "rss_kib": 42000,
        "task_count": 4,
    },
    {
        "elapsed_seconds": 30,
        "fd_count": 22,
        "rss_kib": 24000,
        "task_count": 4,
    },
]
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

PLATFORM="macos"
MACOS_VMMAP_BASELINE="$LEGACY_VMMAP"
MACOS_VMMAP_RECOVERY="$MODERN_VMMAP"
REPORT_PATH="$TEST_ROOT/macos-report.json"
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
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    report = json.load(handle)
assert report["resources"]["macos_vmmap"] == {
    "baseline": {
        "malloc_resident_kib": 32768,
        "malloc_resident_parser_status": "parsed",
        "physical_footprint_kib": 131584,
    },
    "recovery": {
        "malloc_resident_kib": None,
        "malloc_resident_parser_status": "unavailable",
        "physical_footprint_kib": 1024,
    },
}
PY
PLATFORM="linux"
MACOS_VMMAP_BASELINE="-"
MACOS_VMMAP_RECOVERY="-"

PLATFORM="macos"
MACOS_VMMAP_BASELINE='{"status":"capture_failed"}'
MACOS_VMMAP_RECOVERY="$MODERN_VMMAP"
REPORT_PATH="$TEST_ROOT/macos-diagnostic-failure-report.json"
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
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    report = json.load(handle)
assert report["passed"] is True
assert report["failures"] == []
assert report["resources"]["macos_vmmap"]["baseline"] == {
    "status": "capture_failed",
    "diagnostic": "vmmap_diagnostic",
}
PY
PLATFORM="linux"
MACOS_VMMAP_BASELINE="-"
MACOS_VMMAP_RECOVERY="-"

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
