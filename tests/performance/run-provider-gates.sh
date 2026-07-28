#!/usr/bin/env bash
set -Eeuo pipefail

PROFILE=""
OUTPUT_DIRECTORY=""
TARGET_DIRECTORY=""
SKIP_BUILD=0
PROFILE_SECONDS=0
PROFILE_CONCURRENCY=0
RECOVERY_SECONDS=0
PLATFORM=""
REPOSITORY_ROOT=""
MAIN_REPOSITORY=""
TEMPORARY_PARENT=""
TEMPORARY_ROOT=""
ARTIFACT_DIRECTORY=""
MONITOR_PID=""
WOKCORE_PID=""
SIMULATOR_PID=""
CURRENT_LOAD_PID=""
LOAD_PID_FILE=""
MONITOR_STOP_FILE=""
NETWORK_VIOLATION_FILE=""
PROCESS_VIOLATION_FILE=""
RESOURCE_SAMPLES_FILE=""
GNOME_KEYRING_PID=""
MAC_KEYCHAIN_ACTIVE=0
MAC_KEYCHAIN_PATH=""
MAC_KEYCHAIN_PASSWORD=""
MAC_SECURITY_HOME=""
ORIGINAL_DEFAULT_KEYCHAIN=""
ORIGINAL_KEYCHAINS=()
REPORT_PATH=""
RUNTIME_ENVIRONMENT=()

usage() {
    cat <<'EOF'
Usage: run-provider-gates.sh --profile pull-request|soak --output-directory PATH [options]

Options:
  --target-directory PATH  Stable Cargo target directory.
  --skip-build             Use existing fixed-name release executables.
  -h, --help               Show this help.
EOF
}

fail() {
    printf 'portable provider gate: %s\n' "$1" >&2
    exit 1
}

command_required() {
    command -v "$1" >/dev/null 2>&1 ||
        fail "required runtime command is unavailable"
}

real_path() {
    python3 - "$1" <<'PY'
import os
import sys

print(os.path.realpath(os.path.abspath(sys.argv[1])))
PY
}

path_is_within() {
    python3 - "$1" "$2" <<'PY'
import os
import sys

candidate = os.path.realpath(os.path.abspath(sys.argv[1]))
root = os.path.realpath(os.path.abspath(sys.argv[2]))
try:
    inside = os.path.commonpath((candidate, root)) == root
except ValueError:
    inside = False
raise SystemExit(0 if inside else 1)
PY
}

trim_security_path() {
    local value="$1"
    value="${value#"${value%%[![:space:]]*}"}"
    value="${value%"${value##*[![:space:]]}"}"
    value="${value#\"}"
    value="${value%\"}"
    printf '%s' "$value"
}

cleanup() {
    local exit_code=$?
    local cleanup_attempt
    set +e

    if [[ -n "$MONITOR_STOP_FILE" ]]; then
        : >"$MONITOR_STOP_FILE"
    fi
    if [[ -n "$MONITOR_PID" ]]; then
        wait "$MONITOR_PID" 2>/dev/null
    fi

    if [[ -n "$CURRENT_LOAD_PID" ]] &&
        kill -0 "$CURRENT_LOAD_PID" 2>/dev/null; then
        kill -TERM "$CURRENT_LOAD_PID" 2>/dev/null
        wait "$CURRENT_LOAD_PID" 2>/dev/null
    fi
    if [[ -n "$WOKCORE_PID" ]] && kill -0 "$WOKCORE_PID" 2>/dev/null; then
        kill -TERM "$WOKCORE_PID" 2>/dev/null
        for ((cleanup_attempt = 0; cleanup_attempt < 100; cleanup_attempt++)); do
            kill -0 "$WOKCORE_PID" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$WOKCORE_PID" 2>/dev/null
        wait "$WOKCORE_PID" 2>/dev/null
    fi
    if [[ -n "$SIMULATOR_PID" ]] && kill -0 "$SIMULATOR_PID" 2>/dev/null; then
        kill -TERM "$SIMULATOR_PID" 2>/dev/null
        for ((cleanup_attempt = 0; cleanup_attempt < 50; cleanup_attempt++)); do
            kill -0 "$SIMULATOR_PID" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$SIMULATOR_PID" 2>/dev/null
        wait "$SIMULATOR_PID" 2>/dev/null
    fi

    if [[ "$MAC_KEYCHAIN_ACTIVE" -eq 1 ]]; then
        if [[ "${#ORIGINAL_KEYCHAINS[@]}" -gt 0 ]]; then
            security list-keychains -d user -s "${ORIGINAL_KEYCHAINS[@]}" >/dev/null 2>&1
        fi
        if [[ -n "$ORIGINAL_DEFAULT_KEYCHAIN" ]]; then
            security default-keychain -d user -s "$ORIGINAL_DEFAULT_KEYCHAIN" >/dev/null 2>&1
        fi
        if [[ -n "$MAC_KEYCHAIN_PATH" ]]; then
            security delete-keychain "$MAC_KEYCHAIN_PATH" >/dev/null 2>&1
        fi
    fi
    if [[ -n "$GNOME_KEYRING_PID" ]] &&
        [[ "$GNOME_KEYRING_PID" =~ ^[0-9]+$ ]] &&
        kill -0 "$GNOME_KEYRING_PID" 2>/dev/null; then
        kill -TERM "$GNOME_KEYRING_PID" 2>/dev/null
        wait "$GNOME_KEYRING_PID" 2>/dev/null
    fi

    if [[ -n "$TEMPORARY_ROOT" && -n "$TEMPORARY_PARENT" ]]; then
        local resolved_root
        resolved_root="$(real_path "$TEMPORARY_ROOT" 2>/dev/null)"
        case "$resolved_root" in
            "$TEMPORARY_PARENT"/wokcore-portable-gates.*)
                rm -rf -- "$resolved_root"
                ;;
        esac
    fi
    exit "$exit_code"
}

