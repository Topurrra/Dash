#!/usr/bin/env bash
# Builds Dash.app around target/release/dash and packs it in a .dmg (with a
# link to /Applications to drag it onto), plus the .dmg's .sha256 for the
# updater. Signed ad hoc only (no Apple Developer ID): the first start needs
# System Settings > Privacy & Security > Open Anyway.
# Run from the repository root after `cargo build --release -p dash`.
set -euo pipefail

version=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "dash"))')
# Named as Rust names the architecture: the updater (app/src/update.rs)
# looks for Dash_<version>_<std::env::consts::ARCH>.dmg.
arch=$(uname -m)
[ "$arch" = arm64 ] && arch=aarch64
out=target/package
app=$out/stage/Dash.app
dmg=Dash_${version}_${arch}.dmg

rm -rf "$out"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/dash "$app/Contents/MacOS/dash"
python3 -I installer/make_icon.py "$app/Contents/Resources/dash.icns"

# LSUIElement: no Dock icon; Dash lives in the menu bar and its hotkeys.
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Dash</string>
  <key>CFBundleDisplayName</key><string>Dash</string>
  <key>CFBundleIdentifier</key><string>io.github.topurrra.dash</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleExecutable</key><string>dash</string>
  <key>CFBundleIconFile</key><string>dash</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

# Sign the finished bundle ad hoc. The linker signs only the binary; a
# downloaded bundle whose signature doesn't cover its Info.plist and icon is
# what macOS calls "damaged and can't be opened". The check fails the build
# if the bundle doesn't verify.
codesign --force --sign - "$app"
codesign --verify --strict --verbose=2 "$app"

ln -s /Applications "$out/stage/Applications"
# hdiutil's own size guess is sometimes too small ("No space left on
# device"): give it the folder's size plus room.
size_mb=$(( $(du -sm "$out/stage" | cut -f1) * 12 / 10 + 20 ))
hdiutil create -volname Dash -srcfolder "$out/stage" -size "${size_mb}m" -ov -format UDZO "$out/$dmg"
(cd "$out" && shasum -a 256 "$dmg" > "$dmg.sha256")
echo "$out/$dmg"
