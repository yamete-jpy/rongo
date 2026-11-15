#!/bin/sh
# Cross-build rongo.exe for Windows 10/11 (x64) from a Mac and wrap it in
# target/rongo-<version>-x64-setup.exe, a per-user installer made with NSIS
# (scripts/windows/rongo.nsi). Prints the installer's path last, on stdout
# alone; everything else goes to stderr.
#
# Needs, once:
#   rustup target add x86_64-pc-windows-gnu
#   brew install mingw-w64 nsis
set -eu
cd "$(dirname "$0")/.."

missing=""
rustup target list --installed 2>/dev/null | grep -qx x86_64-pc-windows-gnu || missing="$missing\n  rustup target add x86_64-pc-windows-gnu"
for tool in x86_64-w64-mingw32-gcc x86_64-w64-mingw32-windres; do
  command -v "$tool" >/dev/null 2>&1 || { missing="$missing\n  brew install mingw-w64   (provides $tool)"; break; }
done
command -v makensis >/dev/null 2>&1 || missing="$missing\n  brew install nsis   (provides makensis)"
if [ -n "$missing" ]; then
  printf 'bundle-windows.sh: missing tools. Install them, then run this again:%b\n' "$missing" >&2
  exit 1
fi

# The icon and version info are put into rongo.exe by crates/app/build.rs
# (windres); the icon itself is assets/icon.ico (scripts/make-icon.py).
. ./scripts/trim-paths.sh
cargo build --release --target x86_64-pc-windows-gnu -p rongo >&2
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
exe=target/x86_64-pc-windows-gnu/release/rongo.exe
[ -f "$exe" ] || { echo "bundle-windows.sh: $exe was not built" >&2; exit 1; }

stage=target/windows-stage
rm -rf "$stage"
mkdir -p "$stage"
cp "$exe" "$stage/rongo.exe"

out=target/rongo-${version}-x64-setup.exe
rm -f "$out"
# -WX: a warning is an error. Absolute paths, so the script does not depend
# on where makensis resolves relative ones from.
makensis -WX -V3 \
  -DVERSION="$version" \
  -DEXE="$PWD/$stage/rongo.exe" \
  -DICON="$PWD/assets/icon.ico" \
  -DOUTFILE="$PWD/$out" \
  scripts/windows/rongo.nsi >&2
[ -f "$out" ] || { echo "bundle-windows.sh: $out was not built" >&2; exit 1; }
rm -rf "$stage"
echo "$out"
