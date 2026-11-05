#!/bin/sh
# Regenerate SRCINFO-* from PKGBUILD-*.
#
# The AUR workflow substitutes the version and checksums into these templates
# rather than running makepkg, because the runner is not an Arch host. That means
# the templates have to be regenerated here, on a machine that has makepkg,
# whenever a PKGBUILD changes.
#
# Run it after editing either PKGBUILD, and commit the result.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

for kind in bin git; do
  cp "$here/PKGBUILD-$kind" "$work/PKGBUILD"
  (cd "$work" && makepkg --printsrcinfo >"$here/SRCINFO-$kind")
  echo "regenerated SRCINFO-$kind"
done
