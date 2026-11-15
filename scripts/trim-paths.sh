# Sourced by bundle-macos.sh and bundle-windows.sh, from the repository root:
# the release binaries then name source files as /rongo/... and ~/.cargo/...
# (in panic messages and the like), not by this machine's folders (the user
# name, where the checkout is). Rust code and C code (rquickjs, ring) alike;
# rquickjs-sys sets CFLAGS itself for Windows, so a cross build also gets the
# C flags through TARGET_CFLAGS, which the cc crate reads as well.
# Changing these flags rebuilds everything, so a plain `cargo build --release`
# after a bundle (or the other way round) builds again from scratch.
sep=$(printf '\037')
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$HOME=~${sep}--remap-path-prefix=$PWD=/rongo"
map="-ffile-prefix-map=$HOME=~ -ffile-prefix-map=$PWD=/rongo"
export CFLAGS="${CFLAGS:+$CFLAGS }$map"
export TARGET_CFLAGS="${TARGET_CFLAGS:+$TARGET_CFLAGS }$map"