trap cleanup EXIT
trap 'exit 130' INT TERM HUP

parse_arguments() {
    while [[ "$#" -gt 0 ]]; do
        case "$1" in
            --profile)
                [[ "$#" -ge 2 ]] || fail "profile value is missing"
                PROFILE="$2"
                shift 2
                ;;
            --output-directory)
                [[ "$#" -ge 2 ]] || fail "output directory value is missing"
                OUTPUT_DIRECTORY="$2"
                shift 2
                ;;
            --target-directory)
                [[ "$#" -ge 2 ]] || fail "target directory value is missing"
                TARGET_DIRECTORY="$2"
                shift 2
                ;;
            --skip-build)
                SKIP_BUILD=1
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                fail "unknown argument"
                ;;
        esac
    done

    case "$PROFILE" in
        pull-request)
            PROFILE_SECONDS=300
            PROFILE_CONCURRENCY=256
            RECOVERY_SECONDS=30
            ;;
        soak)
            PROFILE_SECONDS=1800
            PROFILE_CONCURRENCY=500
            RECOVERY_SECONDS=60
            ;;
        *)
            fail "profile must be pull-request or soak"
            ;;
    esac
    [[ -n "$OUTPUT_DIRECTORY" ]] || fail "an output directory is required"
}

configure_platform() {
    case "$(uname -s)" in
        Linux)
            PLATFORM="linux"
            command_required ss
            command_required gnome-keyring-daemon
            [[ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ]] ||
                fail "Linux requires a private D-Bus session"
            [[ "${WOKCORE_PRIVATE_DBUS:-}" == "1" ]] ||
                fail "Linux requires WOKCORE_PRIVATE_DBUS=1"
            ;;
        Darwin)
            PLATFORM="macos"
            command_required lsof
            command_required security
            ;;
        *)
            fail "portable provider gates support Linux and macOS only"
            ;;
    esac
}

configure_paths() {
    command_required python3
    command_required git
    command_required cargo

    REPOSITORY_ROOT="$(real_path "$(dirname "${BASH_SOURCE[0]}")/../..")"
    local git_common
    git_common="$(
        git -C "$REPOSITORY_ROOT" \
            rev-parse --path-format=absolute --git-common-dir
    )" || fail "the stable Cargo target directory could not be resolved"
    MAIN_REPOSITORY="$(real_path "$(dirname "$git_common")")"

    if [[ -z "$TARGET_DIRECTORY" ]]; then
        TARGET_DIRECTORY="$MAIN_REPOSITORY/target"
    fi
    TARGET_DIRECTORY="$(real_path "$TARGET_DIRECTORY")"
    OUTPUT_DIRECTORY="$(real_path "$OUTPUT_DIRECTORY")"
    if path_is_within "$OUTPUT_DIRECTORY" "$REPOSITORY_ROOT" ||
        path_is_within "$OUTPUT_DIRECTORY" "$MAIN_REPOSITORY"; then
        fail "provider gate evidence must remain outside the public repository"
    fi
    if [[ -L "$OUTPUT_DIRECTORY" ]]; then
        fail "provider gate output directory must not be a symbolic link"
    fi
    umask 077
    mkdir -p -- "$OUTPUT_DIRECTORY"
    REPORT_PATH="$OUTPUT_DIRECTORY/provider-gates-${PLATFORM}-${PROFILE}.json"
}

build_fixed_executables() {
    if [[ "$SKIP_BUILD" -eq 0 ]]; then
        cargo +1.97.1 build \
            --workspace \
            --all-features \
            --release \
            --locked \
            --offline \
            --target-dir "$TARGET_DIRECTORY"
    fi

    WOKCORE_EXECUTABLE="$TARGET_DIRECTORY/release/wokcore"
    SIMULATOR_EXECUTABLE="$TARGET_DIRECTORY/release/wokcore-provider-sim"
    LOAD_GENERATOR_EXECUTABLE="$TARGET_DIRECTORY/release/wokcore-loadgen"
    for executable in \
        "$WOKCORE_EXECUTABLE" \
        "$SIMULATOR_EXECUTABLE" \
        "$LOAD_GENERATOR_EXECUTABLE"; do
        [[ -f "$executable" && -x "$executable" ]] ||
            fail "a fixed-name release provider gate executable is missing"
    done
    WOKCORE_EXECUTABLE="$(real_path "$WOKCORE_EXECUTABLE")"
    SIMULATOR_EXECUTABLE="$(real_path "$SIMULATOR_EXECUTABLE")"
    LOAD_GENERATOR_EXECUTABLE="$(real_path "$LOAD_GENERATOR_EXECUTABLE")"
}

create_private_runtime() {
    TEMPORARY_PARENT="$(real_path "${TMPDIR:-/tmp}")"
    TEMPORARY_ROOT="$(
        mktemp -d "$TEMPORARY_PARENT/wokcore-portable-gates.XXXXXXXX"
    )"
    TEMPORARY_ROOT="$(real_path "$TEMPORARY_ROOT")"
    case "$TEMPORARY_ROOT" in
        "$TEMPORARY_PARENT"/wokcore-portable-gates.*) ;;
        *) fail "provider gate temporary path resolution failed" ;;
    esac
    chmod 700 "$TEMPORARY_ROOT"
    ARTIFACT_DIRECTORY="$TEMPORARY_ROOT/artifacts"
    mkdir -m 700 "$ARTIFACT_DIRECTORY"
    LOAD_PID_FILE="$TEMPORARY_ROOT/load.pid"
    MONITOR_STOP_FILE="$TEMPORARY_ROOT/monitor.stop"
    NETWORK_VIOLATION_FILE="$TEMPORARY_ROOT/network.violation"
    PROCESS_VIOLATION_FILE="$TEMPORARY_ROOT/process.violation"
    RESOURCE_SAMPLES_FILE="$TEMPORARY_ROOT/resource.tsv"
    : >"$LOAD_PID_FILE"
    : >"$RESOURCE_SAMPLES_FILE"
}

