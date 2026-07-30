#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
BUILD_PACKAGE="$SCRIPT_DIRECTORY/build-package.sh"
BUILD_LINUX_ASSETS="$SCRIPT_DIRECTORY/build-linux-assets.sh"
CORRUPT_DPKG_DEB_DIRECTORY="$SCRIPT_DIRECTORY/fixtures/corrupt-dpkg-deb"
CORRUPT_RPMBUILD_DIRECTORY="$SCRIPT_DIRECTORY/fixtures/corrupt-rpmbuild"
[[ -f "$BUILD_LINUX_ASSETS" ]] || {
    printf 'missing Linux asset builder: %s\n' "$BUILD_LINUX_ASSETS" >&2
    exit 1
}
if ! grep -Fq 'rpm -qpl "$RPM_PACKAGE" 2>/dev/null | LC_ALL=C sort' \
    "$BUILD_LINUX_ASSETS"; then
    printf 'RPM file-list validation must normalize rpm query order\n' >&2
    exit 1
fi
if ! grep -Fq 'RPM_ALLOWED_DIRECTORIES=' "$BUILD_LINUX_ASSETS"; then
    printf 'RPM file-list validation must normalize implicit directories\n' >&2
    exit 1
fi
if [[ "$#" -ne 2 ]]; then
    printf 'usage: %s TARGET PUBLIC_ARCH\n' "$0" >&2
    exit 2
fi
TARGET="$1"
PUBLIC_ARCH="$2"
case "$TARGET:$PUBLIC_ARCH" in
    x86_64-unknown-linux-gnu:x86_64)
        DEB_ARCH=amd64
        RPM_ARCH=x86_64
        OTHER_TARGET=aarch64-unknown-linux-gnu
        ;;
    aarch64-unknown-linux-gnu:arm64)
        DEB_ARCH=arm64
        RPM_ARCH=aarch64
        OTHER_TARGET=x86_64-unknown-linux-gnu
        ;;
    *)
        printf 'unsupported Linux fixture mapping: %s:%s\n' \
            "$TARGET" "$PUBLIC_ARCH" >&2
        exit 2
        ;;
esac
for command in cpio dpkg-deb rpm rpm2cpio rpmbuild; do
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

printf 'wokcore Linux %s fixture executable\n' "$PUBLIC_ARCH" >"$TEST_ROOT/wokcore"
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
FRIENDLY_ARCHIVE="$DIST/WokCore-v1.2.3-Linux-$PUBLIC_ARCH.tar.gz"
DEB_PACKAGE="$DIST/WokCore-v1.2.3-Linux-$PUBLIC_ARCH.deb"
RPM_PACKAGE="$DIST/WokCore-v1.2.3-Linux-$PUBLIC_ARCH.rpm"

"$BUILD_LINUX_ASSETS" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$DIST" \
    --version 1.2.3 \
    --target "$TARGET"

expected_assets="WokCore-v1.2.3-Linux-$PUBLIC_ARCH.deb
WokCore-v1.2.3-Linux-$PUBLIC_ARCH.rpm
WokCore-v1.2.3-Linux-$PUBLIC_ARCH.tar.gz"
test "$(
    find "$DIST" -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort
)" = "$expected_assets"
cmp "$TECHNICAL_ARCHIVE" "$FRIENDLY_ARCHIVE"
test "$(dpkg-deb --field "$DEB_PACKAGE" Package)" = wokcore
test "$(dpkg-deb --field "$DEB_PACKAGE" Version)" = 1.2.3
test "$(dpkg-deb --field "$DEB_PACKAGE" Architecture)" = "$DEB_ARCH"
test "$(rpm -qp --queryformat '%{NAME}' "$RPM_PACKAGE")" = wokcore
test "$(rpm -qp --queryformat '%{VERSION}' "$RPM_PACKAGE")" = 1.2.3
test "$(rpm -qp --queryformat '%{ARCH}' "$RPM_PACKAGE")" = "$RPM_ARCH"

