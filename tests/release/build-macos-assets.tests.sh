#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
BUILD_PACKAGE="$SCRIPT_DIRECTORY/build-package.sh"
BUILD_MACOS_ASSETS="$SCRIPT_DIRECTORY/build-macos-assets.sh"
[[ -f "$BUILD_MACOS_ASSETS" ]] || {
    printf 'missing macOS asset builder: %s\n' "$BUILD_MACOS_ASSETS" >&2
    exit 1
}
for command in zip unzip; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'missing macOS asset test command: %s\n' "$command" >&2
        exit 1
    }
done

TEST_ROOT="$(mktemp -d)"
trap 'rm -rf -- "$TEST_ROOT"' EXIT
FIXTURE_REPOSITORY="$TEST_ROOT/repository"
TECHNICAL_DIST="$TEST_ROOT/technical"
DIST="$TEST_ROOT/dist"
mkdir -p "$FIXTURE_REPOSITORY" "$TECHNICAL_DIST" "$DIST"

printf 'wokcore macOS fixture executable\n' >"$TEST_ROOT/wokcore"
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
    --target aarch64-apple-darwin
TECHNICAL_ARCHIVE="$TECHNICAL_DIST/wokcore-v1.2.3-aarch64-apple-darwin.tar.gz"

"$BUILD_MACOS_ASSETS" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$DIST" \
    --version 1.2.3 \
    --target aarch64-apple-darwin

test -f "$DIST/WokCore-v1.2.3-macOS-arm64.tar.gz"
test -f "$DIST/WokCore-v1.2.3-macOS-arm64.zip"
cmp "$TECHNICAL_ARCHIVE" "$DIST/WokCore-v1.2.3-macOS-arm64.tar.gz"
expected_entries=$'wokcore\nLICENSE-APACHE\nLICENSE-MIT\nNOTICE.md\nREADME.md'
test "$(unzip -Z -1 "$DIST/WokCore-v1.2.3-macOS-arm64.zip")" = "$expected_entries"

ZIP_EXTRACT="$TEST_ROOT/zip-extract"
mkdir "$ZIP_EXTRACT"
unzip -q "$DIST/WokCore-v1.2.3-macOS-arm64.zip" -d "$ZIP_EXTRACT"
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
    grep -Fqx "$expected_error" "$error_file"
}

common_arguments=(
    --technical-archive "$TECHNICAL_ARCHIVE"
    --executable "$TEST_ROOT/wokcore"
    --repository-root "$FIXTURE_REPOSITORY"
    --output-directory "$TEST_ROOT/rejected"
    --version 1.2.3
    --target aarch64-apple-darwin
)
expect_failure \
    "wokcore macOS assets: release version is not canonical SemVer" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2 \
    --target aarch64-apple-darwin
expect_failure \
    "Unsupported macOS target: aarch64-unknown-linux-gnu" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target aarch64-unknown-linux-gnu

cp "$TECHNICAL_ARCHIVE" "$TECHNICAL_DIST/wokcore-v1.2.3-x86_64-apple-darwin.tar.gz"
expect_failure \
    "wokcore macOS assets: technical archive name does not match version and target" \
    --technical-archive "$TECHNICAL_DIST/wokcore-v1.2.3-x86_64-apple-darwin.tar.gz" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target aarch64-apple-darwin

ln -s "$TECHNICAL_ARCHIVE" "$TEST_ROOT/technical-link.tar.gz"
expect_failure \
    "wokcore macOS assets: the technical archive is missing or symbolic" \
    --technical-archive "$TEST_ROOT/technical-link.tar.gz" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target aarch64-apple-darwin

cp "$TEST_ROOT/wokcore" "$TEST_ROOT/not-wokcore"
expect_failure \
    "wokcore macOS assets: the Unix release executable must use the fixed wokcore name" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/not-wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target aarch64-apple-darwin

mv "$FIXTURE_REPOSITORY/README.md" "$TEST_ROOT/README.md.real"
ln -s "$TEST_ROOT/README.md.real" "$FIXTURE_REPOSITORY/README.md"
expect_failure \
    "wokcore macOS assets: a release document is missing or symbolic" \
    "${common_arguments[@]}"

printf 'macOS asset builder tests passed\n'
