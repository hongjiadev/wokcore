#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
RPM_SPEC="$SCRIPT_DIRECTORY/../../release/linux/wokcore.spec"
TECHNICAL_ARCHIVE=""
EXECUTABLE=""
REPOSITORY_ROOT=""
OUTPUT_DIRECTORY=""
VERSION=""
TARGET=""

fail() {
    printf 'wokcore Linux assets: %s\n' "$1" >&2
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
[[ -f "$RPM_SPEC" && ! -L "$RPM_SPEC" ]] ||
    fail "the committed RPM spec is missing or symbolic"
[[ -n "$OUTPUT_DIRECTORY" ]] || fail "output directory is required"
for command in cp dpkg-deb install rpmbuild; do
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

trap - EXIT
rm -rf -- "$WORK_ROOT"
printf '%s\n%s\n%s\n' "$FRIENDLY_ARCHIVE" "$DEB_PACKAGE" "$RPM_PACKAGE"