capture_macos_keychain_configuration() {
    while IFS= read -r line; do
        local keychain
        keychain="$(trim_security_path "$line")"
        [[ -n "$keychain" ]] && ORIGINAL_KEYCHAINS+=("$keychain")
    done < <(security list-keychains -d user)
    ORIGINAL_DEFAULT_KEYCHAIN="$(
        trim_security_path "$(security default-keychain -d user)"
    )"
}

configure_macos_keychain() {
    MAC_KEYCHAIN_PATH="$TEMPORARY_ROOT/wokcore-performance.keychain-db"
    MAC_KEYCHAIN_PASSWORD="$(
        python3 - <<'PY'
import os
print(os.urandom(24).hex())
PY
    )"
    security create-keychain -p "$MAC_KEYCHAIN_PASSWORD" "$MAC_KEYCHAIN_PATH"
    MAC_KEYCHAIN_ACTIVE=1
    security set-keychain-settings -lut 7200 "$MAC_KEYCHAIN_PATH"
    security unlock-keychain -p "$MAC_KEYCHAIN_PASSWORD" "$MAC_KEYCHAIN_PATH"
    security list-keychains -d user -s "$MAC_KEYCHAIN_PATH"
    security default-keychain -d user -s "$MAC_KEYCHAIN_PATH"
}

prepare_macos_runtime_keychain() {
    local observed_default
    local probe_account="wokcore-performance-$$"
    local probe_service="dev.wokcore.performance-probe"

    security list-keychains -d user -s "$MAC_KEYCHAIN_PATH"
    security default-keychain -d user -s "$MAC_KEYCHAIN_PATH"
    security unlock-keychain -p "$MAC_KEYCHAIN_PASSWORD" "$MAC_KEYCHAIN_PATH"
    observed_default="$(
        trim_security_path "$(security default-keychain -d user)"
    )"
    if [[ "$(real_path "$observed_default")" != "$(real_path "$MAC_KEYCHAIN_PATH")" ]]; then
        fail "the isolated macOS keychain is not the runtime default"
    fi
    security add-generic-password \
        -a "$probe_account" \
        -s "$probe_service" \
        -w "$MAC_KEYCHAIN_PASSWORD" >/dev/null
    security delete-generic-password \
        -a "$probe_account" \
        -s "$probe_service" >/dev/null
    MAC_KEYCHAIN_PASSWORD=""
}

configure_linux_keyring() {
    local daemon_output
    local keyring_password
    keyring_password="$(
        python3 - <<'PY'
import os
print(os.urandom(24).hex())
PY
    )"
    daemon_output="$(
        printf '%s' "$keyring_password" |
            gnome-keyring-daemon --unlock --components=secrets 2>/dev/null
    )" || fail "the private Linux Secret Service could not start"
    while IFS= read -r assignment; do
        case "$assignment" in
            GNOME_KEYRING_CONTROL=*)
                assignment="${assignment#GNOME_KEYRING_CONTROL=}"
                assignment="${assignment%;}"
                export GNOME_KEYRING_CONTROL="$assignment"
                ;;
            GNOME_KEYRING_PID=*)
                assignment="${assignment#GNOME_KEYRING_PID=}"
                assignment="${assignment%;}"
                GNOME_KEYRING_PID="$assignment"
                ;;
        esac
    done <<<"$daemon_output"
}

isolate_environment() {
    local isolated_home="$TEMPORARY_ROOT/home"
    if [[ "$PLATFORM" == "macos" ]]; then
        MAC_SECURITY_HOME="$HOME"
        capture_macos_keychain_configuration
        configure_macos_keychain
    fi

    export WOKCORE_HOME="$TEMPORARY_ROOT/wokcore-home"
    if [[ "$PLATFORM" == "macos" ]]; then
        export HOME="$MAC_SECURITY_HOME"
    else
        export HOME="$isolated_home"
    fi
    export USERPROFILE="$HOME"
    export XDG_CONFIG_HOME="$TEMPORARY_ROOT/config"
    export XDG_STATE_HOME="$TEMPORARY_ROOT/state"
    export XDG_CACHE_HOME="$TEMPORARY_ROOT/cache"
    export XDG_DATA_HOME="$TEMPORARY_ROOT/data"
    export XDG_RUNTIME_DIR="$TEMPORARY_ROOT/runtime"
    export TMPDIR="$TEMPORARY_ROOT/tmp"
    mkdir -m 700 \
        "$WOKCORE_HOME" \
        "$isolated_home" \
        "$XDG_CONFIG_HOME" \
        "$XDG_STATE_HOME" \
        "$XDG_CACHE_HOME" \
        "$XDG_DATA_HOME" \
        "$XDG_RUNTIME_DIR" \
        "$TMPDIR"

    if [[ "$PLATFORM" == "macos" ]]; then
        prepare_macos_runtime_keychain
    fi

    unset \
        OPENAI_API_KEY \
        ANTHROPIC_API_KEY \
        GOOGLE_API_KEY \
        GEMINI_API_KEY \
        AZURE_OPENAI_API_KEY \
        AZURE_OPENAI_ENDPOINT \
        AWS_ACCESS_KEY_ID \
        AWS_SECRET_ACCESS_KEY \
        AWS_SESSION_TOKEN \
        GITHUB_TOKEN \
        GH_TOKEN \
        CODEX_HOME \
        CLAUDE_CONFIG_DIR \
        GEMINI_CLI_HOME \
        HTTP_PROXY \
        HTTPS_PROXY \
        ALL_PROXY \
        http_proxy \
        https_proxy \
        all_proxy || true
    export CODEX_HOME="$TEMPORARY_ROOT/sessions/codex"
    export CLAUDE_CONFIG_DIR="$TEMPORARY_ROOT/sessions/claude"
    export GEMINI_CLI_HOME="$TEMPORARY_ROOT/sessions/gemini"
    mkdir -m 700 -p \
        "$CODEX_HOME" \
        "$CLAUDE_CONFIG_DIR" \
        "$GEMINI_CLI_HOME"
    export NO_PROXY="127.0.0.1,::1"
    export no_proxy="$NO_PROXY"

    if [[ "$PLATFORM" == "linux" ]]; then
        configure_linux_keyring
    fi

    RUNTIME_ENVIRONMENT=(
        env -i
        "PATH=$PATH"
        "WOKCORE_HOME=$WOKCORE_HOME"
        "HOME=$HOME"
        "USERPROFILE=$USERPROFILE"
        "CODEX_HOME=$CODEX_HOME"
        "CLAUDE_CONFIG_DIR=$CLAUDE_CONFIG_DIR"
        "GEMINI_CLI_HOME=$GEMINI_CLI_HOME"
        "XDG_CONFIG_HOME=$XDG_CONFIG_HOME"
        "XDG_STATE_HOME=$XDG_STATE_HOME"
        "XDG_CACHE_HOME=$XDG_CACHE_HOME"
        "XDG_DATA_HOME=$XDG_DATA_HOME"
        "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"
        "TMPDIR=$TMPDIR"
        "NO_PROXY=$NO_PROXY"
        "no_proxy=$no_proxy"
        "LANG=${LANG:-C.UTF-8}"
    )
    if [[ "$PLATFORM" == "linux" ]]; then
        RUNTIME_ENVIRONMENT+=(
            "DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS"
        )
        if [[ -n "${GNOME_KEYRING_CONTROL:-}" ]]; then
            RUNTIME_ENVIRONMENT+=(
                "GNOME_KEYRING_CONTROL=$GNOME_KEYRING_CONTROL"
            )
        fi
    fi
}

