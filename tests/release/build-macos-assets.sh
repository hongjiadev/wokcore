#!/usr/bin/env bash
set -Eeuo pipefail

TECHNICAL_ARCHIVE=""
EXECUTABLE=""
REPOSITORY_ROOT=""
OUTPUT_DIRECTORY=""
VERSION=""
TARGET=""

fail() {
    printf 'wokcore macOS assets: %s\n' "$1" >&2
    exit 1
}

while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --technical-archive)
            [[ "$#" -ge 2 ]] || fail "technical archive value is missing"
            TECHNICAL_ARCHIVE="$2"
            shift 2
            ;;
        --executable)
            [[ "$#" -ge 2 ]] || fail "executable value is missing"
            EXECUTABLE="$2"
            shift 2
            ;;
        --repository-root)
            [[ "$#" -ge 2 ]] || fail "repository root value is missing"
            REPOSITORY_ROOT="$2"
            shift 2
            ;;
        --output-directory)
            [[ "$#" -ge 2 ]] || fail "output directory value is missing"
            OUTPUT_DIRECTORY="$2"
            shift 2
            ;;
        --version)
            [[ "$#" -ge 2 ]] || fail "version value is missing"
            VERSION="$2"
            shift 2
            ;;
        --target)
            [[ "$#" -ge 2 ]] || fail "target value is missing"
            TARGET="$2"
            shift 2
            ;;
        *)
            fail "unknown argument"
            ;;
    esac
done

[[ "$VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$ ]] ||
    fail "release version is not canonical SemVer"
case "$TARGET" in
    x86_64-apple-darwin) PUBLIC_ARCH=x86_64 ;;
    aarch64-apple-darwin) PUBLIC_ARCH=arm64 ;;
    *)
        printf 'Unsupported macOS target: %s\n' "$TARGET" >&2
        exit 2
        ;;
esac

[[ -f "$TECHNICAL_ARCHIVE" && ! -L "$TECHNICAL_ARCHIVE" ]] ||
    fail "the technical archive is missing or symbolic"
EXPECTED_ARCHIVE="wokcore-v$VERSION-$TARGET.tar.gz"
[[ "$(basename "$TECHNICAL_ARCHIVE")" == "$EXPECTED_ARCHIVE" ]] ||
    fail "technical archive name does not match version and target"
[[ -f "$EXECUTABLE" && ! -L "$EXECUTABLE" ]] ||
    fail "the release executable is missing or symbolic"
[[ "$(basename "$EXECUTABLE")" == "wokcore" ]] ||
    fail "the Unix release executable must use the fixed wokcore name"
[[ -d "$REPOSITORY_ROOT" && ! -L "$REPOSITORY_ROOT" ]] ||
    fail "the repository root is missing or symbolic"
for name in LICENSE-APACHE LICENSE-MIT NOTICE.md README.md; do
    [[ -f "$REPOSITORY_ROOT/$name" && ! -L "$REPOSITORY_ROOT/$name" ]] ||
        fail "a release document is missing or symbolic"
done
[[ -n "$OUTPUT_DIRECTORY" ]] || fail "output directory is required"
for command in cp install unzip zip; do
    command -v "$command" >/dev/null 2>&1 ||
        fail "$command is required"
done

mkdir -p "$OUTPUT_DIRECTORY"
[[ -d "$OUTPUT_DIRECTORY" && ! -L "$OUTPUT_DIRECTORY" ]] ||
    fail "the output directory is not a regular directory"
FRIENDLY_PREFIX="WokCore-v${VERSION}-macOS-${PUBLIC_ARCH}"
FRIENDLY_ARCHIVE="$OUTPUT_DIRECTORY/${FRIENDLY_PREFIX}.tar.gz"
ZIP_PACKAGE="$OUTPUT_DIRECTORY/${FRIENDLY_PREFIX}.zip"
for destination in "$FRIENDLY_ARCHIVE" "$ZIP_PACKAGE"; do
    [[ ! -L "$destination" ]] ||
        fail "an output destination is symbolic"
done

WORK_ROOT="$(mktemp -d)"
cleanup() {
    rm -rf -- "$WORK_ROOT"
}
trap cleanup EXIT
STAGE="$WORK_ROOT/stage"
VERIFY_DIRECTORY="$WORK_ROOT/verify"
mkdir -p "$STAGE" "$VERIFY_DIRECTORY"

cp -- "$TECHNICAL_ARCHIVE" "$FRIENDLY_ARCHIVE"
install -m 0755 "$EXECUTABLE" "$STAGE/wokcore"
install -m 0644 "$REPOSITORY_ROOT/LICENSE-APACHE" "$STAGE/LICENSE-APACHE"
install -m 0644 "$REPOSITORY_ROOT/LICENSE-MIT" "$STAGE/LICENSE-MIT"
install -m 0644 "$REPOSITORY_ROOT/NOTICE.md" "$STAGE/NOTICE.md"
install -m 0644 "$REPOSITORY_ROOT/README.md" "$STAGE/README.md"
touch -t 198001010000.00 "$STAGE"/*
zip -X -9 -j "$WORK_ROOT/${FRIENDLY_PREFIX}.zip" \
    "$STAGE/wokcore" \
    "$STAGE/LICENSE-APACHE" \
    "$STAGE/LICENSE-MIT" \
    "$STAGE/NOTICE.md" \
    "$STAGE/README.md"
unzip -q "$WORK_ROOT/${FRIENDLY_PREFIX}.zip" -d "$VERIFY_DIRECTORY"
[[ -x "$VERIFY_DIRECTORY/wokcore" ]] ||
    fail "the macOS ZIP did not preserve the executable mode"
install -m 0644 "$WORK_ROOT/${FRIENDLY_PREFIX}.zip" "$ZIP_PACKAGE"

trap - EXIT
rm -rf -- "$WORK_ROOT"
printf '%s\n%s\n' "$FRIENDLY_ARCHIVE" "$ZIP_PACKAGE"
