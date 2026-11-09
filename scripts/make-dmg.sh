#!/bin/sh
# Build target/rongo.app and wrap it in a disk image to hand to others:
# open it, drag rongo onto Applications.
set -eu
cd "$(dirname "$0")/.."
./scripts/bundle-macos.sh >/dev/null
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
arch=$(lipo -archs target/rongo.app/Contents/MacOS/rongo | tr ' ' '+')
dmg="target/rongo-${version}-${arch}.dmg"

stage=target/dmg
rm -rf "$stage"
mkdir -p "$stage"
# ditto keeps the bundle's signature and attributes intact.
ditto target/rongo.app "$stage/rongo.app"
ln -s /Applications "$stage/Applications"
cat > "$stage/อ่านก่อนติดตั้ง.txt" <<'TEXT'
ติดตั้ง rongo
==============

1. ลาก rongo ไปวางบน Applications
2. เปิด rongo จาก Applications (หรือ Launchpad)

ใช้ได้กับ Mac ชิป Apple (M1 ขึ้นไป) และ macOS 11 ขึ้นไป


ถ้าเปิดแล้วขึ้นว่า "rongo" Not Opened / can't be opened
-------------------------------------------------------
rongo ไม่ได้ลงทะเบียนกับ Apple เลยต้องอนุญาตเองครั้งแรกครั้งเดียว

วิธีที่ 1: กด Done ที่หน้าต่างแจ้งเตือน แล้วไปที่
  System Settings > Privacy & Security
  เลื่อนลงไปจะเจอ "rongo" was blocked ... กด Open Anyway
  แล้วใส่รหัสผ่านเครื่อง

วิธีที่ 2: เปิด Terminal แล้ววางคำสั่งนี้ (ต้องลากไป Applications ก่อน)
  xattr -dr com.apple.quarantine /Applications/rongo.app

แล้วเปิด rongo ใหม่


อัปเดตเป็นเวอร์ชันใหม่
----------------------
ปิด rongo ก่อน แล้วลากตัวใหม่ไปทับใน Applications (กด Replace)
การเชื่อมต่อที่บันทึกไว้และรหัสผ่านใน Keychain ยังอยู่ครบ
TEXT

rm -f "$dmg"
hdiutil create -quiet -volname rongo -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg"
rm -rf "$stage"
echo "$dmg"
