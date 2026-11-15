#!/bin/sh
# Build the disk image and the Windows installer, and point Casks/rongo.rb at
# the disk image. Then commit, tag v<version>, push, and upload both files to
# the GitHub release of that tag:
#   gh release create v<version> target/rongo-<version>-arm64.dmg target/rongo-<version>-x64-setup.exe
set -eu
cd "$(dirname "$0")/.."
dmg=$(./scripts/make-dmg.sh)
setup=$(./scripts/bundle-windows.sh)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
sha=$(shasum -a 256 "$dmg" | cut -d' ' -f1)
sed -i '' -e "s/^  version \".*\"/  version \"$version\"/" \
          -e "s/^  sha256 \".*\"/  sha256 \"$sha\"/" Casks/rongo.rb
echo "$dmg"
echo "$setup"
echo "Casks/rongo.rb -> $version $sha"
