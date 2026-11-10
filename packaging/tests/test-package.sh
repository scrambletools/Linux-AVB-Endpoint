#!/bin/sh
# Installs an avb-endpointd package in a fresh container, as someone
# would, with the packages it needs from elsewhere (gptpd's) first; checks
# what it puts where, that avb-netraw holds CAP_NET_RAW and nothing else
# a capability, and that the daemon runs; then that removing it leaves
# nothing behind. run.sh starts it:
#
#   packaging/tests/test-package.sh /packages/avb-endpointd_0.2.1-1_amd64.deb /first/gptpd_0.1.0-1_amd64.deb
set -eu

package=${1:?usage: test-package.sh PACKAGE [FIRST...]}
shift

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

files="/usr/bin/avb-endpointd
/usr/lib/avb-endpointd/avb-netraw
/usr/lib/systemd/user/avb-endpointd@.service
/usr/lib/systemd/system/avb-link@.service
/usr/lib/systemd/system/avb-link-reset@.service
/usr/share/polkit-1/rules.d/50-avb-link.rules
/usr/share/avb-endpointd/models/avb-endpointd.toml
/usr/share/avb-endpointd/models/scramble-vendor.png
/usr/share/doc/avb-endpointd/endpointd.md
/usr/share/licenses/avb-endpointd/LICENSE-MIT
/usr/share/licenses/avb-endpointd/LICENSE-APACHE"

# Ubuntu's and Arch's images leave /usr/share/doc out of every install,
# where a full system doesn't, so the test undoes that first.
case $package in
*.deb)
    sed -i '/^path-exclude/d' /etc/dpkg/dpkg.cfg.d/* 2>/dev/null || true
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "$@" "$package"
    ;;
*.rpm) dnf install -y -q --setopt=install_weak_deps=False "$@" "$package" ;;
*.pkg.tar.*)
    sed -i '/^NoExtract/d' /etc/pacman.conf
    pacman -Syu --noconfirm -q
    # No Arch package of gptpd to install first: the rest of what it
    # needs, then the package without the dependency check.
    pacman -S --noconfirm --needed alsa-lib ethtool gcc-libs glibc iproute2 libcap libpipewire \
        pipewire rtkit
    pacman -Udd --noconfirm "$package"
    ;;
*) fail "no package manager for $package" ;;
esac

for file in $files; do
    [ -e "$file" ] || fail "$file is missing"
done
grep -q '^ExecStart=/usr/bin/avb-endpointd --interface %i$' \
    /usr/lib/systemd/user/avb-endpointd@.service ||
    fail "the user unit does not run /usr/bin/avb-endpointd"
grep -q '^firmware_version = "[0-9]' /usr/share/avb-endpointd/models/avb-endpointd.toml ||
    fail "the default entity's model has no firmware version"
caps=$(getcap /usr/lib/avb-endpointd/avb-netraw)
case $caps in
*cap_net_raw=ep*) ;;
*) fail "avb-netraw holds no CAP_NET_RAW: $caps" ;;
esac
[ -z "$(getcap /usr/bin/avb-endpointd)" ] || fail "the daemon holds a capability"
for binary in /usr/bin/avb-endpointd /usr/lib/avb-endpointd/avb-netraw; do
    if ldd "$binary" | grep 'not found'; then
        fail "$binary misses a shared library"
    fi
done
version=$(avb-endpointd --version) || fail "avb-endpointd --version failed"
echo "$version"
case $version in
"avb-endpointd "[0-9]*) ;;
*) fail "avb-endpointd --version said: $version" ;;
esac

case $package in
*.deb) DEBIAN_FRONTEND=noninteractive apt-get purge -y -qq avb-endpointd ;;
*.rpm) dnf remove -y -q avb-endpointd ;;
*.pkg.tar.*) pacman -Rdd --noconfirm avb-endpointd ;;
esac
for file in $files; do
    [ -e "$file" ] && fail "$file is left after removal"
done
echo "PASS: $(basename "$package")"
