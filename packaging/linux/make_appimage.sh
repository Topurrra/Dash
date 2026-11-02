#!/usr/bin/env bash
# Packs target/release/dash as an AppImage, plus its .sha256 for the updater.
# The system libraries GPUI uses (Vulkan, xkbcommon, Wayland / X11, ALSA,
# D-Bus) come from the system, as they do for other desktop apps.
# Run from the repository root after `cargo build --release -p dash`.
set -euo pipefail

# appimagetool, pinned (chosen 2026-11-02).
tool_url=https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage

version=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "dash"))')
arch=$(uname -m)
out=target/package
appdir=$out/Dash.AppDir
image=Dash_${version}_${arch}.AppImage

rm -rf "$out"
mkdir -p "$appdir/usr/bin"
cp target/release/dash "$appdir/usr/bin/dash"
ln -s usr/bin/dash "$appdir/AppRun"
python3 -I installer/make_icon.py "$appdir/dash.png"
cat > "$appdir/dash.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Dash
Comment=Search and clipboard history in a hotkey overlay
Exec=dash
Icon=dash
Categories=Utility;
Terminal=false
EOF

curl -fsSL -o "$out/appimagetool" "$tool_url"
chmod +x "$out/appimagetool"
# No FUSE on CI runners: the tool unpacks itself and runs.
ARCH=$arch APPIMAGE_EXTRACT_AND_RUN=1 "$out/appimagetool" "$appdir" "$out/$image"
(cd "$out" && sha256sum "$image" > "$image.sha256")
echo "$out/$image"
