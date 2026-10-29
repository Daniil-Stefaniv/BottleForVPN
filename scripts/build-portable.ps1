# Сборка portable-дистрибутива Погребка (Windows, CI или локально).
#
# Формирует dist/portable/Pogrebok/: pogrebok-gui.exe, pogrebok.exe,
# pogrebok-daemon.exe, pogrebok-svc.exe (для --install по желанию
# админа), appliance/pogrebok-rootfs.tar.gz и INSTALL.txt; упаковывает в
# artifacts/Pogrebok-portable-<version>-win-x64.zip + SHA256.
#
# Параметры:
#   -RootfsPath  путь к собранному pogrebok-rootfs.tar.gz
#                (артефакт джобы appliance-build; обязателен)
#   -OutDir      каталог для артефактов (по умолчанию artifacts)
#
# Версия читается из [workspace.package].version через cargo metadata —
# единственный источник правды (рулинг плана M3).

param(
    [Parameter(Mandatory = $true)]
    [string]$RootfsPath,

    # GUI (M4): путь к pogrebok-gui.exe (артефакт джобы gui-build).
    [Parameter(Mandatory = $true)]
    [string]$GuiExePath,

    [string]$OutDir = "artifacts"
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $RootfsPath)) {
    throw "rootfs не найден: $RootfsPath (артефакт appliance-build обязателен)"
}
if (-not (Test-Path $GuiExePath)) {
    throw "pogrebok-gui.exe не найден: $GuiExePath (артефакт gui-build обязателен)"
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
$layout = "dist/portable/Pogrebok"
if (Test-Path $layout) { Remove-Item -Recurse -Force $layout }
New-Item -ItemType Directory -Force -Path "$layout/appliance" | Out-Null

Copy-Item "target/release/pogrebok.exe"        $layout
Copy-Item "target/release/pogrebok-daemon.exe" $layout
Copy-Item "target/release/pogrebok-svc.exe"    $layout
Copy-Item $RootfsPath "$layout/appliance/pogrebok-rootfs.tar.gz"
Copy-Item $GuiExePath "$layout/pogrebok-gui.exe"

# INSTALL.txt: русский, portable = env-only (спека §11).
@(
    "Погребок $version — portable-вариант (без прав администратора)."
    ""
    "Что здесь лежит:"
    "  pogrebok-gui.exe    — приложение «Погребок» (бутылки, демон, диагностика)"
    "  pogrebok-daemon.exe — демон (запускается из приложения кнопкой «Запустить демона»)"
    "  pogrebok.exe        — служебный CLI (doctor / logs export / upgrade / routes cleanup)"
    "  pogrebok-svc.exe    — служба маршрутов для host-бутылок (НЕ обязательна)"
    "  appliance/          — rootfs аплаенса WSL (используется демоном отсюда)"
    ""
    "Требования:"
    "  - WSL включён в системе ЗАРАНЕЕ (wsl --install --no-distribution + перезагрузка);"
    "    portable-вариант сам WSL не устанавливает."
    ""
    "Ограничения portable:"
    "  - Только env-бутылки (WireGuard/OpenVPN в изолированных окружениях WSL)."
    "  - Host-бутылки (фирменные VPN-клиенты) требуют службу маршрутов:"
    "    запустите от администратора  pogrebok-svc.exe --install"
    ""
    "Начало работы:"
    "  1) pogrebok-gui.exe   (приложение; при первом запуске — мастер-пароль)"
    "  2) кнопка «Запустить демона» в окне приложения"
    "  3) вкладка «Диагностика» → Doctor (проверка окружения)"
    ""
    "Диагностика: pogrebok doctor, pogrebok logs export."
) | Set-Content -Encoding UTF8 "$layout/INSTALL.txt"

# ZIP + SHA256.
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$zip = "$OutDir/Pogrebok-portable-$version-win-x64.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path "$layout/*" -DestinationPath $zip

$hash = (Get-FileHash $zip -Algorithm SHA256).Hash
"$hash  $(Split-Path $zip -Leaf)" | Set-Content -Encoding ascii "$zip.sha256"

Write-Host "Готово: $zip"
Write-Host "SHA256: $hash"