free_loopback_port() {
    python3 - <<'PY'
import socket

with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
    listener.bind(("127.0.0.1", 0))
    print(listener.getsockname()[1])
PY
}

write_config() {
    CORE_PORT="$(free_loopback_port)"
    while :; do
        SIMULATOR_PORT="$(free_loopback_port)"
        [[ "$SIMULATOR_PORT" != "$CORE_PORT" ]] && break
    done

    local config_directory="$WOKCORE_HOME"
    mkdir -m 700 -p "$config_directory"
    CONFIG_PATH="$config_directory/config.toml"
    cat >"$CONFIG_PATH" <<EOF
revision = 1

[server]
port = $CORE_PORT

[providers]

[[providers.instances]]
id = "synthetic"
catalog_id = "ollama"
enabled = true
endpoint = "http://127.0.0.1:$SIMULATOR_PORT/v1"
allow_private_network = true

[[providers.accounts]]
id = "local"
provider = "synthetic"
enabled = true

[providers.accounts.auth]
kind = "local"

[routing]
aliases = []
rules = []

[routing.default]
provider = "synthetic"
model = "synthetic"
EOF
    chmod 600 "$CONFIG_PATH"
}

wait_for_loopback_port() {
    local port="$1"
    local owner_pid="$2"
    python3 - "$port" "$owner_pid" <<'PY'
import os
import socket
import sys
import time

port = int(sys.argv[1])
pid = int(sys.argv[2])
deadline = time.monotonic() + 20
while time.monotonic() < deadline:
    try:
        os.kill(pid, 0)
    except OSError:
        raise SystemExit(1)
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.2):
            raise SystemExit(0)
    except OSError:
        time.sleep(0.1)
raise SystemExit(1)
PY
}

wait_for_wokcore_ready() {
    local owner_pid="$1"
    local stdout_path="$ARTIFACT_DIRECTORY/readiness.stdout"
    local stderr_path="$ARTIFACT_DIRECTORY/readiness.stderr"
    local attempt
    for ((attempt = 0; attempt < 200; attempt++)); do
        kill -0 "$owner_pid" 2>/dev/null || return 1
        if "${RUNTIME_ENVIRONMENT[@]}" "$WOKCORE_EXECUTABLE" "status" "--json" \
            >"$stdout_path" 2>"$stderr_path" &&
            python3 - "$stdout_path" <<'PY'
import json
import os
import sys

path = sys.argv[1]
if os.path.getsize(path) >= 4096:
    raise SystemExit(1)
with open(path, "r", encoding="utf-8") as handle:
    value = json.load(handle)
raise SystemExit(0 if value.get("code") == "running" else 1)
PY
        then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

report_wokcore_start_failure() {
    local stdout_path="$ARTIFACT_DIRECTORY/wokcore.stdout"
    local stderr_path="$ARTIFACT_DIRECTORY/wokcore.stderr"
    local command_code=""
    local startup_events=""
    if [[ -f "$stdout_path" ]]; then
        command_code="$(
            tail -c 4096 "$stdout_path" |
                grep -Eo '"code"[[:space:]]*:[[:space:]]*"[a-z0-9_]+"' |
                head -n 1 |
                sed -E 's/^"code"[[:space:]]*:[[:space:]]*"([a-z0-9_]+)"$/\1/' ||
                true
        )"
    fi
    if [[ -f "$stderr_path" ]]; then
        startup_events="$(
            tail -c 4096 "$stderr_path" |
                grep -E '^wokcore startup event_code=[a-z0-9_]+$' || true
        )"
    fi
    [[ -n "$command_code" || -n "$startup_events" ]] || return 0
    printf '%s\n' "portable provider gate: bounded WokCore startup diagnostics follow" >&2
    if [[ -n "$command_code" ]]; then
        printf 'portable provider gate: WokCore command_code=%s\n' "$command_code" >&2
    fi
    if [[ -n "$startup_events" ]]; then
        printf '%s\n' "$startup_events" >&2
    fi
}

