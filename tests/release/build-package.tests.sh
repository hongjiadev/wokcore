#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
BUILD_PACKAGE="$SCRIPT_DIRECTORY/build-package.sh"
[[ -f "$BUILD_PACKAGE" ]] || {
    printf 'missing release package builder: %s\n' "$BUILD_PACKAGE" >&2
    exit 1
}

TEST_ROOT="$(mktemp -d)"
trap 'rm -rf -- "$TEST_ROOT"' EXIT
REPOSITORY_ROOT="$TEST_ROOT/repository"
FIRST_OUTPUT="$TEST_ROOT/first"
SECOND_OUTPUT="$TEST_ROOT/second"
mkdir -p "$REPOSITORY_ROOT" "$FIRST_OUTPUT" "$SECOND_OUTPUT"

printf 'wokcore fixture executable\n' >"$TEST_ROOT/wokcore"
printf 'MIT fixture\n' >"$REPOSITORY_ROOT/LICENSE-MIT"
printf 'Apache fixture\n' >"$REPOSITORY_ROOT/LICENSE-APACHE"
printf 'notice fixture\n' >"$REPOSITORY_ROOT/NOTICE.md"
printf 'readme fixture\n' >"$REPOSITORY_ROOT/README.md"

targets=(
    x86_64-apple-darwin
    aarch64-apple-darwin
    x86_64-unknown-linux-gnu
    aarch64-unknown-linux-gnu
)
expected_entries=$'wokcore\nLICENSE-APACHE\nLICENSE-MIT\nNOTICE.md\nREADME.md'

for target in "${targets[@]}"; do
    chmod 755 "$TEST_ROOT/wokcore"
    chmod 644 \
        "$REPOSITORY_ROOT/LICENSE-APACHE" \
        "$REPOSITORY_ROOT/LICENSE-MIT" \
        "$REPOSITORY_ROOT/NOTICE.md" \
        "$REPOSITORY_ROOT/README.md"
    touch -t 202001020304.05 \
        "$TEST_ROOT/wokcore" \
        "$REPOSITORY_ROOT/LICENSE-APACHE" \
        "$REPOSITORY_ROOT/LICENSE-MIT" \
        "$REPOSITORY_ROOT/NOTICE.md" \
        "$REPOSITORY_ROOT/README.md"
    "$BUILD_PACKAGE" \
        --executable "$TEST_ROOT/wokcore" \
        --repository-root "$REPOSITORY_ROOT" \
        --output-directory "$FIRST_OUTPUT" \
        --version 1.2.3 \
        --target "$target"
    chmod 700 "$TEST_ROOT/wokcore"
    chmod 600 \
        "$REPOSITORY_ROOT/LICENSE-APACHE" \
        "$REPOSITORY_ROOT/LICENSE-MIT" \
        "$REPOSITORY_ROOT/NOTICE.md" \
        "$REPOSITORY_ROOT/README.md"
    touch -t 203001020304.05 \
        "$TEST_ROOT/wokcore" \
        "$REPOSITORY_ROOT/LICENSE-APACHE" \
        "$REPOSITORY_ROOT/LICENSE-MIT" \
        "$REPOSITORY_ROOT/NOTICE.md" \
        "$REPOSITORY_ROOT/README.md"
    "$BUILD_PACKAGE" \
        --executable "$TEST_ROOT/wokcore" \
        --repository-root "$REPOSITORY_ROOT" \
        --output-directory "$SECOND_OUTPUT" \
        --version 1.2.3 \
        --target "$target"

    archive="wokcore-v1.2.3-$target.tar.gz"
    cmp "$FIRST_OUTPUT/$archive" "$SECOND_OUTPUT/$archive"
    python3 - "$FIRST_OUTPUT/$archive" <<'PY'
import gzip
import sys
import tarfile

path = sys.argv[1]
with open(path, "rb") as handle:
    header = handle.read(10)
if len(header) != 10 or header[:3] != b"\x1f\x8b\x08":
    raise SystemExit("release package does not have a canonical gzip header")
if header[3] != 0 or header[4:8] != b"\0\0\0\0":
    raise SystemExit("release package retained gzip flags or source time")

expected = (
    ("wokcore", 0o755),
    ("LICENSE-APACHE", 0o644),
    ("LICENSE-MIT", 0o644),
    ("NOTICE.md", 0o644),
    ("README.md", 0o644),
)
with tarfile.open(path, "r:gz") as archive:
    members = archive.getmembers()
if tuple((item.name, item.mode) for item in members) != expected:
    raise SystemExit("release package has non-canonical names or modes")
for item in members:
    if (
        not item.isfile()
        or item.mtime != 0
        or item.uid != 0
        or item.gid != 0
        or item.uname
        or item.gname
    ):
        raise SystemExit("release package has non-canonical tar metadata")
PY
    [[ "$(tar -tzf "$FIRST_OUTPUT/$archive")" == "$expected_entries" ]]
    if tar -tzf "$FIRST_OUTPUT/$archive" |
        grep -E '(^/|(^|/)\.\.(/|$)|wokcore-provider-sim|wokcore-loadgen)'; then
        printf 'release archive retained a forbidden entry\n' >&2
        exit 1
    fi
    if grep -aF "$TEST_ROOT" "$FIRST_OUTPUT/$archive"; then
        printf 'release archive retained a local absolute path\n' >&2
        exit 1
    fi
done

if "$BUILD_PACKAGE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$REPOSITORY_ROOT" \
    --output-directory "$FIRST_OUTPUT" \
    --version invalid \
    --target x86_64-unknown-linux-gnu; then
    printf 'invalid release version was accepted\n' >&2
    exit 1
fi

printf 'release package shell tests passed\n'
