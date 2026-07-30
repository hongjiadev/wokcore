#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
RPM_SPEC="$SCRIPT_DIRECTORY/../../release/linux/wokcore.spec"
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
    printf 'wokcore Linux assets: %s\n' "$1" >&2
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

EXPECTED_PACKAGE_FILES="/usr/bin/wokcore
/usr/share/doc/wokcore/LICENSE-APACHE
/usr/share/doc/wokcore/LICENSE-MIT
/usr/share/doc/wokcore/NOTICE.md
/usr/share/doc/wokcore/README.md"
RPM_ALLOWED_DIRECTORIES="/usr
/usr/bin
/usr/share
/usr/share/doc
/usr/share/doc/wokcore"
EXPECTED_DEB_NODE_INVENTORY="- ./usr/bin/wokcore
- ./usr/share/doc/wokcore/LICENSE-APACHE
- ./usr/share/doc/wokcore/LICENSE-MIT
- ./usr/share/doc/wokcore/NOTICE.md
- ./usr/share/doc/wokcore/README.md
d ./
d ./usr/
d ./usr/bin/
d ./usr/share/
d ./usr/share/doc/
d ./usr/share/doc/wokcore/"

verify_package_tree() {
    local root="$1"
    local package_format="$2"
    local actual_files
    actual_files="$(
        find "$root" -type f -printf '/%P\n' | LC_ALL=C sort
    )" || fail "built $package_format package file list is invalid"
    [[ "$actual_files" == "$EXPECTED_PACKAGE_FILES" ]] ||
        fail "built $package_format package file list is invalid"
    cmp -- "$EXECUTABLE" "$root/usr/bin/wokcore" ||
        fail "built $package_format package payload bytes are invalid"
    for name in LICENSE-APACHE LICENSE-MIT NOTICE.md README.md; do
        cmp -- "$REPOSITORY_ROOT/$name" \
            "$root/usr/share/doc/wokcore/$name" ||
            fail "built $package_format package payload bytes are invalid"
    done
    [[ "$(stat -c '%a' "$root/usr/bin/wokcore")" == 755 ]] ||
        fail "built $package_format package modes are invalid"
    for name in LICENSE-APACHE LICENSE-MIT NOTICE.md README.md; do
        [[ "$(
            stat -c '%a' "$root/usr/share/doc/wokcore/$name"
        )" == 644 ]] ||
            fail "built $package_format package modes are invalid"
    done
}

normalize_rpm_file_list() {
    local path
    local normalized
    while IFS= read -r path; do
        [[ -n "$path" ]] || fail "built RPM package file list is invalid"
        normalized="${path%/}"
        if printf '%s\n' "$RPM_ALLOWED_DIRECTORIES" |
            grep -Fqx -- "$normalized"; then
            [[ "$path" == "$normalized" || "$path" == "$normalized/" ]] ||
                fail "built RPM package file list is invalid"
        elif [[
            "$normalized" =~ ^/usr/lib/\.build-id$ ||
            "$normalized" =~ ^/usr/lib/\.build-id/[0-9a-f]{2}$ ||
            "$normalized" =~ ^/usr/lib/\.build-id/[0-9a-f]{2}/[0-9a-f]{38}$
        ]]; then
            [[ "$path" == "$normalized" || "$path" == "$normalized/" ]] ||
                fail "built RPM package file list is invalid"
        else
            printf '%s\n' "$path"
        fi
    done
}

verify_rpm_build_id_links() {
    local root="$1"
    local link
    local link_count=0
    local links
    links="$(
        find "$root" -type l -printf '/%P -> %l\n' | LC_ALL=C sort
    )" || fail "built RPM package file list is invalid"
    while IFS= read -r link; do
        [[ -n "$link" ]] || continue
        link_count=$((link_count + 1))
        [[ "$link" =~ ^/usr/lib/\.build-id/[0-9a-f]{2}/[0-9a-f]{38}\ \-\>\ ../../../../usr/bin/wokcore$ ]] ||
            fail "built RPM package file list is invalid"
    done <<< "$links"
    [[ "$link_count" -le 1 ]] ||
        fail "built RPM package file list is invalid"
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
# Linux native package metadata supports stable WokCore releases only.
[[ "$VERSION" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
    fail "native packages require a stable x.y.z version"
case "$TARGET" in
    x86_64-unknown-linux-gnu)
        PUBLIC_ARCH=x86_64
        DEB_ARCH=amd64
        RPM_ARCH=x86_64
        ;;
    aarch64-unknown-linux-gnu)
        PUBLIC_ARCH=arm64
        DEB_ARCH=arm64
        RPM_ARCH=aarch64
        ;;
    *)
        printf 'Unsupported Linux target: %s\n' "$TARGET" >&2
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
if path_has_symlink "$RPM_SPEC"; then
    fail "the committed RPM spec is missing or symbolic"