assert_exact_process_path() {
    local pid="$1"
    local expected="$2"
    local observed
    local path_attempt
    for ((path_attempt = 0; path_attempt < 50; path_attempt++)); do
        kill -0 "$pid" 2>/dev/null ||
            fail "a provider gate process exited before path verification"
        observed=""
        if [[ "$PLATFORM" == "linux" ]]; then
            observed="$(real_path "/proc/$pid/exe" 2>/dev/null || true)"
        else
            observed="$(
                lsof -nP -a -p "$pid" -d txt -Fn 2>/dev/null |
                    awk '/^n/{print substr($0, 2); exit}'
            )"
            if [[ -n "$observed" ]]; then
                observed="$(real_path "$observed" 2>/dev/null || true)"
            fi
        fi
        [[ "$observed" == "$expected" ]] && return 0
        sleep 0.02
    done
    fail "a provider gate process path did not match its fixed executable"
}

endpoint_is_loopback() {
    local endpoint="$1"
    case "$endpoint" in
        127.*:*|"[::1]":*|::1:*) return 0 ;;
        *) return 1 ;;
    esac
}

audit_linux_pid() {
    local pid="$1"
    local line
    local state
    local local_endpoint
    local peer_endpoint

    local tcp_sockets
    local udp_sockets
    tcp_sockets="$(ss -H -t -a -n -p 2>/dev/null)" || return 1
    udp_sockets="$(ss -H -u -a -n -p 2>/dev/null)" || return 1

    while IFS= read -r line; do
        [[ "$line" == *"pid=$pid,"* ]] || continue
        read -r state _ _ local_endpoint peer_endpoint _ <<<"$line"
        endpoint_is_loopback "$local_endpoint" || return 1
        if [[ "$state" != "LISTEN" ]]; then
            endpoint_is_loopback "$peer_endpoint" || return 1
        fi
    done <<<"$tcp_sockets"

    while IFS= read -r line; do
        [[ "$line" == *"pid=$pid,"* ]] && return 1
    done <<<"$udp_sockets"
    return 0
}

audit_macos_pid() {
    local pid="$1"
    local line
    local endpoint
    local left
    local right

    while IFS= read -r line; do
        [[ "$line" == COMMAND* ]] && continue
        [[ -z "$line" ]] && continue
        [[ "$line" == *" UDP "* ]] && return 1
        [[ "$line" == *" TCP "* ]] || continue
        endpoint="$(
            awk '{
                for (field_index = 1; field_index <= NF; field_index++) {
                    if ($field_index == "TCP") {
                        print $(field_index + 1)
                        exit
                    }
                }
            }' <<<"$line"
        )"
        if [[ "$endpoint" == *"->"* ]]; then
            left="${endpoint%%->*}"
            right="${endpoint#*->}"
            endpoint_is_loopback "$left" || return 1
            endpoint_is_loopback "$right" || return 1
        else
            endpoint_is_loopback "$endpoint" || return 1
        fi
    done < <(lsof -nP -a -p "$pid" -iTCP -iUDP 2>/dev/null || true)
    return 0
}

audit_pid_network() {
    local pid="$1"
    kill -0 "$pid" 2>/dev/null || return 0
    if [[ "$PLATFORM" == "linux" ]]; then
        audit_linux_pid "$pid"
    else
        audit_macos_pid "$pid"
    fi
}

resource_snapshot() {
    local pid="$1"
    if [[ "$PLATFORM" == "linux" ]]; then
        local rss
        local descriptors
        local tasks
        rss="$(awk '/^VmRSS:/{print $2; exit}' "/proc/$pid/status")"
        descriptors="$(
            find "/proc/$pid/fd" -mindepth 1 -maxdepth 1 -printf '.' 2>/dev/null |
                wc -c |
                tr -d '[:space:]'
        )"
        tasks="$(
            find "/proc/$pid/task" -mindepth 1 -maxdepth 1 -type d -printf '.' 2>/dev/null |
                wc -c |
                tr -d '[:space:]'
        )"
        [[ "$rss" =~ ^[0-9]+$ ]] || return 1
        printf '%s\t%s\t%s\n' "$rss" "$descriptors" "$tasks"
    else
        local rss
        local descriptors
        local tasks
        rss="$(ps -o rss= -p "$pid" | tr -d '[:space:]')"
        descriptors="$(
            lsof -nP -a -p "$pid" -F f 2>/dev/null |
                awk '/^f[0-9]+/{count++} END{print count + 0}'
        )"
        tasks="$(
            ps -M -p "$pid" |
                awk 'NR > 1 {count++} END{print count + 0}'
        )"
        [[ "$rss" =~ ^[0-9]+$ ]] || return 1
        printf '%s\t%s\t%s\n' "$rss" "$descriptors" "$tasks"
    fi
}

monitor_runtime() {
    while [[ ! -e "$MONITOR_STOP_FILE" ]]; do
        if [[ -n "$WOKCORE_PID" ]] && kill -0 "$WOKCORE_PID" 2>/dev/null; then
            local sample
            if sample="$(resource_snapshot "$WOKCORE_PID")"; then
                printf '%s\t%s\n' "$(date +%s)" "$sample" >>"$RESOURCE_SAMPLES_FILE"
            else
                : >"$PROCESS_VIOLATION_FILE"
            fi
        elif [[ -n "$WOKCORE_PID" ]]; then
            : >"$PROCESS_VIOLATION_FILE"
        fi

        local pid
        for pid in "$WOKCORE_PID" "$SIMULATOR_PID"; do
            [[ -n "$pid" ]] || continue
            audit_pid_network "$pid" || : >"$NETWORK_VIOLATION_FILE"
        done
        if [[ -s "$LOAD_PID_FILE" ]]; then
            pid="$(<"$LOAD_PID_FILE")"
            if [[ "$pid" =~ ^[0-9]+$ ]]; then
                audit_pid_network "$pid" || : >"$NETWORK_VIOLATION_FILE"
            fi
        fi
        sleep 1
    done
}

