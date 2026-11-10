#!/usr/bin/env bash
# Builds the Arch package from packaging/arch/avb-endpointd, as someone
# using the PKGBUILD would, but from SOURCE, an archive of this tree,
# rather than the tag's, in a fresh Arch container. run.sh then tests the
# package.
#
#   git archive --prefix=Linux-AVB-Endpoint-avb-endpointd-v0.2.1/ \
#       -o Linux-AVB-Endpoint-0.2.1.tar.gz HEAD
#   docker run --rm -v "$PWD:/work" -w /work archlinux:latest \
#       packaging/tests/build-arch.sh Linux-AVB-Endpoint-0.2.1.tar.gz dist-arch
#
# The archive's name and folder must carry the PKGBUILD's pkgver.
set -euo pipefail

source_archive=$(readlink -f "${1:?usage: build-arch.sh SOURCE OUT}")
out=${2:?usage: build-arch.sh SOURCE OUT}
recipe=$(cd "$(dirname "$0")/../arch/avb-endpointd" && pwd)

pacman -Syu --noconfirm --needed base-devel
# makepkg runs as a regular user, so the dependencies go in first; gptpd,
# which Arch does not carry, the build does not need.
# shellcheck source=/dev/null disable=SC2154
(. "$recipe/PKGBUILD" &&
    printf '%s\n' "${depends[@]}" "${makedepends[@]}" | grep -vx gptpd |
    xargs pacman -S --noconfirm --needed)
useradd -m builder
build=$(mktemp -d)
cp "$recipe/PKGBUILD" "$recipe/avb-endpointd.install" "$source_archive" "$build/"
chown -R builder "$build"
mkdir -p "$out"
(cd "$build" && runuser -u builder -- makepkg --noconfirm --nodeps)
cp "$build"/avb-endpointd-[0-9]*.pkg.tar.* "$out/"
ls -l "$out"
