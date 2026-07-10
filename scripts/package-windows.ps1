# Package navgator for Windows into dist\ as a portable .zip: navgator-<ver>-windows-x64.zip
#
# Layout inside the zip:
#   navgator.exe                 the browser (Servo engine + egui chrome)
#   navgator.ico                 app icon (not yet embedded in the exe; see build.rs TODO)
#   resources\content\           gator:// page templates (navgator reads these next to the exe)
#   *.dll                        GStreamer core runtime (Servo media backend) next to the exe
#   lib\gstreamer-1.0\*.dll      GStreamer plugins, loaded at runtime
#   navgator.cmd                 launcher: points GST_PLUGIN_PATH at the bundled plugins
#
# Mirrors the macOS .app / Linux AppImage self-contained bundling. Run from the repo root on the
# Windows agent after cargo build --release. Not a signed installer; a portable zip is the first
# Windows artifact (an .msi via cargo-wix is a follow-up). ASCII-only (transfers cleanly).
param(
  [string]$Dist = 'dist',
  [string]$Gst  = 'C:\Program Files\GStreamer\1.0\msvc_x86_64'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# Version from crates/navgator/Cargo.toml (same source as package.sh).
$ver = (Select-String -Path 'crates\navgator\Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' |
        Select-Object -First 1).Matches.Groups[1].Value
if (-not $ver) { throw 'could not read version from crates/navgator/Cargo.toml' }

$exe = 'target\release\navgator.exe'
if (-not (Test-Path $exe)) { throw "missing $exe (run cargo build --release --workspace first)" }

$name = "navgator-$ver-windows-x64"
$root = Join-Path $Dist $name
Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $root | Out-Null

# 1. binary + icon
Copy-Item $exe (Join-Path $root 'navgator.exe')
if (Test-Path 'packaging\navgator.ico') { Copy-Item 'packaging\navgator.ico' (Join-Path $root 'navgator.ico') }

# 2. resources (navgator resolves resources\content next to the exe)
New-Item -ItemType Directory -Force (Join-Path $root 'resources') | Out-Null
Copy-Item -Recurse 'crates\navgator\src\content' (Join-Path $root 'resources\content')

# 3. GStreamer runtime: core DLLs next to the exe (so it launches), plugins under lib\gstreamer-1.0.
if (Test-Path $Gst) {
  Copy-Item "$Gst\bin\*.dll" $root
  $plug = Join-Path $root 'lib\gstreamer-1.0'
  New-Item -ItemType Directory -Force $plug | Out-Null
  Copy-Item "$Gst\lib\gstreamer-1.0\*.dll" $plug
  $launcher = "@echo off`r`nset GST_PLUGIN_PATH=%~dp0lib\gstreamer-1.0`r`nset GST_PLUGIN_SYSTEM_PATH=`r`n`"%~dp0navgator.exe`" %*`r`n"
  Set-Content -Path (Join-Path $root 'navgator.cmd') -Value $launcher -Encoding ascii -NoNewline
  Write-Host "bundled GStreamer runtime from $Gst"
} else {
  Write-Warning "GStreamer not found at $Gst; the zip will need a host GStreamer install to run."
}

# 4. zip
$zip = Join-Path $Dist "$name.zip"
Remove-Item $zip -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $root '*') -DestinationPath $zip -Force
$sizeMb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
# Drop the staging dir so only the .zip is stashed/published (not ~400 loose files).
Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
Write-Host ("Windows package: {0} ({1} MB)" -f $zip, $sizeMb)