start_runtime() {
    local scenario
    scenario="$REPOSITORY_ROOT/crates/wokcore-provider-sim/scenarios/standard.toml"

    "${RUNTIME_ENVIRONMENT[@]}" "$SIMULATOR_EXECUTABLE" \
        --bind "127.0.0.1:$SIMULATOR_PORT" \
        --scenario "$scenario" \
        >"$ARTIFACT_DIRECTORY/simulator.stdout" \
        2>"$ARTIFACT_DIRECTORY/simulator.stderr" &
    SIMULATOR_PID=$!
    assert_exact_process_path "$SIMULATOR_PID" "$SIMULATOR_EXECUTABLE"
    wait_for_loopback_port "$SIMULATOR_PORT" "$SIMULATOR_PID" ||
        fail "the synthetic Provider did not become ready"

    "${RUNTIME_ENVIRONMENT[@]}" "$WOKCORE_EXECUTABLE" serve --json \
        >"$ARTIFACT_DIRECTORY/wokcore.stdout" \
        2>"$ARTIFACT_DIRECTORY/wokcore.stderr" &
    WOKCORE_PID=$!
    assert_exact_process_path "$WOKCORE_PID" "$WOKCORE_EXECUTABLE"
    wait_for_loopback_port "$CORE_PORT" "$WOKCORE_PID" || {
        report_wokcore_start_failure
        fail "WokCore did not become ready"
    }
    wait_for_wokcore_ready "$WOKCORE_PID" || {
        report_wokcore_start_failure
        fail "WokCore management plane did not become ready"
    }

    monitor_runtime &
    MONITOR_PID=$!
}

authorize_load_generator() {
    local authorization_json
    authorization_json="$(
        "${RUNTIME_ENVIRONMENT[@]}" "$WOKCORE_EXECUTABLE" \
            authorize \
            --client wokcore-portable-performance-gate \
            --scope proxy.use \
            --json
    )" || fail "synthetic provider gate authorization failed"
    CLIENT_TOKEN="$(
        printf '%s' "$authorization_json" |
            python3 -c '
import json
import sys
value = json.load(sys.stdin).get("token", "")
if not isinstance(value, str) or not value.startswith("wok_proxy_v1_"):
    raise SystemExit(1)
print(value, end="")
'
    )" || fail "synthetic provider gate token shape was invalid"
    authorization_json=""
}

parse_load_report() {
    local report_path="$1"
    python3 - "$report_path" <<'PY'
import json
import os
import sys

path = sys.argv[1]
if os.path.getsize(path) <= 0 or os.path.getsize(path) >= 131072:
    raise SystemExit(1)
with open(path, "r", encoding="utf-8") as handle:
    report = json.load(handle)
keys = ("started", "active", "peak_active", "completed", "cancelled", "errors")
values = []
for key in keys:
    value = report.get(key)
    if not isinstance(value, int) or value < 0:
        raise SystemExit(1)
    values.append(value)
print("\t".join(str(value) for value in values))
PY
}

TOTAL_STARTED=0
TOTAL_COMPLETED=0
TOTAL_CANCELLED=0
TOTAL_ERRORS=0
MAX_PEAK_ACTIVE=0
STANDARD_ROUNDS=0
CANCELLATION_ROUNDS=0
LOAD_SEQUENCE=0
LOAD_FAILURE=0

run_load_phase() {
    local phase="$1"
    local concurrency="$2"
    local cancellation_permyriad="$3"
    local report_path
    local load_exit
    local values
    local started
    local active
    local peak
    local completed
    local cancelled
    local errors

    LOAD_SEQUENCE=$((LOAD_SEQUENCE + 1))
    report_path="$ARTIFACT_DIRECTORY/load-$LOAD_SEQUENCE.json"
    set +e
    printf '%s' "$CLIENT_TOKEN" |
        "${RUNTIME_ENVIRONMENT[@]}" "$LOAD_GENERATOR_EXECUTABLE" \
            --target "http://127.0.0.1:$CORE_PORT" \
            --concurrency "$concurrency" \
            --ramp-ms 250 \
            --duration-ms 15000 \
            --protocol responses=2 \
            --protocol chat=2 \
            --protocol anthropic=1 \
            --payload-profile long-reasoning \
            --cancellation-permyriad "$cancellation_permyriad" \
            --slow-consumer-ms 1 \
            --token-stdin \
            --max-errors 0 \
            --require-peak-active "$concurrency" \
            >"$report_path" \
            2>"$ARTIFACT_DIRECTORY/load-$LOAD_SEQUENCE.stderr" &
    local load_pid=$!
    CURRENT_LOAD_PID="$load_pid"
    printf '%s' "$load_pid" >"$LOAD_PID_FILE"
    assert_exact_process_path "$load_pid" "$LOAD_GENERATOR_EXECUTABLE"
    wait "$load_pid"
    load_exit=$?
    CURRENT_LOAD_PID=""
    : >"$LOAD_PID_FILE"
    set -e

    if ! values="$(parse_load_report "$report_path")"; then
        LOAD_FAILURE=1
        return 1
    fi
    IFS=$'\t' read -r \
        started active peak completed cancelled errors <<<"$values"
    if [[ "$load_exit" -ne 0 ||
        "$started" -ne "$concurrency" ||
        "$active" -ne 0 ||
        "$peak" -ne "$concurrency" ||
        "$errors" -ne 0 ||
        $((completed + cancelled)) -ne "$concurrency" ]]; then
        LOAD_FAILURE=1
        return 1
    fi
    if [[ "$phase" == "standard" && "$cancelled" -ne 0 ]]; then
        LOAD_FAILURE=1
        return 1
    fi
    if [[ "$phase" == "cancellation" && "$cancelled" -eq 0 ]]; then
        LOAD_FAILURE=1
        return 1
    fi

    TOTAL_STARTED=$((TOTAL_STARTED + started))
    TOTAL_COMPLETED=$((TOTAL_COMPLETED + completed))
    TOTAL_CANCELLED=$((TOTAL_CANCELLED + cancelled))
    TOTAL_ERRORS=$((TOTAL_ERRORS + errors))
    if ((peak > MAX_PEAK_ACTIVE)); then
        MAX_PEAK_ACTIVE="$peak"
    fi
    if [[ "$phase" == "standard" ]]; then
        STANDARD_ROUNDS=$((STANDARD_ROUNDS + 1))
    else
        CANCELLATION_ROUNDS=$((CANCELLATION_ROUNDS + 1))
    fi
    return 0
}