expected_package_files="/usr/bin/wokcore
/usr/share/doc/wokcore/LICENSE-APACHE
/usr/share/doc/wokcore/LICENSE-MIT
/usr/share/doc/wokcore/NOTICE.md
/usr/share/doc/wokcore/README.md"
expected_deb_archive_files="- /usr/bin/wokcore
- /usr/share/doc/wokcore/LICENSE-APACHE
- /usr/share/doc/wokcore/LICENSE-MIT
- /usr/share/doc/wokcore/NOTICE.md
- /usr/share/doc/wokcore/README.md"
test "$(
    dpkg-deb --contents "$DEB_PACKAGE" |
        awk '$1 !~ /^d/ { print substr($1, 1, 1) " /" substr($6, 3) }' |
        LC_ALL=C sort
)" = "$expected_deb_archive_files"
verify_package_tree() {
    local root="$1"
    test "$(
        find "$root" -mindepth 1 ! -type d -printf '%y /%P\n' |
            LC_ALL=C sort
    )" = "$(
        printf '%s\n' "$expected_package_files" | sed 's|^|f |'
    )"
    cmp "$TEST_ROOT/wokcore" "$root/usr/bin/wokcore"
    for name in LICENSE-APACHE LICENSE-MIT NOTICE.md README.md; do
        cmp "$FIXTURE_REPOSITORY/$name" \
            "$root/usr/share/doc/wokcore/$name"
    done
    test "$(stat -c '%a' "$root/usr/bin/wokcore")" = 755
    for name in LICENSE-APACHE LICENSE-MIT NOTICE.md README.md; do
        test "$(stat -c '%a' "$root/usr/share/doc/wokcore/$name")" = 644
    done
}

DEB_EXTRACT="$TEST_ROOT/deb-extract"
mkdir "$DEB_EXTRACT"
dpkg-deb --extract "$DEB_PACKAGE" "$DEB_EXTRACT"
verify_package_tree "$DEB_EXTRACT"

test "$(rpm -qpl "$RPM_PACKAGE" | LC_ALL=C sort)" = "$expected_package_files"
RPM_EXTRACT="$TEST_ROOT/rpm-extract"
mkdir "$RPM_EXTRACT"
rpm2cpio "$RPM_PACKAGE" | (cd "$RPM_EXTRACT" && cpio -idmu --quiet)
verify_package_tree "$RPM_EXTRACT"

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
        "wokcore Linux assets: duplicate $flag argument" \
        "${common_arguments[@]}" \
        "$flag" "$value"
done

expect_failure \
    "wokcore Linux assets: release version is not canonical SemVer" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 01.2.3 \
    --target "$TARGET"

PRERELEASE_ARCHIVE="$TECHNICAL_DIST/wokcore-v1.2.3-alpha.1+build.5-$TARGET.tar.gz"
cp "$TECHNICAL_ARCHIVE" "$PRERELEASE_ARCHIVE"
expect_failure \
    "wokcore Linux assets: native packages require a stable x.y.z version" \
    --technical-archive "$PRERELEASE_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected-prerelease" \
    --version 1.2.3-alpha.1+build.5 \
    --target "$TARGET"
test ! -e "$TEST_ROOT/rejected-prerelease"

expect_failure \
    "Unsupported Linux target: x86_64-unknown-linux-musl" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target x86_64-unknown-linux-musl

cp "$TECHNICAL_ARCHIVE" "$TECHNICAL_DIST/wokcore-v1.2.3-$OTHER_TARGET.tar.gz"
expect_failure \
    "wokcore Linux assets: technical archive name does not match version and target" \
    --technical-archive "$TECHNICAL_DIST/wokcore-v1.2.3-$OTHER_TARGET.tar.gz" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target "$TARGET"

ln -s "$TECHNICAL_ARCHIVE" "$TEST_ROOT/technical-link.tar.gz"
expect_failure \
    "wokcore Linux assets: the technical archive is missing or symbolic" \
    --technical-archive "$TEST_ROOT/technical-link.tar.gz" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target "$TARGET"

ln -s "$TECHNICAL_DIST" "$TEST_ROOT/technical-ancestor-link"
expect_failure \
    "wokcore Linux assets: the technical archive is missing or symbolic" \
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
    "wokcore Linux assets: the release executable is missing or symbolic" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/executable-ancestor-link/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target "$TARGET"

ln -s "$FIXTURE_REPOSITORY" "$TEST_ROOT/repository-ancestor-link"
expect_failure \
    "wokcore Linux assets: the repository root is missing or symbolic" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$TEST_ROOT/repository-ancestor-link/" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target "$TARGET"

