# Downloads Google's Android platform-tools and copies adb into
# desktop/src-tauri/bin so the Windows installer can bundle it.
# Run once before `npx tauri build`.
$ErrorActionPreference = "Stop"
$bin = Join-Path $PSScriptRoot "src-tauri\bin"
$tmp = Join-Path $env:TEMP "phonecast-platform-tools"
New-Item -ItemType Directory -Force $bin, $tmp | Out-Null

$zip = Join-Path $tmp "platform-tools.zip"
Write-Host "Downloading platform-tools from Google..."
curl.exe -sSL -o $zip "https://dl.google.com/android/repository/platform-tools-latest-windows.zip"
tar -xf $zip -C $tmp
foreach ($f in "adb.exe", "AdbWinApi.dll", "AdbWinUsbApi.dll") {
    Copy-Item (Join-Path $tmp "platform-tools\$f") $bin -Force
}
Write-Host "adb ready in $bin"