process_is_alive() {
    local pid="$1"
    kill -0 "$pid" 2>/dev/null || return 1
    local state
    state="$(ps -o stat= -p "$pid" 2>/dev/null | tr -d '[:space:]')"
    [[ -n "$state" && "$state" != Z* ]]
}

wait_for_process_exit() {
    local pid="$1"
    local attempts="$2"
    local wait_attempt
    for ((wait_attempt = 0; wait_attempt < attempts; wait_attempt++)); do
        process_is_alive "$pid" || return 0
        sleep 0.1
    done
    return 1
}

port_is_closed() {
    local port="$1"
    python3 - "$port" <<'PY'
import socket
import sys

try:
    with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=0.2):
        raise SystemExit(1)
except OSError:
    raise SystemExit(0)
PY
}

write_final_report() {
    local passed="$1"
    local failure_csv="$2"
    local profile_elapsed="$3"
    local baseline_rss="$4"
    local peak_rss="$5"
    local recovery_rss="$6"
    local recovery_limit="$7"
    local baseline_fd="$8"
    local final_fd="$9"
    shift 9
    local baseline_tasks="$1"
    local final_tasks="$2"
    local temporary_report="$TEMPORARY_ROOT/provider-gates-report.json"
    local failure_argument="$failure_csv"
    [[ -n "$failure_argument" ]] || failure_argument="-"

    python3 - \
        "$temporary_report" \
        "$passed" \
        "$failure_argument" \
        "$PLATFORM" \
        "$PROFILE" \
        "$PROFILE_SECONDS" \
        "$profile_elapsed" \
        "$PROFILE_CONCURRENCY" \
        "$STANDARD_ROUNDS" \
        "$CANCELLATION_ROUNDS" \
        "$TOTAL_STARTED" \
        "$TOTAL_COMPLETED" \
        "$TOTAL_CANCELLED" \
        "$TOTAL_ERRORS" \
        "$MAX_PEAK_ACTIVE" \
        "$baseline_rss" \
        "$peak_rss" \
        "$recovery_rss" \
        "$recovery_limit" \
        "$baseline_fd" \
        "$final_fd" \
        "$baseline_tasks" \
        "$final_tasks" <<'PY'
import json
import os
import sys

(
    path,
    passed,
    failure_csv,
    platform,
    profile,
    requested_seconds,
    elapsed_seconds,
    concurrency,
    standard_rounds,
    cancellation_rounds,
    started,
    completed,
    cancelled,
    errors,
    peak_active,
    baseline_rss,
    peak_rss,
    recovery_rss,
    recovery_limit,
    baseline_fd,
    final_fd,
    baseline_tasks,
    final_tasks,
) = sys.argv[1:]
failures = (
    []
    if failure_csv == "-"
    else [value for value in failure_csv.split(",") if value]
)
report = {
    "schema_version": 1,
    "passed": passed == "true",
    "failures": failures,
    "platform": platform,
    "profile": profile,
    "offline_runtime": True,
    "network_loopback_only": "non_loopback_network" not in failures,
    "fixed_executables": [
        "wokcore",
        "wokcore-provider-sim",
        "wokcore-loadgen",
    ],
    "profile_requested_seconds": int(requested_seconds),
    "profile_elapsed_seconds": int(elapsed_seconds),
    "loads": {
        "configured_concurrency": int(concurrency),
        "standard_rounds": int(standard_rounds),
        "cancellation_rounds": int(cancellation_rounds),
        "started": int(started),
        "completed": int(completed),
        "cancelled": int(cancelled),
        "errors": int(errors),
        "peak_active": int(peak_active),
    },
    "resources": {
        "baseline_rss_kib": int(baseline_rss),
        "peak_rss_kib": int(peak_rss),
        "recovery_rss_kib": int(recovery_rss),
        "recovery_limit_kib": int(recovery_limit),
        "baseline_fd_count": int(baseline_fd),
        "final_fd_count": int(final_fd),
        "baseline_task_count": int(baseline_tasks),
        "final_task_count": int(final_tasks),
    },
}
encoded = json.dumps(report, separators=(",", ":"), sort_keys=True)
if not encoded or len(encoded.encode("utf-8")) >= 131072:
    raise SystemExit(1)
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
    if forbidden in encoded.lower():
        raise SystemExit(1)
with open(path, "w", encoding="utf-8", newline="\n") as handle:
    handle.write(encoded)
    handle.write("\n")
os.chmod(path, 0o600)
PY
    install -m 600 "$temporary_report" "$REPORT_PATH"
    [[ "$(wc -c <"$REPORT_PATH" | tr -d '[:space:]')" -lt 131072 ]] ||
        fail "portable provider gate evidence exceeded its bound"
}

