param(
    [string]$Version = "0.1.0"
)

$ErrorActionPreference = "Stop"
$AppName = "CutDub"
$BinName = "cutdub.exe"

Write-Host "==> Building $AppName v$Version (Windows x86_64) in release mode..."
cargo build --release --bin cutdub --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$DistDir = "releases\$Version"
$PkgDir = "$DistDir\$AppName-$Version-windows-x86_64"
if (Test-Path $PkgDir) { Remove-Item -Recurse -Force $PkgDir }
New-Item -ItemType Directory -Path $PkgDir | Out-Null

Copy-Item "target\x86_64-pc-windows-msvc\release\$BinName" "$PkgDir\$BinName"
Copy-Item "README.md" "$PkgDir\README.md"
Copy-Item "LICENSE" "$PkgDir\LICENSE"

$ZipPath = "$DistDir\$AppName-$Version-windows-x86_64.zip"
if (Test-Path $ZipPath) { Remove-Item -Force $ZipPath }
Compress-Archive -Path "$PkgDir\*" -DestinationPath $ZipPath
Write-Host "==> Created Windows portable release: $ZipPath"
