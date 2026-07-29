#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
BUILD_PACKAGE="$SCRIPT_DIRECTORY/build-package.sh"
BUILD_MACOS_ASSETS="$SCRIPT_DIRECTORY/build-macos-assets.sh"
[[ -f "$BUILD_MACOS_ASSETS" ]] || {
    printf 'missing macOS asset builder: %s\n' "$BUILD_MACOS_ASSETS" >&2
    exit 1
}
if [[ "$#" -ne 2 ]]; then
    printf 'usage: %s TARGET PUBLIC_ARCH\n' "$0" >&2
    exit 2
fi
TARGET="$1"
PUBLIC_ARCH="$2"
case "$TARGET:$PUBLIC_ARCH" in
    x86_64-apple-darwin:x86_64)
        OTHER_TARGET=aarch64-apple-darwin
        ;;
    aarch64-apple-darwin:arm64)
        OTHER_TARGET=x86_64-apple-darwin
        ;;
    *)
        printf 'unsupported macOS fixture mapping: %s:%s\n' \
            "$TARGET" "$PUBLIC_ARCH" >&2
        exit 2
        ;;
esac
for command in zip unzip; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'missing macOS asset test command: %s\n' "$command" >&2
        exit 1
    }
done

TEST_ROOT="$(mktemp -d)"
trap 'rm -rf -- "$TEST_ROOT"' EXIT
if [[ -n "${WOKCORE_EXPECT_DARWIN_SYSTEM_PREFIX:-}" ]]; then
    SYSTEM_PREFIX="$WOKCORE_EXPECT_DARWIN_SYSTEM_PREFIX"
    EXPECTED_PHYSICAL_PREFIX="/private${SYSTEM_PREFIX}"
    [[ "$SYSTEM_PREFIX" == /var || "$SYSTEM_PREFIX" == /tmp ]] || {
        printf 'Darwin system-prefix fixture received an invalid prefix\n' >&2
        exit 1
    }
    [[ -L "$SYSTEM_PREFIX" ]] || {
        printf 'Darwin system-prefix fixture requires a symbolic prefix\n' >&2
        exit 1
    }
    [[ "$(cd "$SYSTEM_PREFIX" && pwd -P)" == "$EXPECTED_PHYSICAL_PREFIX" ]] || {
        printf 'Darwin system-prefix fixture has the wrong physical prefix\n' >&2
        exit 1
    }
    [[ "$TEST_ROOT" == "$SYSTEM_PREFIX"/* ]] || {
        printf 'Darwin system-prefix fixture allocated under the wrong prefix\n' >&2
        exit 1
    }
fi
FIXTURE_REPOSITORY="$TEST_ROOT/repository"
TECHNICAL_DIST="$TEST_ROOT/technical"
DIST="$TEST_ROOT/dist"
mkdir -p "$FIXTURE_REPOSITORY" "$TECHNICAL_DIST" "$DIST"

printf 'wokcore macOS %s fixture executable\n' "$PUBLIC_ARCH" >"$TEST_ROOT/wokcore"
printf 'Apache fixture\n' >"$FIXTURE_REPOSITORY/LICENSE-APACHE"
printf 'MIT fixture\n' >"$FIXTURE_REPOSITORY/LICENSE-MIT"
printf 'notice fixture\n' >"$FIXTURE_REPOSITORY/NOTICE.md"
printf 'readme fixture\n' >"$FIXTURE_REPOSITORY/README.md"
chmod 0755 "$TEST_ROOT/wokcore"

"$BUILD_PACKAGE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TECHNICAL_DIST" \
    --version 1.2.3 \
    --target "$TARGET"
TECHNICAL_ARCHIVE="$TECHNICAL_DIST/wokcore-v1.2.3-$TARGET.tar.gz"
FRIENDLY_ARCHIVE="$DIST/WokCore-v1.2.3-macOS-$PUBLIC_ARCH.tar.gz"
ZIP_PACKAGE="$DIST/WokCore-v1.2.3-macOS-$PUBLIC_ARCH.zip"

"$BUILD_MACOS_ASSETS" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$DIST" \
    --version 1.2.3 \
    --target "$TARGET"

expected_assets="WokCore-v1.2.3-macOS-$PUBLIC_ARCH.tar.gz
WokCore-v1.2.3-macOS-$PUBLIC_ARCH.zip"
test "$(
    cd "$DIST"
    printf '%s\n' * | LC_ALL=C sort
)" = "$expected_assets"
cmp "$TECHNICAL_ARCHIVE" "$FRIENDLY_ARCHIVE"
expected_entries=$'wokcore\nLICENSE-APACHE\nLICENSE-MIT\nNOTICE.md\nREADME.md'
test "$(unzip -Z -1 "$ZIP_PACKAGE")" = "$expected_entries"

ZIP_EXTRACT="$TEST_ROOT/zip-extract"
mkdir "$ZIP_EXTRACT"
unzip -q "$ZIP_PACKAGE" -d "$ZIP_EXTRACT"
cmp "$TEST_ROOT/wokcore" "$ZIP_EXTRACT/wokcore"
cmp "$FIXTURE_REPOSITORY/LICENSE-APACHE" "$ZIP_EXTRACT/LICENSE-APACHE"
cmp "$FIXTURE_REPOSITORY/LICENSE-MIT" "$ZIP_EXTRACT/LICENSE-MIT"
cmp "$FIXTURE_REPOSITORY/NOTICE.md" "$ZIP_EXTRACT/NOTICE.md"
cmp "$FIXTURE_REPOSITORY/README.md" "$ZIP_EXTRACT/README.md"
test -x "$ZIP_EXTRACT/wokcore"
test ! -x "$ZIP_EXTRACT/LICENSE-APACHE"

expect_failure() {
    local expected_error="$1"
    shift
    local error_file="$TEST_ROOT/rejected-error"
    if "$BUILD_MACOS_ASSETS" "$@" > /dev/null 2>"$error_file"; then
        printf 'macOS asset builder accepted an invalid fixture\n' >&2
        exit 1
    fi
    if ! grep -Fqx "$expected_error" "$error_file"; then
        printf 'expected macOS asset error: %s\n' "$expected_error" >&2
        printf 'actual macOS asset error:\n' >&2
        cat "$error_file" >&2
        exit 1
    fi
}

common_arguments=(
    --technical-archive "$TECHNICAL_ARCHIVE"
    --executable "$TEST_ROOT/wokcore"
    --repository-root "$FIXTURE_REPOSITORY"
    --output-directory "$TEST_ROOT/rejected"
    --version 1.2.3
    --target "$TARGET"
)
duplicate_arguments=(
    --technical-archive "$TECHNICAL_ARCHIVE"
    --executable "$TEST_ROOT/wokcore"
    --repository-root "$FIXTURE_REPOSITORY"
    --output-directory "$TEST_ROOT/rejected"
    --version 1.2.3
    --target "$TARGET"
)
for ((index = 0; index < ${#duplicate_arguments[@]}; index += 2)); do
    flag="${duplicate_arguments[$index]}"
    value="${duplicate_arguments[$((index + 1))]}"
    expect_failure \
        "wokcore macOS assets: duplicate $flag argument" \
        "${common_arguments[@]}" \
        "$flag" "$value"
done

expect_failure \
    "wokcore macOS assets: release version is not canonical SemVer" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2 \
    --target "$TARGET"
expect_failure \
    "Unsupported macOS target: aarch64-unknown-linux-gnu" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target aarch64-unknown-linux-gnu

cp "$TECHNICAL_ARCHIVE" "$TECHNICAL_DIST/wokcore-v1.2.3-$OTHER_TARGET.tar.gz"
expect_failure \
    "wokcore macOS assets: technical archive name does not match version and target" \
    --technical-archive "$TECHNICAL_DIST/wokcore-v1.2.3-$OTHER_TARGET.tar.gz" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target "$TARGET"

ln -s "$TECHNICAL_ARCHIVE" "$TEST_ROOT/technical-link.tar.gz"
expect_failure \
    "wokcore macOS assets: the technical archive is missing or symbolic" \
    --technical-archive "$TEST_ROOT/technical-link.tar.gz" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target "$TARGET"

ln -s "$TECHNICAL_DIST" "$TEST_ROOT/technical-ancestor-link"
expect_failure \
    "wokcore macOS assets: the technical archive is missing or symbolic" \
    --technical-archive "$TEST_ROOT/technical-ancestor-link/$(basename "$TECHNICAL_ARCHIVE")" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target "$TARGET"

mkdir "$TEST_ROOT/executable-real"
cp "$TEST_ROOT/wokcore" "$TEST_ROOT/executable-real/wokcore"
ln -s "$TEST_ROOT/executable-real" "$TEST_ROOT/executable-ancestor-link"
expect_failure \
    "wokcore macOS assets: the release executable is missing or symbolic" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/executable-ancestor-link/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target "$TARGET"

ln -s "$FIXTURE_REPOSITORY" "$TEST_ROOT/repository-ancestor-link"
expect_failure \
    "wokcore macOS assets: the repository root is missing or symbolic" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$TEST_ROOT/repository-ancestor-link/" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target "$TARGET"

mkdir "$TEST_ROOT/output-real"
ln -s "$TEST_ROOT/output-real" "$TEST_ROOT/output-ancestor-link"
expect_failure \
    "wokcore macOS assets: the output directory is missing or symbolic" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/output-ancestor-link/nested/" \
    --version 1.2.3 \
    --target "$TARGET"

cp "$TEST_ROOT/wokcore" "$TEST_ROOT/not-wokcore"
expect_failure \
    "wokcore macOS assets: the Unix release executable must use the fixed wokcore name" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/not-wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target "$TARGET"

mv "$FIXTURE_REPOSITORY/README.md" "$TEST_ROOT/README.md.real"
ln -s "$TEST_ROOT/README.md.real" "$FIXTURE_REPOSITORY/README.md"
expect_failure \
    "wokcore macOS assets: a release document is missing or symbolic" \
    "${common_arguments[@]}"

printf 'macOS asset builder tests passed\n'
