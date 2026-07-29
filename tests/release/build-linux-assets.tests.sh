#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
BUILD_PACKAGE="$SCRIPT_DIRECTORY/build-package.sh"
BUILD_LINUX_ASSETS="$SCRIPT_DIRECTORY/build-linux-assets.sh"
[[ -f "$BUILD_LINUX_ASSETS" ]] || {
    printf 'missing Linux asset builder: %s\n' "$BUILD_LINUX_ASSETS" >&2
    exit 1
}
for command in dpkg-deb rpm rpmbuild; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'missing Linux package test command: %s\n' "$command" >&2
        exit 1
    }
done

TEST_ROOT="$(mktemp -d)"
trap 'rm -rf -- "$TEST_ROOT"' EXIT
FIXTURE_REPOSITORY="$TEST_ROOT/repository"
TECHNICAL_DIST="$TEST_ROOT/technical"
DIST="$TEST_ROOT/dist"
mkdir -p "$FIXTURE_REPOSITORY" "$TECHNICAL_DIST" "$DIST"

printf 'wokcore Linux fixture executable\n' >"$TEST_ROOT/wokcore"
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
    --target x86_64-unknown-linux-gnu
TECHNICAL_ARCHIVE="$TECHNICAL_DIST/wokcore-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"

"$BUILD_LINUX_ASSETS" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$DIST" \
    --version 1.2.3 \
    --target x86_64-unknown-linux-gnu

test -f "$DIST/WokCore-v1.2.3-Linux-x86_64.tar.gz"
test -f "$DIST/WokCore-v1.2.3-Linux-x86_64.deb"
test -f "$DIST/WokCore-v1.2.3-Linux-x86_64.rpm"
cmp "$TECHNICAL_ARCHIVE" "$DIST/WokCore-v1.2.3-Linux-x86_64.tar.gz"
test "$(dpkg-deb --field "$DIST/WokCore-v1.2.3-Linux-x86_64.deb" Package)" = wokcore
test "$(dpkg-deb --field "$DIST/WokCore-v1.2.3-Linux-x86_64.deb" Version)" = 1.2.3
test "$(dpkg-deb --field "$DIST/WokCore-v1.2.3-Linux-x86_64.deb" Architecture)" = amd64
test "$(rpm -qp --queryformat '%{NAME}' "$DIST/WokCore-v1.2.3-Linux-x86_64.rpm")" = wokcore
test "$(rpm -qp --queryformat '%{VERSION}' "$DIST/WokCore-v1.2.3-Linux-x86_64.rpm")" = 1.2.3
test "$(rpm -qp --queryformat '%{ARCH}' "$DIST/WokCore-v1.2.3-Linux-x86_64.rpm")" = x86_64

DEB_EXTRACT="$TEST_ROOT/deb-extract"
mkdir "$DEB_EXTRACT"
dpkg-deb --extract "$DIST/WokCore-v1.2.3-Linux-x86_64.deb" "$DEB_EXTRACT"
cmp "$TEST_ROOT/wokcore" "$DEB_EXTRACT/usr/bin/wokcore"
cmp "$FIXTURE_REPOSITORY/LICENSE-APACHE" \
    "$DEB_EXTRACT/usr/share/doc/wokcore/LICENSE-APACHE"
cmp "$FIXTURE_REPOSITORY/LICENSE-MIT" \
    "$DEB_EXTRACT/usr/share/doc/wokcore/LICENSE-MIT"
cmp "$FIXTURE_REPOSITORY/NOTICE.md" \
    "$DEB_EXTRACT/usr/share/doc/wokcore/NOTICE.md"
cmp "$FIXTURE_REPOSITORY/README.md" \
    "$DEB_EXTRACT/usr/share/doc/wokcore/README.md"
test -x "$DEB_EXTRACT/usr/bin/wokcore"
test ! -x "$DEB_EXTRACT/usr/share/doc/wokcore/LICENSE-APACHE"

expected_rpm_files=$'/usr/bin/wokcore\n/usr/share/doc/wokcore/LICENSE-APACHE\n/usr/share/doc/wokcore/LICENSE-MIT\n/usr/share/doc/wokcore/NOTICE.md\n/usr/share/doc/wokcore/README.md'
test "$(rpm -qpl "$DIST/WokCore-v1.2.3-Linux-x86_64.rpm")" = "$expected_rpm_files"

expect_failure() {
    local expected_error="$1"
    shift
    local error_file="$TEST_ROOT/rejected-error"
    if "$BUILD_LINUX_ASSETS" "$@" > /dev/null 2>"$error_file"; then
        printf 'Linux asset builder accepted an invalid fixture\n' >&2
        exit 1
    fi
    if ! grep -Fqx "$expected_error" "$error_file"; then
        printf 'expected Linux asset error: %s\n' "$expected_error" >&2
        printf 'actual Linux asset error:\n' >&2
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
    --target x86_64-unknown-linux-gnu
)
expect_failure \
    "wokcore Linux assets: release version is not canonical SemVer" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 01.2.3 \
    --target x86_64-unknown-linux-gnu

PRERELEASE_ARCHIVE="$TECHNICAL_DIST/wokcore-v1.2.3-alpha.1+build.5-x86_64-unknown-linux-gnu.tar.gz"
cp "$TECHNICAL_ARCHIVE" "$PRERELEASE_ARCHIVE"
expect_failure \
    "wokcore Linux assets: native packages require a stable x.y.z version" \
    --technical-archive "$PRERELEASE_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected-prerelease" \
    --version 1.2.3-alpha.1+build.5 \
    --target x86_64-unknown-linux-gnu
test ! -e "$TEST_ROOT/rejected-prerelease"

expect_failure \
    "Unsupported Linux target: x86_64-unknown-linux-musl" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target x86_64-unknown-linux-musl

cp "$TECHNICAL_ARCHIVE" "$TECHNICAL_DIST/wokcore-v1.2.3-aarch64-unknown-linux-gnu.tar.gz"
expect_failure \
    "wokcore Linux assets: technical archive name does not match version and target" \
    --technical-archive "$TECHNICAL_DIST/wokcore-v1.2.3-aarch64-unknown-linux-gnu.tar.gz" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target x86_64-unknown-linux-gnu

ln -s "$TECHNICAL_ARCHIVE" "$TEST_ROOT/technical-link.tar.gz"
expect_failure \
    "wokcore Linux assets: the technical archive is missing or symbolic" \
    --technical-archive "$TEST_ROOT/technical-link.tar.gz" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target x86_64-unknown-linux-gnu

cp "$TEST_ROOT/wokcore" "$TEST_ROOT/not-wokcore"
expect_failure \
    "wokcore Linux assets: the Unix release executable must use the fixed wokcore name" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/not-wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target x86_64-unknown-linux-gnu

mv "$FIXTURE_REPOSITORY/LICENSE-MIT" "$TEST_ROOT/LICENSE-MIT.real"
ln -s "$TEST_ROOT/LICENSE-MIT.real" "$FIXTURE_REPOSITORY/LICENSE-MIT"
expect_failure \
    "wokcore Linux assets: a release document is missing or symbolic" \
    "${common_arguments[@]}"

printf 'Linux asset builder tests passed\n'