fi
[[ -f "$RPM_SPEC" && ! -L "$RPM_SPEC" ]] ||
    fail "the committed RPM spec is missing or symbolic"
[[ -n "$OUTPUT_DIRECTORY" ]] || fail "output directory is required"
if path_has_symlink "$OUTPUT_DIRECTORY"; then
    fail "the output directory is missing or symbolic"
fi
for command in awk cmp cpio cp dpkg-deb find install rpm rpm2cpio rpmbuild sort stat tar; do
    command -v "$command" >/dev/null 2>&1 ||
        fail "$command is required"
done

mkdir -p "$OUTPUT_DIRECTORY"
[[ -d "$OUTPUT_DIRECTORY" && ! -L "$OUTPUT_DIRECTORY" ]] ||
    fail "the output directory is not a regular directory"
FRIENDLY_PREFIX="WokCore-v${VERSION}-Linux-${PUBLIC_ARCH}"
FRIENDLY_ARCHIVE="$OUTPUT_DIRECTORY/${FRIENDLY_PREFIX}.tar.gz"
DEB_PACKAGE="$OUTPUT_DIRECTORY/${FRIENDLY_PREFIX}.deb"
RPM_PACKAGE="$OUTPUT_DIRECTORY/${FRIENDLY_PREFIX}.rpm"
for destination in "$FRIENDLY_ARCHIVE" "$DEB_PACKAGE" "$RPM_PACKAGE"; do
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

cp -- "$TECHNICAL_ARCHIVE" "$FRIENDLY_ARCHIVE"

DEB_ROOT="$WORK_ROOT/deb"
install -D -m 0755 "$EXECUTABLE" "$DEB_ROOT/usr/bin/wokcore"
install -D -m 0644 \
    "$REPOSITORY_ROOT/LICENSE-APACHE" \
    "$DEB_ROOT/usr/share/doc/wokcore/LICENSE-APACHE"
install -D -m 0644 \
    "$REPOSITORY_ROOT/LICENSE-MIT" \
    "$DEB_ROOT/usr/share/doc/wokcore/LICENSE-MIT"
install -D -m 0644 \
    "$REPOSITORY_ROOT/NOTICE.md" \
    "$DEB_ROOT/usr/share/doc/wokcore/NOTICE.md"
install -D -m 0644 \
    "$REPOSITORY_ROOT/README.md" \
    "$DEB_ROOT/usr/share/doc/wokcore/README.md"
mkdir -p "$DEB_ROOT/DEBIAN"
cat >"$DEB_ROOT/DEBIAN/control" <<EOF
Package: wokcore
Version: $VERSION
Section: utils
Priority: optional
Architecture: $DEB_ARCH
Maintainer: WokCore maintainers
Description: Independent local Provider gateway
 WokCore independent local Provider gateway.
EOF
chmod 0644 "$DEB_ROOT/DEBIAN/control"
dpkg-deb --build --root-owner-group "$DEB_ROOT" "$DEB_PACKAGE"
DEB_VERIFY="$WORK_ROOT/deb-verify"
mkdir "$DEB_VERIFY"
DEB_NAME="$(dpkg-deb --field "$DEB_PACKAGE" Package 2>/dev/null)" ||
    fail "built Debian package metadata is invalid"
DEB_VERSION="$(dpkg-deb --field "$DEB_PACKAGE" Version 2>/dev/null)" ||
    fail "built Debian package metadata is invalid"
DEB_BUILT_ARCH="$(dpkg-deb --field "$DEB_PACKAGE" Architecture 2>/dev/null)" ||
    fail "built Debian package metadata is invalid"
[[ "$DEB_NAME" == wokcore &&
    "$DEB_VERSION" == "$VERSION" &&
    "$DEB_BUILT_ARCH" == "$DEB_ARCH" ]] ||
    fail "built Debian package metadata is invalid"