mkdir "$TEST_ROOT/output-real"
ln -s "$TEST_ROOT/output-real" "$TEST_ROOT/output-ancestor-link"
expect_failure \
    "wokcore Linux assets: the output directory is missing or symbolic" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/output-ancestor-link/nested/" \
    --version 1.2.3 \
    --target "$TARGET"

cp "$TEST_ROOT/wokcore" "$TEST_ROOT/not-wokcore"
expect_failure \
    "wokcore Linux assets: the Unix release executable must use the fixed wokcore name" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/not-wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/rejected" \
    --version 1.2.3 \
    --target "$TARGET"

REAL_DPKG_DEB="$(command -v dpkg-deb)"
CORRUPT_DEB_ERROR="$TEST_ROOT/corrupt-deb-error"
if env \
    PATH="$CORRUPT_DPKG_DEB_DIRECTORY:$PATH" \
    WOKCORE_REAL_DPKG_DEB="$REAL_DPKG_DEB" \
    "$BUILD_LINUX_ASSETS" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/corrupt-deb-output" \
    --version 1.2.3 \
    --target "$TARGET" \
    >/dev/null 2>"$CORRUPT_DEB_ERROR"; then
    printf 'Linux asset builder accepted a corrupted Debian package\n' >&2
    exit 1
fi
if ! grep -Fqx \
    "wokcore Linux assets: built Debian package metadata is invalid" \
    "$CORRUPT_DEB_ERROR"; then
    printf 'Linux asset builder returned the wrong corrupted Debian error\n' >&2
    cat "$CORRUPT_DEB_ERROR" >&2
    exit 1
fi

EXTRA_DEB_NODE_ERROR="$TEST_ROOT/extra-deb-node-error"
if env \
    PATH="$CORRUPT_DPKG_DEB_DIRECTORY:$PATH" \
    WOKCORE_REAL_DPKG_DEB="$REAL_DPKG_DEB" \
    WOKCORE_DPKG_DEB_EXTRA_NODE=symlink \
    "$BUILD_LINUX_ASSETS" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/extra-deb-node-output" \
    --version 1.2.3 \
    --target "$TARGET" \
    >/dev/null 2>"$EXTRA_DEB_NODE_ERROR"; then
    printf 'Linux asset builder accepted an extra Debian symlink node\n' >&2
    exit 1
fi
if ! grep -Fqx \
    "wokcore Linux assets: built Debian package node inventory is invalid" \
    "$EXTRA_DEB_NODE_ERROR"; then
    printf 'Linux asset builder returned the wrong extra Debian node error\n' >&2
    cat "$EXTRA_DEB_NODE_ERROR" >&2
    exit 1
fi

REAL_RPMBUILD="$(command -v rpmbuild)"
CORRUPT_RPM_ERROR="$TEST_ROOT/corrupt-rpm-error"
if env \
    PATH="$CORRUPT_RPMBUILD_DIRECTORY:$PATH" \
    WOKCORE_REAL_RPMBUILD="$REAL_RPMBUILD" \
    "$BUILD_LINUX_ASSETS" \
    --technical-archive "$TECHNICAL_ARCHIVE" \
    --executable "$TEST_ROOT/wokcore" \
    --repository-root "$FIXTURE_REPOSITORY" \
    --output-directory "$TEST_ROOT/corrupt-rpm-output" \
    --version 1.2.3 \
    --target "$TARGET" \
    >/dev/null 2>"$CORRUPT_RPM_ERROR"; then
    printf 'Linux asset builder accepted a corrupted RPM package\n' >&2
    exit 1
fi
if ! grep -Fqx \
    "wokcore Linux assets: built RPM package metadata is invalid" \
    "$CORRUPT_RPM_ERROR"; then
    printf 'Linux asset builder returned the wrong corrupted RPM error\n' >&2
    cat "$CORRUPT_RPM_ERROR" >&2
    exit 1
fi

mv "$FIXTURE_REPOSITORY/LICENSE-MIT" "$TEST_ROOT/LICENSE-MIT.real"
ln -s "$TEST_ROOT/LICENSE-MIT.real" "$FIXTURE_REPOSITORY/LICENSE-MIT"
expect_failure \
    "wokcore Linux assets: a release document is missing or symbolic" \
    "${common_arguments[@]}"

printf 'Linux asset builder tests passed\n'
