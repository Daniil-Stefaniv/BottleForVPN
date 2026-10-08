# Сборка portable-дистрибутива BottleForApps (Windows, CI или локально).
#
# Формирует dist/portable/BottleForApps/: bottle.exe, bottled.exe,
# bottle-svc.exe (для --install по желанию админа), appliance/
# bottleforapps-rootfs.tar.gz и INSTALL.txt; упаковывает в
# artifacts/BottleForApps-portable-<version>-win-x64.zip + SHA256.
#
# Параметры:
#   -RootfsPath  путь к собранному bottleforapps-rootfs.tar.gz
#                (артефакт джобы appliance-build; обязателен)
#   -OutDir      каталог для артефактов (по умолчанию artifacts)
#
# Версия читается из [workspace.package].version через cargo metadata —
# единственный источник правды (рулинг плана M3).

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

# Версия workspace из cargo metadata.
$metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
$version = ($metadata.packages | Where-Object { $_.name -eq "daemon" }).version
if (-not $version) {
    throw "не удалось определить версию workspace (пакет daemon)"
}
Write-Host "Версия: $version"

# Release-сборка трёх бинарников (msvc host).
cargo build --release -p daemon -p cli -p svc
if ($LASTEXITCODE -ne 0) { throw "cargo build --release не удался" }

# Раскладка portable.
$layout = "dist/portable/BottleForApps"
if (Test-Path $layout) { Remove-Item -Recurse -Force $layout }
New-Item -ItemType Directory -Force -Path "$layout/appliance" | Out-Null

Copy-Item "target/release/bottle.exe"     $layout
Copy-Item "target/release/bottled.exe"    $layout
Copy-Item "target/release/bottle-svc.exe" $layout
Copy-Item $RootfsPath "$layout/appliance/bottleforapps-rootfs.tar.gz"
Copy-Item $GuiExePath "$layout/bottle-gui.exe"

# INSTALL.txt: русский, portable = env-only (спека §11).
@(
    "BottleForApps $version — portable-вариант (без прав администратора)."
    ""
    "Что здесь лежит:"
    "  bottle.exe      — CLI (bottle create/list/start/stop/doctor/logs export/upgrade)"
    "  bottled.exe     — демон (запустите его в терминале; работает в вашей сессии)"
    "  bottle-svc.exe  — служба маршрутов для host-бутылок (НЕ обязательна)"
    "  bottle-gui.exe  — графический интерфейс (список бутылок, старт/стоп, doctor)"
    "  appliance/      — rootfs аплаенса WSL (используется bottled отсюда)"
    ""
    "Требования:"
    "  - WSL включён в системе ЗАРАНЕЕ (wsl --install --no-distribution + перезагрузка);"
    "    portable-вариант сам WSL не устанавливает."
    ""
    "Ограничения portable:"
    "  - Только env-бутылки (WireGuard/OpenVPN в изолированных окружениях WSL)."
    "  - Host-бутылки (фирменные VPN-клиенты) требуют службу маршрутов:"
    "    запустите от администратора  bottle-svc.exe --install"
    ""
    "Начало работы:"
    "  1) bottled.exe            (демон; держите окно открытым)"
    "  2) bottle doctor          (проверка окружения)"
    "  3) bottle create my-vpn --name `"Мой VPN`" --kind env --protocol wireguard --config C:\path\wg.conf"
    "  4) bottle start my-vpn"
    ""
    "Диагностика: bottle doctor, bottle logs export."
) | Set-Content -Encoding UTF8 "$layout/INSTALL.txt"

# ZIP + SHA256.
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$zip = "$OutDir/BottleForApps-portable-$version-win-x64.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path "$layout/*" -DestinationPath $zip

$hash = (Get-FileHash $zip -Algorithm SHA256).Hash
"$hash  $(Split-Path $zip -Leaf)" | Set-Content -Encoding ascii "$zip.sha256"

Write-Host "Готово: $zip"
Write-Host "SHA256: $hash"
