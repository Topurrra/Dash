# Builds the Dash installer: release dash.exe, then dash.nsi with NSIS
# (makensis from PATH, or the copy Tauri's bundler downloads).
# Output: dash/target/release/bundle/Dash_<version>_x64-setup.exe
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$dash = Split-Path $here

$version = (Select-String -Path "$dash\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
cargo build --release -p dash --manifest-path "$dash\Cargo.toml"
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

if (-not (Test-Path "$here\dash.ico")) { python -I "$here\make_icon.py" "$here\dash.ico" }

$makensis = (Get-Command makensis -ErrorAction SilentlyContinue).Source
if (-not $makensis) { $makensis = "$env:LOCALAPPDATA\tauri\NSIS\makensis.exe" }
if (-not (Test-Path $makensis)) { throw "NSIS not found: install it or put makensis on PATH" }

$out = "$dash\target\release\bundle"
New-Item -ItemType Directory -Force $out | Out-Null
$setup = "$out\Dash_${version}_x64-setup.exe"
& $makensis /V2 "/DVERSION=$version" "/DOUTFILE=$setup" "$here\dash.nsi"
if ($LASTEXITCODE -ne 0) { throw "makensis failed" }
# Published beside the installer on a GitHub Release: Dash's updater only
# installs a download whose SHA-256 matches this file (app/src/update.rs).
$hash = (Get-FileHash -Algorithm SHA256 $setup).Hash.ToLower()
Set-Content -NoNewline -Encoding ascii "$setup.sha256" "$hash  Dash_${version}_x64-setup.exe"
Get-Item $setup, "$setup.sha256"
