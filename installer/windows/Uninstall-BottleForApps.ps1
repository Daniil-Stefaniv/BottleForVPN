$ErrorActionPreference = "Stop"

$installDirectory = Join-Path $env:LOCALAPPDATA "Programs\BottleForApps"
$startMenuDirectory = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\BottleForApps"
$desktopShortcut = Join-Path ([Environment]::GetFolderPath("Desktop")) "BottleForApps.lnk"
$serviceName = "BottleForAppsRouteHelper"

# Служба удаляется через один UAC-запрос (требует прав администратора).
$innerCommand = @"
sc.exe stop $serviceName | Out-Null
sc.exe delete $serviceName | Out-Null
"@
try {
    Start-Process powershell.exe -Verb RunAs -Wait -WindowStyle Hidden `
        -ArgumentList "-NoProfile", "-Command", $innerCommand | Out-Null
}
catch {
    Write-Warning "Не удалось удалить службу маршрутов: подтвердите запрос прав или выполните это вручную (sc.exe delete $serviceName)."
}

Remove-Item $desktopShortcut -Force -ErrorAction SilentlyContinue
Remove-Item $startMenuDirectory -Recurse -Force -ErrorAction SilentlyContinue

# Удаление выполняется после завершения текущего PowerShell, потому что скрипт находится в каталоге приложения.
$command = "Start-Sleep -Seconds 2; Remove-Item -LiteralPath '$($installDirectory.Replace("'", "''"))' -Recurse -Force"
Start-Process powershell.exe -WindowStyle Hidden -ArgumentList "-NoProfile", "-Command", $command
Write-Host "BottleForApps удалён. Профили и диагностические логи сохранены в LocalAppData."
