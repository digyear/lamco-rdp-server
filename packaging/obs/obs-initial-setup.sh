#!/bin/bash
# Initial OBS home project setup for lamco-rdp-server
# Run interactively on tumbleweed (192.168.1.102) after osc authentication.
#
# Prerequisites:
#   - osc installed (zypper install osc)
#   - Authenticated: osc ls home:lamco-development
#   - Spec file at /tmp/lamco-rdp-server.spec
#   - Source tarball at /tmp/lamco-rdp-server-$VERSION.tar.xz
#
# Usage:
#   bash obs-initial-setup.sh [VERSION]
#   Example: bash obs-initial-setup.sh 1.4.2

set -euo pipefail

VERSION="${1:-1.4.2}"
USERNAME="lamco-development"
PKG="lamco-rdp-server"
PRJ="home:${USERNAME}"

SPEC="/tmp/${PKG}.spec"
TARBALL="/tmp/${PKG}-${VERSION}.tar.xz"

echo "=== OBS Home Project Setup ==="
echo "Project: ${PRJ}"
echo "Package: ${PKG}"
echo "Version: ${VERSION}"
echo ""

# Validate inputs
for f in "$SPEC" "$TARBALL"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: $f not found"
        exit 1
    fi
done

# Step 1: Set project meta with Tumbleweed repo
echo ">>> Setting project metadata..."
osc meta prj "${PRJ}" --file /dev/stdin << META
<project name="${PRJ}">
  <title>Lamco Development</title>
  <description>Wayland RDP server and related tools</description>
  <person userid="${USERNAME}" role="maintainer"/>
  <repository name="openSUSE_Tumbleweed">
    <path project="openSUSE:Factory" repository="snapshot"/>
    <arch>x86_64</arch>
  </repository>
</project>
META
echo "Project meta set."

# Step 2: Create package
echo ""
echo ">>> Creating package..."
osc meta pkg "${PRJ}" "${PKG}" --file /dev/stdin << PKGMETA
<package name="${PKG}" project="${PRJ}">
  <title>Wayland RDP server for Linux desktops</title>
  <description>High-performance RDP server using XDG Desktop Portals for secure screen capture and input injection. Features H.264 encoding, multi-monitor support, clipboard sync, and a full-featured configuration GUI.</description>
  <url>https://www.lamco.ai/products/lamco-rdp-server/</url>
</package>
PKGMETA
echo "Package meta set."

# Step 3: Checkout and upload files
echo ""
echo ">>> Checking out package..."
WORK_DIR=$(mktemp -d /tmp/obs-setup-XXXXXX)
cd "$WORK_DIR"
osc checkout "${PRJ}/${PKG}"
cd "${PRJ//:/:}/${PKG}"

echo ">>> Copying spec and tarball..."
cp "$SPEC" .
cp "$TARBALL" .

echo ">>> Adding files to osc..."
osc add "${PKG}.spec" "${PKG}-${VERSION}.tar.xz"

echo ">>> Committing..."
osc commit -m "Initial package: ${PKG} ${VERSION}"

echo ""
echo "=== Done! ==="
echo "Build status: https://build.opensuse.org/package/show/${PRJ}/${PKG}"
echo "One-click install: https://software.opensuse.org/package/${PKG}"
echo ""
echo "Monitor build: osc results ${PRJ} ${PKG}"

# Cleanup
rm -rf "$WORK_DIR"
