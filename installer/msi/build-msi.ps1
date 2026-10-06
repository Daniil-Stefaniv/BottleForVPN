# Сборка MSI BottleForApps (Windows + WiX v4/v5 CLI; CI: windows-раннер).
#
# Формирует artifacts/BottleForApps-<version>-win-x64.msi + SHA256.
# Версия — из [workspace.package].version (cargo metadata); требует
# собранных release-бинарников (cargo build --release -p daemon -p cli
# -p svc) и rootfs-артефакта appliance-build.
#
# WiX ставится при отсутствии: dotnet tool (глобально) либо choco
# wixtoolset — что доступно на раннере.

param(
    [Parameter(Mandatory = $true)]
    [string]$RootfsPath,

    # GUI (M4): путь к bottle-gui.exe (артефакт джобы gui-build).
    [Parameter(Mandatory = $true)]
    [string]$GuiExePath,

    [string]$OutDir = "artifacts"
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $RootfsPath)) {
    throw "rootfs не найден: $RootfsPath (артефакт appliance-build обязателен)"
}
if (-not (Test-Path $GuiExePath)) {
    throw "bottle-gui.exe не найден: $GuiExePath (артефакт gui-build обязателен)"
}

# WiX CLI: PATH → dotnet tool → choco.
if (-not (Get-Command wix -ErrorAction SilentlyContinue)) {
    Write-Host "WiX не найден — устанавливаю dotnet tool..."
    dotnet tool install --global wix
    if ($LASTEXITCODE -ne 0) {
        Write-Host "dotnet tool не удался — пробую choco wixtoolset..."
        choco install wixtoolset -y
        if ($LASTEXITCODE -ne 0) { throw "не удалось установить WiX" }
    }
}

$metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
$version = ($metadata.packages | Where-Object { $_.name -eq "daemon" }).version
if (-not $version) {
    throw "не удалось определить версию workspace (пакет daemon)"
}
Write-Host "Версия: $version"

if (-not (Test-Path "target/release/bottle.exe")) {
    cargo build --release -p daemon -p cli -p svc
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release не удался" }
}

$buildDir = (Resolve-Path "target/release/").Path + [System.IO.Path]::DirectorySeparatorChar
$rootfs = (Resolve-Path $RootfsPath).Path
$guiExe = (Resolve-Path $GuiExePath).Path

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$msi = "$OutDir/BottleForApps-$version-win-x64.msi"
if (Test-Path $msi) { Remove-Item -Force $msi }

# wix build сам валидирует ICE; -arch x64 обязателен для ProgramFiles64Folder.
wix build `
    -arch x64 `
    -d "ProductVersion=$version" `
    -d "BuildDir=$buildDir" `
    -d "RootfsPath=$rootfs" `
    -d "GuiExePath=$guiExe" `
    -o $msi `
    installer/msi/BottleForApps.wxs
if ($LASTEXITCODE -ne 0) { throw "wix build не удался" }

$hash = (Get-FileHash $msi -Algorithm SHA256).Hash
"$hash  $(Split-Path $msi -Leaf)" | Set-Content -Encoding ascii "$msi.sha256"

Write-Host "Готово: $msi"
Write-Host "SHA256: $hash"