DEB_DATA_ARCHIVE="$WORK_ROOT/deb-data.tar"
dpkg-deb --fsys-tarfile "$DEB_PACKAGE" >"$DEB_DATA_ARCHIVE" 2>/dev/null ||
    fail "built Debian package payload is invalid"
DEB_NODE_INVENTORY="$(
    LC_ALL=C tar -tvf "$DEB_DATA_ARCHIVE" |
        awk '{ print substr($1, 1, 1) " " $6 }' |
        LC_ALL=C sort
)" || fail "built Debian package node inventory is invalid"
[[ "$DEB_NODE_INVENTORY" == "$EXPECTED_DEB_NODE_INVENTORY" ]] ||
    fail "built Debian package node inventory is invalid"
dpkg-deb --extract "$DEB_PACKAGE" "$DEB_VERIFY" 2>/dev/null ||
    fail "built Debian package payload is invalid"
verify_package_tree "$DEB_VERIFY" Debian

RPM_ROOT="$WORK_ROOT/rpmbuild"
SOURCES="$RPM_ROOT/SOURCES"
install -D -m 0755 "$EXECUTABLE" "$SOURCES/wokcore"
install -D -m 0644 \
    "$REPOSITORY_ROOT/LICENSE-APACHE" \
    "$SOURCES/LICENSE-APACHE"
install -D -m 0644 \
    "$REPOSITORY_ROOT/LICENSE-MIT" \
    "$SOURCES/LICENSE-MIT"
install -D -m 0644 "$REPOSITORY_ROOT/NOTICE.md" "$SOURCES/NOTICE.md"
install -D -m 0644 "$REPOSITORY_ROOT/README.md" "$SOURCES/README.md"
rpmbuild \
    --define "_topdir $RPM_ROOT" \
    --define "wokcore_version $VERSION" \
    --define "wokcore_arch $RPM_ARCH" \
    --define "wokcore_executable $SOURCES/wokcore" \
    --define "license_apache $SOURCES/LICENSE-APACHE" \
    --define "license_mit $SOURCES/LICENSE-MIT" \
    --define "notice $SOURCES/NOTICE.md" \
    --define "readme $SOURCES/README.md" \
    -bb "$RPM_SPEC"
BUILT_RPM="$RPM_ROOT/RPMS/$RPM_ARCH/wokcore-$VERSION-1.$RPM_ARCH.rpm"
[[ -f "$BUILT_RPM" && ! -L "$BUILT_RPM" ]] ||
    fail "rpmbuild did not produce the expected package"
cp -- "$BUILT_RPM" "$RPM_PACKAGE"
RPM_NAME="$(rpm -qp --queryformat '%{NAME}' "$RPM_PACKAGE" 2>/dev/null)" ||
    fail "built RPM package metadata is invalid"
RPM_VERSION="$(rpm -qp --queryformat '%{VERSION}' "$RPM_PACKAGE" 2>/dev/null)" ||
    fail "built RPM package metadata is invalid"
RPM_BUILT_ARCH="$(rpm -qp --queryformat '%{ARCH}' "$RPM_PACKAGE" 2>/dev/null)" ||
    fail "built RPM package metadata is invalid"
[[ "$RPM_NAME" == wokcore &&
    "$RPM_VERSION" == "$VERSION" &&
    "$RPM_BUILT_ARCH" == "$RPM_ARCH" ]] ||
    fail "built RPM package metadata is invalid"
RPM_FILES="$(rpm -qpl "$RPM_PACKAGE" 2>/dev/null | LC_ALL=C sort | normalize_rpm_file_list)" ||
    fail "built RPM package file list is invalid"
[[ "$RPM_FILES" == "$EXPECTED_PACKAGE_FILES" ]] ||
    fail "built RPM package file list is invalid"
RPM_VERIFY="$WORK_ROOT/rpm-verify"
mkdir "$RPM_VERIFY"
rpm2cpio "$RPM_PACKAGE" 2>/dev/null |
    (cd "$RPM_VERIFY" && cpio -idmu --quiet 2>/dev/null) ||
    fail "built RPM package payload is invalid"
verify_rpm_build_id_links "$RPM_VERIFY"
verify_package_tree "$RPM_VERIFY" RPM

trap - EXIT
rm -rf -- "$WORK_ROOT"
printf '%s\n%s\n%s\n' "$FRIENDLY_ARCHIVE" "$DEB_PACKAGE" "$RPM_PACKAGE"
