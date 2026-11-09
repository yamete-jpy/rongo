#!/bin/sh
# Build a release binary and wrap it in target/rongo.app.
set -eu
cd "$(dirname "$0")/.."
cargo build --release -p rongo
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
app=target/rongo.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/rongo "$app/Contents/MacOS/rongo"

# App icon: every size macOS asks for, from the 1024px source
# (redraw it with scripts/make-icon.py).
iconset=target/rongo.iconset
rm -rf "$iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z $size $size assets/icon-1024.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double assets/icon-1024.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/rongo.icns"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>rongo</string>
  <key>CFBundleDisplayName</key><string>rongo</string>
  <key>CFBundleIdentifier</key><string>dev.rongo.app</string>
  <key>CFBundleExecutable</key><string>rongo</string>
  <key>CFBundleIconFile</key><string>rongo.icns</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
# Sign with a fixed identifier. With RONGO_SIGN_IDENTITY set to a code
# signing certificate (see README), macOS keeps "Always Allow" for the
# Keychain across rebuilds; ad-hoc signing ("-") still changes every build.
codesign --force --sign "${RONGO_SIGN_IDENTITY:--}" --identifier dev.rongo.app "$app" 2>&1 | grep -v "replacing existing signature" || true
# Make Finder and the Dock notice the new icon.
touch "$app"
echo "$app"
