#!/usr/bin/env bash
set -Eeuo pipefail

EXECUTABLE=""
REPOSITORY_ROOT=""
OUTPUT_DIRECTORY=""
VERSION=""
TARGET=""

fail() {
    printf 'wokcore release package: %s\n' "$1" >&2
    exit 1
}

while [[ "$#" -gt 0 ]]; do
    case "$1" in
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
    x86_64-apple-darwin | aarch64-apple-darwin | \
        x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu) ;;
    *) fail "release target is unsupported" ;;
esac

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
command -v python3 >/dev/null 2>&1 || fail "python3 is required"

mkdir -p "$OUTPUT_DIRECTORY"
archive_name="wokcore-v$VERSION-$TARGET.tar.gz"
archive_path="$OUTPUT_DIRECTORY/$archive_name"
temporary_path="$OUTPUT_DIRECTORY/.$archive_name.$$.tmp"

cleanup() {
    rm -f -- "$temporary_path"
}
trap cleanup EXIT

python3 - \
    "$temporary_path" \
    "$EXECUTABLE" \
    "$REPOSITORY_ROOT/LICENSE-APACHE" \
    "$REPOSITORY_ROOT/LICENSE-MIT" \
    "$REPOSITORY_ROOT/NOTICE.md" \
    "$REPOSITORY_ROOT/README.md" <<'PY'
import gzip
import os
import sys
import tarfile

output = sys.argv[1]
entries = (
    ("wokcore", sys.argv[2], 0o755),
    ("LICENSE-APACHE", sys.argv[3], 0o644),
    ("LICENSE-MIT", sys.argv[4], 0o644),
    ("NOTICE.md", sys.argv[5], 0o644),
    ("README.md", sys.argv[6], 0o644),
)

with open(output, "xb") as raw:
    with gzip.GzipFile(
        filename="",
        mode="wb",
        compresslevel=9,
        fileobj=raw,
        mtime=0,
    ) as compressed:
        with tarfile.open(
            fileobj=compressed,
            mode="w",
            format=tarfile.USTAR_FORMAT,
        ) as archive:
            for name, source, mode in entries:
                size = os.path.getsize(source)
                info = tarfile.TarInfo(name)
                info.size = size
                info.mode = mode
                info.uid = 0
                info.gid = 0
                info.uname = ""
                info.gname = ""
                info.mtime = 0
                with open(source, "rb") as handle:
                    archive.addfile(info, handle)
os.chmod(output, 0o644)
PY

mv -f -- "$temporary_path" "$archive_path"
trap - EXIT
printf '%s\n' "$archive_path"