run_profile() {
    run_load_phase standard 64 0 ||
        fail "portable provider gate warmup failed"
    sleep 5
    local baseline
    baseline="$(resource_snapshot "$WOKCORE_PID")" ||
        fail "portable baseline resource sampling failed"
    local baseline_rss
    local baseline_fd
    local baseline_tasks
    IFS=$'\t' read -r baseline_rss baseline_fd baseline_tasks <<<"$baseline"
    : >"$RESOURCE_SAMPLES_FILE"
    TOTAL_STARTED=0
    TOTAL_COMPLETED=0
    TOTAL_CANCELLED=0
    TOTAL_ERRORS=0
    MAX_PEAK_ACTIVE=0
    STANDARD_ROUNDS=0
    CANCELLATION_ROUNDS=0

    local profile_started
    local deadline
    local now
    local remaining
    profile_started="$(date +%s)"
    deadline=$((profile_started + PROFILE_SECONDS))
    while :; do
        now="$(date +%s)"
        remaining=$((deadline - now))
        ((remaining >= 12)) || break
        if ! run_load_phase standard "$PROFILE_CONCURRENCY" 0; then
            break
        fi
        if ! run_load_phase cancellation "$PROFILE_CONCURRENCY" 2500; then
            break
        fi
    done
    now="$(date +%s)"
    remaining=$((deadline - now))
    if ((remaining > 0)); then
        sleep "$remaining"
    fi
    CLIENT_TOKEN=""
    local profile_elapsed
    profile_elapsed=$(( $(date +%s) - profile_started ))

    sleep "$RECOVERY_SECONDS"
    local recovery
    recovery="$(resource_snapshot "$WOKCORE_PID")" || {
        : >"$PROCESS_VIOLATION_FILE"
        recovery="0"$'\t'"0"$'\t'"0"
    }
    local recovery_rss
    local final_fd
    local final_tasks
    IFS=$'\t' read -r recovery_rss final_fd final_tasks <<<"$recovery"
    local peak_rss
    peak_rss="$(
        awk -F '\t' '
            $2 ~ /^[0-9]+$/ && $2 > maximum { maximum = $2 }
            END { print maximum + 0 }
        ' "$RESOURCE_SAMPLES_FILE"
    )"
    ((peak_rss >= baseline_rss)) || peak_rss="$baseline_rss"
    ((peak_rss >= recovery_rss)) || peak_rss="$recovery_rss"
    local doubled_baseline=$((baseline_rss * 2))
    local additive_limit=$((baseline_rss + 65536))
    local recovery_limit="$doubled_baseline"
    ((additive_limit > recovery_limit)) && recovery_limit="$additive_limit"

    : >"$MONITOR_STOP_FILE"
    wait "$MONITOR_PID" || true
    MONITOR_PID=""

    local failures=()
    [[ "$LOAD_FAILURE" -eq 0 ]] || failures+=("load_contract")
    [[ "$STANDARD_ROUNDS" -gt 0 && "$CANCELLATION_ROUNDS" -gt 0 ]] ||
        failures+=("profile_coverage")
    [[ "$profile_elapsed" -ge "$PROFILE_SECONDS" ]] ||
        failures+=("profile_duration")
    [[ ! -e "$PROCESS_VIOLATION_FILE" ]] ||
        failures+=("runtime_process")
    [[ ! -e "$NETWORK_VIOLATION_FILE" ]] ||
        failures+=("non_loopback_network")
    ((recovery_rss <= recovery_limit)) ||
        failures+=("recovery_rss")
    ((final_fd <= baseline_fd + 32)) ||
        failures+=("fd_growth")
    ((final_tasks <= baseline_tasks + 8)) ||
        failures+=("task_growth")

    "${RUNTIME_ENVIRONMENT[@]}" \
        "$WOKCORE_EXECUTABLE" stop --json >/dev/null 2>&1 || true
    if ! wait_for_process_exit "$WOKCORE_PID" 200; then
        failures+=("wokcore_shutdown")
        kill -TERM "$WOKCORE_PID" 2>/dev/null || true
        wait_for_process_exit "$WOKCORE_PID" 50 || {
            kill -KILL "$WOKCORE_PID" 2>/dev/null || true
            wait_for_process_exit "$WOKCORE_PID" 50 || true
        }
    fi
    wait "$WOKCORE_PID" 2>/dev/null || true
    WOKCORE_PID=""
    kill -TERM "$SIMULATOR_PID" 2>/dev/null || true
    if ! wait_for_process_exit "$SIMULATOR_PID" 100; then
        failures+=("simulator_shutdown")
        kill -KILL "$SIMULATOR_PID" 2>/dev/null || true
        wait_for_process_exit "$SIMULATOR_PID" 50 || true
    fi
    wait "$SIMULATOR_PID" 2>/dev/null || true
    SIMULATOR_PID=""
    port_is_closed "$CORE_PORT" || failures+=("core_listener")
    port_is_closed "$SIMULATOR_PORT" || failures+=("simulator_listener")

    local passed="true"
    (("${#failures[@]}" == 0)) || passed="false"
    local failure_csv=""
    if (("${#failures[@]}" > 0)); then
        failure_csv="$(IFS=,; printf '%s' "${failures[*]}")"
    fi
    write_final_report \
        "$passed" \
        "$failure_csv" \
        "$profile_elapsed" \
        "$baseline_rss" \
        "$peak_rss" \
        "$recovery_rss" \
        "$recovery_limit" \
        "$baseline_fd" \
        "$final_fd" \
        "$baseline_tasks" \
        "$final_tasks"

    if [[ "$passed" != "true" ]]; then
        fail "one or more portable provider gates failed"
    fi
    printf 'portable provider gates passed\n'
}

main() {
    parse_arguments "$@"
    configure_platform
    configure_paths
    build_fixed_executables

    if command -v pgrep >/dev/null 2>&1 &&
        pgrep -x wokcore >/dev/null 2>&1; then
        fail "another WokCore process is running"
    fi

    create_private_runtime
    isolate_environment
    write_config
    start_runtime
    authorize_load_generator
    run_profile
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main "$@"
fi
