#!/usr/bin/env bash
set -Eeuo pipefail

TECHNICAL_ARCHIVE=""
TECHNICAL_ARCHIVE_SET=false
EXECUTABLE=""
EXECUTABLE_SET=false
REPOSITORY_ROOT=""
REPOSITORY_ROOT_SET=false
OUTPUT_DIRECTORY=""
OUTPUT_DIRECTORY_SET=false
VERSION=""
VERSION_SET=false
TARGET=""
TARGET_SET=false

fail() {
    printf 'wokcore macOS assets: %s\n' "$1" >&2
    exit 1
}

path_has_symlink() {
    local candidate="$1"
    local parent
    case "$candidate" in
        /*) ;;
        *) candidate="$PWD/$candidate" ;;
    esac
    while [[ "$candidate" != "/" && "$candidate" == */ ]]; do
        candidate="${candidate%/}"
    done
    while true; do
        [[ ! -L "$candidate" ]] || return 0
        [[ "$candidate" != "/" ]] || return 1
        parent="${candidate%/*}"
        [[ -n "$parent" ]] || parent="/"
        [[ "$parent" != "$candidate" ]] || return 1
        candidate="$parent"
    done
}

while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --technical-archive)
            [[ "$TECHNICAL_ARCHIVE_SET" == false ]] ||
                fail "duplicate --technical-archive argument"
            [[ "$#" -ge 2 ]] || fail "technical archive value is missing"
            TECHNICAL_ARCHIVE="$2"
            TECHNICAL_ARCHIVE_SET=true
            shift 2
            ;;
        --executable)
            [[ "$EXECUTABLE_SET" == false ]] ||
                fail "duplicate --executable argument"
            [[ "$#" -ge 2 ]] || fail "executable value is missing"
            EXECUTABLE="$2"
            EXECUTABLE_SET=true
            shift 2
            ;;
        --repository-root)
            [[ "$REPOSITORY_ROOT_SET" == false ]] ||
                fail "duplicate --repository-root argument"
            [[ "$#" -ge 2 ]] || fail "repository root value is missing"
            REPOSITORY_ROOT="$2"
            REPOSITORY_ROOT_SET=true
            shift 2
            ;;
        --output-directory)
            [[ "$OUTPUT_DIRECTORY_SET" == false ]] ||
                fail "duplicate --output-directory argument"
            [[ "$#" -ge 2 ]] || fail "output directory value is missing"
            OUTPUT_DIRECTORY="$2"
            OUTPUT_DIRECTORY_SET=true
            shift 2
            ;;
        --version)
            [[ "$VERSION_SET" == false ]] ||
                fail "duplicate --version argument"
            [[ "$#" -ge 2 ]] || fail "version value is missing"
            VERSION="$2"
            VERSION_SET=true
            shift 2
            ;;
        --target)
            [[ "$TARGET_SET" == false ]] ||
                fail "duplicate --target argument"
            [[ "$#" -ge 2 ]] || fail "target value is missing"
            TARGET="$2"
            TARGET_SET=true
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

if path_has_symlink "$TECHNICAL_ARCHIVE"; then
    fail "the technical archive is missing or symbolic"
fi
[[ -f "$TECHNICAL_ARCHIVE" && ! -L "$TECHNICAL_ARCHIVE" ]] ||
    fail "the technical archive is missing or symbolic"
EXPECTED_ARCHIVE="wokcore-v$VERSION-$TARGET.tar.gz"
[[ "$(basename "$TECHNICAL_ARCHIVE")" == "$EXPECTED_ARCHIVE" ]] ||
    fail "technical archive name does not match version and target"
if path_has_symlink "$EXECUTABLE"; then
    fail "the release executable is missing or symbolic"
fi
[[ -f "$EXECUTABLE" && ! -L "$EXECUTABLE" ]] ||
    fail "the release executable is missing or symbolic"
[[ "$(basename "$EXECUTABLE")" == "wokcore" ]] ||
    fail "the Unix release executable must use the fixed wokcore name"
if path_has_symlink "$REPOSITORY_ROOT"; then
    fail "the repository root is missing or symbolic"
fi
[[ -d "$REPOSITORY_ROOT" && ! -L "$REPOSITORY_ROOT" ]] ||
    fail "the repository root is missing or symbolic"
for name in LICENSE-APACHE LICENSE-MIT NOTICE.md README.md; do
    if path_has_symlink "$REPOSITORY_ROOT/$name"; then
        fail "a release document is missing or symbolic"
    fi
    [[ -f "$REPOSITORY_ROOT/$name" && ! -L "$REPOSITORY_ROOT/$name" ]] ||
        fail "a release document is missing or symbolic"
done
[[ -n "$OUTPUT_DIRECTORY" ]] || fail "output directory is required"
if path_has_symlink "$OUTPUT_DIRECTORY"; then
    fail "the output directory is missing or symbolic"
fi
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
    if path_has_symlink "$destination"; then
        fail "an output destination is symbolic"
    fi
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
